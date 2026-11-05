// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `chaos`: runs of the harness's mixes until the time or the run count is up, each on settings
//! drawn from the seed, each checked by the model's replay of its log and the checkpoint at its
//! end. Its lines follow `store_stress`'s: on stdout `NOTE`, `COVERAGE` and `COVERAGE-GAP`, and its
//! own `RUN` per run and `REPORT` summary; on stderr `VIOLATION` and `HANG`. A panic ends the
//! process, as the release build aborts on one: the `NOTE running` line before each run names the
//! run it ended.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use lore_revision::state::node::hooks::log::Effect;

use crate::args::Args;
use crate::harness::catalogue::call::Outcome;
use crate::harness::mixes;
use crate::harness::mixes::Mix;
use crate::harness::mixes::ROUNDS;
use crate::harness::mixes::Settings;
use crate::harness::mixes::Variant;

/// The exit codes: no violation; violations; a usage error; stopped at the first violation; a
/// hang. A panic aborts the process instead.
pub const CLEAN: u8 = 0;
pub const VIOLATIONS: u8 = 1;
pub const USAGE: u8 = 2;
pub const STOPPED: u8 = 99;
pub const HANG: u8 = 100;

/// A xorshift stream: the runs' settings, drawn from the seed, so a seed names a campaign's draws.
struct Draw(u64);

impl Draw {
    fn next(&mut self) -> u64 {
        mixes::xorshift(&mut self.0)
    }

    /// A draw from `0..span`.
    fn below(&mut self, span: u64) -> u64 {
        self.next() % span.max(1)
    }
}

/// The options `chaos` takes.
const OPTIONS: [&str; 11] = [
    "seconds",
    "runs",
    "mix",
    "variant",
    "seed",
    "run-seed",
    "workers",
    "every",
    "rounds",
    "hang-seconds",
    "stop-on-violation",
];

/// What `chaos` was asked for. Without `--seconds` or `--runs` it runs for a minute; with `--runs`
/// alone, until that many have run; with `--run-seed`, one run.
struct Options {
    seconds: Option<u64>,
    runs: Option<u64>,
    mixes: Vec<Mix>,
    /// The variants the runs draw from.
    variants: Vec<Variant>,
    seed: u64,
    /// The seed of every run in place of the campaign's draws: its operations' order and its
    /// threads' yields.
    run_seed: Option<u64>,
    workers: Option<usize>,
    every: Option<u64>,
    rounds: Option<usize>,
    hang: Duration,
    stop: bool,
}

fn options(args: &Args) -> Result<Options, String> {
    if let Some(name) = args.names().find(|name| !OPTIONS.contains(name)) {
        return Err(format!("chaos takes no option --{name}"));
    }
    let mixes = match args.get("mix").unwrap_or("all") {
        "all" => Mix::EVERY.to_vec(),
        names => names
            .split(',')
            .map(|name| Mix::named(name).ok_or_else(|| format!("no mix named '{name}'")))
            .collect::<Result<_, _>>()?,
    };
    let variants: Vec<Variant> = match args.get("variant").unwrap_or("all") {
        "all" => Variant::EVERY.to_vec(),
        names => names
            .split(',')
            .map(|name| Variant::named(name).ok_or_else(|| format!("no variant named '{name}'")))
            .collect::<Result<_, _>>()?,
    };
    let mixes: Vec<Mix> = (mixes.into_iter())
        .filter(|mix| variants.iter().any(|&variant| mix.runs(variant)))
        .collect();
    if mixes.is_empty() {
        return Err(
            "no mix named runs a variant named: fault runs on the unserved mixes alone".into(),
        );
    }
    let at_least_one = |name: &str| -> Result<Option<u64>, String> {
        let value = args.number(name)?;
        match value {
            Some(0) => Err(format!("--{name} takes a number above 0")),
            _ => Ok(value),
        }
    };
    let rounds = at_least_one("rounds")?;
    if rounds.is_some_and(|rounds| rounds > ROUNDS as u64) {
        return Err(format!("--rounds takes at most {ROUNDS}"));
    }
    let seed = match args.number("seed")? {
        Some(seed) => seed,
        None => SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(1, |since| since.as_nanos() as u64),
    };
    let run_seed = args.number("run-seed")?;
    let runs = at_least_one("runs")?.or(run_seed.map(|_| 1));
    let seconds = match (args.number("seconds")?, runs) {
        (None, None) => Some(60),
        (seconds, _) => seconds,
    };
    Ok(Options {
        seconds,
        runs,
        mixes,
        variants,
        seed,
        run_seed,
        workers: at_least_one("workers")?.map(|workers| workers as usize),
        every: at_least_one("every")?,
        rounds: rounds.map(|rounds| rounds as usize),
        hang: Duration::from_secs(at_least_one("hang-seconds")?.unwrap_or(300)),
        stop: args.flag("stop-on-violation"),
    })
}

/// What the runs of one mix covered: how many ran and how many passed, and the records the passing
/// ones made, by [`mixes::kinds`]' names, all of them and their holder's, task 0's.
#[derive(Default)]
struct Covered {
    runs: u64,
    passed: u64,
    records: BTreeMap<String, usize>,
    holder: BTreeMap<String, usize>,
}

/// What a campaign of runs covered.
#[derive(Default)]
struct Coverage {
    mixes: BTreeMap<&'static str, Covered>,
    /// Each variant's runs and passing runs.
    variants: BTreeMap<&'static str, (u64, u64)>,
    /// The calls the passing runs dropped part way, and those that failed on a failed read.
    dropped_calls: usize,
    failed_calls: usize,
    calls: BTreeMap<&'static str, usize>,
    arms: BTreeMap<String, usize>,
    yields: u64,
    contentions: u64,
    runs: u64,
    failed: u64,
}

impl Coverage {
    fn passed(&mut self, mix: Mix, variant: Variant, run: &mixes::Run) {
        let covered = self.mixes.entry(mix.name()).or_default();
        covered.passed += 1;
        self.variants.entry(variant.name()).or_default().1 += 1;
        for (kind, count) in mixes::kinds(run.on_state()) {
            *covered.records.entry(kind).or_default() += count;
        }
        if mix.holder().is_some() {
            let holder = run.on_state().filter(|record| record.task == Some(0));
            for (kind, count) in mixes::kinds(holder) {
                *covered.holder.entry(kind).or_default() += count;
            }
        }
        for call in &run.calls {
            *self.calls.entry(call.kind.name).or_default() += 1;
            match call.outcome {
                Outcome::Dropped => self.dropped_calls += 1,
                Outcome::Failed(_) => self.failed_calls += 1,
                _ => {}
            }
        }
        for record in &run.log {
            if let Effect::Arm(arm) = &record.effect {
                let arm = format!("{arm:?}");
                let name = arm.split([' ', '{', '(']).next().unwrap_or_default();
                *self.arms.entry(name.to_string()).or_default() += 1;
            }
        }
        self.yields += run.yields;
        self.contentions += run.contentions;
    }

    /// The coverage lines, and a gap for each record a mix that passed is for and none of its
    /// runs made: of its holder's task, for a mix whose records are its holder's.
    fn report(&self) {
        for (mix, covered) in &self.mixes {
            println!("COVERAGE mix {mix} {} {}", covered.runs, covered.passed);
            for (record, count) in &covered.records {
                println!("COVERAGE record {mix} {} {count}", record.replace(' ', "-"));
            }
            for (record, count) in &covered.holder {
                println!("COVERAGE holder {mix} {} {count}", record.replace(' ', "-"));
            }
        }
        for (variant, (runs, passed)) in &self.variants {
            println!("COVERAGE variant {variant} {runs} {passed}");
        }
        println!("COVERAGE outcome dropped {}", self.dropped_calls);
        println!("COVERAGE outcome failed {}", self.failed_calls);
        for (kind, count) in &self.calls {
            println!("COVERAGE call {} {count}", kind.replace(' ', "-"));
        }
        for (arm, count) in &self.arms {
            println!("COVERAGE arm {arm} {count}");
        }
        for mix in Mix::EVERY {
            let Some(covered) = self.mixes.get(mix.name()) else {
                continue;
            };
            if covered.passed == 0 {
                continue;
            }
            let (made_by, made) = if mix.makes_by_holder() {
                ("holder", &covered.holder)
            } else {
                ("record", &covered.records)
            };
            for record in mix.makes() {
                if !made.contains_key(*record) {
                    println!(
                        "COVERAGE-GAP {made_by} {} {}",
                        mix.name(),
                        record.replace(' ', "-")
                    );
                }
            }
        }
        println!(
            "REPORT runs {} failed {} yields {} contentions {}",
            self.runs, self.failed, self.yields, self.contentions
        );
    }
}

/// A thread that exits the process with [`HANG`] where no run ends for `hang` from `started` or
/// from the last run's end, which `ended` holds in seconds since `started`: a run whose replay or
/// checkpoint never ends would hold the harness forever. A task that never ends exits the same way
/// from its join, after a minute.
#[expect(
    clippy::exit,
    reason = "a hung run never returns, so only the process's end stops it"
)]
fn watchdog(started: Instant, hang: Duration) -> Arc<AtomicU64> {
    let ended = Arc::new(AtomicU64::new(0));
    let watched = ended.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let last = Duration::from_secs(watched.load(Ordering::Relaxed));
            if started.elapsed().saturating_sub(last) > hang {
                let _ = writeln!(
                    std::io::stderr(),
                    "HANG: no run ended for {} s",
                    hang.as_secs()
                );
                std::process::exit(i32::from(HANG));
            }
        }
    });
    ended
}

pub fn main(args: &Args) -> u8 {
    let options = match options(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            return USAGE;
        }
    };
    let started = Instant::now();
    let ended = watchdog(started, options.hang);
    let most_workers = std::thread::available_parallelism().map_or(8, usize::from) * 2;
    let mut draw = Draw(mixes::xorshift_state(options.seed));
    let mut coverage = Coverage::default();
    println!("NOTE seed {}", options.seed);
    loop {
        let runs_done = options.runs.is_some_and(|runs| coverage.runs >= runs);
        let time_up = options
            .seconds
            .is_some_and(|seconds| started.elapsed() >= Duration::from_secs(seconds));
        if runs_done || time_up {
            break;
        }
        let mix = options.mixes[(coverage.runs % options.mixes.len() as u64) as usize];
        let rounds = (options.rounds).unwrap_or_else(|| 1 + draw.below(ROUNDS as u64) as usize);
        let workers = (options.workers)
            .unwrap_or_else(|| 2 + draw.below(most_workers.saturating_sub(1) as u64) as usize);
        let every = options.every.unwrap_or_else(|| 1 << draw.below(4));
        let runnable = || (options.variants.iter().copied()).filter(|&variant| mix.runs(variant));
        let variant = runnable()
            .nth(draw.below(runnable().count() as u64) as usize)
            .expect("every mix kept runs a variant named");
        let seed = options.run_seed.unwrap_or_else(|| draw.next());
        let shown = if mix.repeats() {
            rounds.to_string()
        } else {
            "-".to_string()
        };
        let label = format!(
            "{} {} rounds {shown} workers {workers} every {every} seed {seed}",
            mix.name(),
            variant.name()
        );
        let ops = mixes::shuffled(mix.ops(rounds), seed);
        let settings = Settings {
            workers,
            every,
            seed,
            variant,
        };
        // Before the run, and flushed, so a run that aborts the process on a panic, or hangs, is
        // named.
        println!("NOTE running {label}");
        let _ = std::io::stdout().flush();
        let result = mixes::replayed_on_workers(mix, &ops, settings);
        coverage.runs += 1;
        coverage.mixes.entry(mix.name()).or_default().runs += 1;
        coverage.variants.entry(variant.name()).or_default().0 += 1;
        ended.store(started.elapsed().as_secs(), Ordering::Relaxed);
        match result {
            Ok(run) => {
                println!("RUN {label} ok");
                coverage.passed(mix, variant, &run);
            }
            Err(failure) => {
                coverage.failed += 1;
                println!("RUN {label} {}", failure.class);
                for detail in &failure.details {
                    eprintln!(
                        "VIOLATION {}-{}: {detail} ({label})",
                        mix.name(),
                        failure.class
                    );
                }
                if options.stop {
                    coverage.report();
                    return STOPPED;
                }
            }
        }
    }
    coverage.report();
    if coverage.failed > 0 {
        VIOLATIONS
    } else {
        CLEAN
    }
}

/// The mixes, one name a line, each followed by its operations with `--ops`.
pub fn list(args: &Args) {
    for mix in Mix::EVERY {
        println!("{}", mix.name());
        if args.flag("ops") {
            for op in mix.ops(ROUNDS) {
                println!("  {op:?}");
            }
        }
    }
}
