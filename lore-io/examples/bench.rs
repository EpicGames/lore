// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Micro-benchmark comparing today's file I/O dispatch shapes on identical workloads:
//! `blocking` (`spawn_blocking` around a positional syscall), `tokiofs` (`tokio::fs`), and
//! `loreio` (this crate). Where a completion backend exists it runs as a fourth engine —
//! `uring` on Linux, `iocp` on Windows — through the same driver API as `loreio`, so the pair
//! differ in backend and in nothing else.
//!
//! One process runs one engine. Sharing a process let the engines contaminate each other —
//! whichever ran second read a data file the first had evicted, and inherited its warmed thread
//! pools and allocator — which was worth 3x and looked exactly like an engine difference.
//!
//! Warm suite, all three engines, one child process each:
//! `cargo run --release -p lore-io --example bench -- warm`
//!
//! One engine only, which is what to run under `perf`:
//! `cargo run --release -p lore-io --example bench -- warm loreio`
//!
//! Cold suite (real device reads), one engine per process. `prepare-cold` checks that this
//! filesystem can be evicted, measures what the device gives, and sizes the phases from it, so the
//! data set is as large as a phase of about a second needs and no larger. `check-eviction` runs the
//! first of those on its own, for qualifying a filesystem before writing anything:
//! 1. `cargo run --release -p lore-io --example bench -- prepare-cold`
//! 2. Drop caches: `sudo sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches'`
//!    (required on ZFS, where the ARC ignores `posix_fadvise`; unnecessary elsewhere, including
//!    macOS, where the harness evicts each file through `msync(MS_INVALIDATE)`)
//! 3. `cargo run --release -p lore-io --example bench -- cold`
//!
//! A single round decides nothing — see the noise floor discussion in BENCHMARKS.md. The suite
//! modes run the protocol that does: N rounds with the engine order alternated, then a median,
//! a range and a position control. `cold-suite` additionally rotates which copy of the cold data
//! set each engine reads, because the copies are not equivalent and a fixed assignment biases an
//! engine for the whole run; give it a round count divisible by the engine count.
//! `cargo run --release -p lore-io --example bench -- warm-suite 6`
//! `cargo run --release -p lore-io --example bench -- cold-suite 6`
//!
//! Set `LORE_BENCH_DIR` to place benchmark files on a specific filesystem.

// Linking lore-base installs its `#[global_allocator]`, so the benchmark allocates through
// `LoreAllocator` — rpmalloc by default, `std::alloc::System` under `LORE_ALLOCATOR=system` — the
// same path a Lore process takes. This is load-bearing rather than incidental: reads allocate
// their buffer inside the I/O job specifically because the process allocator caches spans per
// thread, and on glibc malloc that premise does not hold.
#[allow(unused_extern_crates)]
extern crate lore_base;

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use lore_io::BackendKind;
use lore_io::IoDriver;
use lore_io::IoFile;
use lore_io::OpenOptions;

// Operation counts are sized so each warm phase runs for roughly a second rather than tens of
// milliseconds. A phase shorter than that measures the machine — frequency ramp from idle,
// cache state, whatever else is running — rather than the engines, and no number of repeats
// recovers a signal from it. The cost is a warm run of about ten seconds per engine.
const FILE_SIZE: u64 = 256 * 1024 * 1024;
const READ_LARGE_SIZE: usize = 64 * 1024;
const READ_LARGE_OPS: usize = 1_572_864;
const READ_LARGE_CONCURRENCY: usize = 64;
const READ_SMALL_SIZE: usize = 4 * 1024;
const READ_SMALL_OPS: usize = 6_291_456;
const READ_SMALL_CONCURRENCY: usize = 128;
const WRITE_SIZE: usize = 256 * 1024;
const WRITE_OPS: usize = 16_384;
const WRITE_CONCURRENCY: usize = 32;
const SMALL_FILE_SIZE: usize = 4 * 1024;
const SMALL_FILE_COUNT: usize = 65_536;
const SMALL_FILE_CONCURRENCY: usize = 64;

const COLD_DIR_NAME: &str = "lore-io-bench-cold";

/// Where the cold phase sizes chosen by `prepare-cold` are recorded, so that every engine in a
/// suite reads the same amount and a reader can see what the sizes were derived from.
const COLD_MANIFEST_NAME: &str = "manifest";

/// How long a cold phase should run.
///
/// A cold phase reads each block exactly once, because a second read of a block is a cache hit, so
/// its duration is the data set size divided by what the device gives for that access pattern.
/// Fixing the operation count instead — this was 4096 — fixes the duration only for one device
/// speed: 4096 permuted 64 KiB reads are about a second on a SATA SSD and 13 ms on a volume that
/// serves 300k of them a second, which is far below the floor [`report`] warns at. So the count is
/// derived per data set from a measured rate, and recorded in the manifest so every engine in the
/// suite still reads the same one.
const COLD_TARGET_SECS: f64 = 1.0;

/// Floor on the derived operation count, which is what a slow device gets. It is the count this
/// harness used unconditionally, so a device slow enough for it to bind behaves as before.
const COLD_MIN_OPS: usize = 4096;

/// Data set the sizing probe reads. Large enough that a fast device does not complete it during
/// its own start-up, small enough to write before the real data sets.
const COLD_PROBE_FILE_SIZE: u64 = 2 * 1024 * 1024 * 1024;

/// Passes the sizing probe takes, keeping the fastest. A cold read is bounded below by the device
/// and inflated by anything else the machine is doing, so the fastest pass is the least
/// contaminated estimate rather than an optimistic one.
const COLD_PROBE_PASSES: usize = 3;

/// Total disk the cold data sets may occupy, across every engine and both large-file phases.
///
/// Deriving from the device alone is unbounded: at 300k ops/s a one-second 64 KiB phase is 18 GiB
/// per engine, and the 4 KiB phase reads one block per [`COLD_READ_STRIDE`] region, so it is
/// larger still for the same operation count. The budget caps that, and `prepare-cold` reports the
/// phase duration the cap implies so a run that cannot reach the floor says so rather than being
/// discovered later in the results.
const COLD_BUDGET_VAR: &str = "LORE_BENCH_COLD_BUDGET_GIB";
const COLD_DEFAULT_BUDGET_GIB: u64 = 32;

/// Files a cold phase spreads its reads over. One, always.
///
/// `LORE_BENCH_READ_SPREAD` exists for the warm synthetic phases, where reading one file measures
/// a filesystem's per-inode serialization rather than the backend. A cold phase cannot use it: the
/// spread copies are made by copying the data file, so they are in cache the moment they exist and
/// the file the phase evicted is not the file it reads. The remapping that goes with a spread is
/// the second half of the problem — it folds every offset into one file's span, so a phase sized to
/// read each block once reads a fraction of the data set several times over, which is a cache hit
/// whatever the eviction did.
const COLD_SPREAD: usize = 1;

/// How far above the device probe a cold phase may measure before it is reported as not cold.
///
/// The probe is what plain threads got from an evicted file, and an engine can legitimately exceed
/// it — on Windows the probe is bounded by the platform's concurrent single-file behaviour rather
/// than by the device. Twice it is the point past which cache is the likelier explanation.
const COLD_SUSPECT_RATIO: f64 = 2.0;

/// Ratio of post-eviction to cached read rate above which eviction is reported as ineffective.
///
/// A working eviction leaves the second read reading from the device, which is far below cache
/// bandwidth on any device this benchmark is interesting on. The threshold is loose because the
/// comparison only has to separate "reads the device" from "reads memory".
const COLD_EVICTION_MAX_RATIO: f64 = 0.8;

/// Stride between cold 4 KiB reads. Reading each block from its own
/// 128 KiB region guarantees every op hits storage: ZFS caches whole
/// records (128 KiB default) in the ARC, so a second 4 KiB read inside an
/// already-touched record would be a cache hit, and the spacing also
/// defeats page-cache readahead on other filesystems.
const COLD_READ_STRIDE: u64 = 128 * 1024;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str);

    // The suite modes take a round count where the other modes take an engine, and they only
    // spawn children, so they are dispatched before the engine argument is parsed and before a
    // runtime this process will never use is built.
    match mode {
        Some("warm-suite") => return run_suite("warm", rounds_from(args.get(1))),
        Some("cold-suite") => return run_suite("cold", rounds_from(args.get(1))),
        _ => {}
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(blocking_pool_threads())
        .build()
        .expect("failed to build benchmark runtime");
    let engine = args.get(1).map(String::as_str);
    // The cold modes take the data set to read as a third argument, because which copy an engine
    // is given is worth more than the engines differ by. Omitted, an engine reads its own.
    let dataset = args.get(2).map(String::as_str);
    let selected = engine.map(select_engine);
    match (mode, selected) {
        (Some("warm"), None) => run_each_in_its_own_process("warm"),
        (Some("warm"), Some(Selection::Engine(engine))) => runtime.block_on(run_warm(engine)),
        (Some("read"), Some(Selection::Engine(engine))) => runtime.block_on(run_read_only(engine)),
        (Some("cold"), None) => run_each_in_its_own_process("cold"),
        (Some("cold"), Some(Selection::Engine(engine))) => {
            runtime.block_on(run_cold(engine, dataset));
        }
        (Some("prepare-cold"), _) => runtime.block_on(prepare_cold()),
        (Some("cold-baseline"), _) => run_cold_baseline(),
        (Some("check-eviction"), _) => {
            if !runtime.block_on(run_check_eviction()) {
                std::process::exit(1);
            }
        }
        (_, Some(Selection::Unavailable(error))) => {
            println!(
                "skipping engine \"{}\": {error}",
                engine.unwrap_or_default()
            );
        }
        (_, Some(Selection::Unknown)) => {
            eprintln!(
                "unknown engine \"{}\" (engines: {})",
                engine.unwrap_or_default(),
                ENGINE_TAGS.join(", ")
            );
            std::process::exit(2);
        }
        _ => {
            eprintln!(
                "usage: bench <warm|read|cold> [{}]\n       bench <warm-suite|cold-suite> [rounds]\n       bench prepare-cold\n       bench cold-baseline\n       bench check-eviction",
                ENGINE_TAGS.join("|")
            );
            std::process::exit(2);
        }
    }
}

/// Runs `mode` once per engine, each in a fresh process.
///
/// Process isolation is the point: page cache, thread pools and allocator state all carry between
/// engines otherwise, and the engine that runs second inherits whatever the first left behind.
/// A sync and a settle between children keeps writeback from one out of the next.
fn run_each_in_its_own_process(mode: &str) {
    let exe = std::env::current_exe().expect("current executable");
    for tag in ENGINE_TAGS {
        run_engine_in_child(&exe, mode, tag, "", "");
    }
}

/// Runs one engine in a fresh process, echoing its output as it arrives, and returns the lines.
///
/// The sync and the settle belong here rather than at the call site: every path that starts an
/// engine child owes the next one a quiet device. An empty `tag` runs a mode that takes no engine,
/// and an empty `dataset` leaves the engine reading its own.
fn run_engine_in_child(
    exe: &Path,
    mode: &str,
    tag: &str,
    dataset: &str,
    prefix: &str,
) -> Vec<String> {
    use std::io::BufRead;

    let mut command = std::process::Command::new(exe);
    command.arg(mode);
    if !tag.is_empty() {
        command.arg(tag);
    }
    if !dataset.is_empty() {
        command.arg(dataset);
    }
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("failed to run the engine child process");
    let stdout = child.stdout.take().expect("child process stdout");
    let mut lines = Vec::new();
    for line in std::io::BufReader::new(stdout).lines() {
        let line = line.expect("failed to read the engine child's output");
        println!("{prefix}{line}");
        lines.push(line);
    }
    let status = child
        .wait()
        .expect("failed to wait for the engine child process");
    assert!(status.success(), "engine {tag} exited with {status}");
    sync_filesystem();
    // Six seconds rather than three: ZFS commits transaction groups on a five-second timer, and a
    // measurement taken inside that window has read 5x low against the same binary settled.
    std::thread::sleep(std::time::Duration::from_secs(6));
    lines
}

// ---------------------------------------------------------------------------
// Suite modes: the noise-floor protocol, executable
// ---------------------------------------------------------------------------

/// Default rounds for a suite run. BENCHMARKS.md asks for at least six: absolute ops/s has moved
/// 38% between batches an hour apart, so fewer rounds measures the sitting rather than the engine.
const DEFAULT_SUITE_ROUNDS: usize = 6;

fn rounds_from(argument: Option<&String>) -> usize {
    let rounds = argument.map_or(DEFAULT_SUITE_ROUNDS, |value| {
        value
            .parse()
            .expect("rounds must be a positive whole number")
    });
    assert!(rounds > 0, "rounds must be at least 1");
    rounds
}

/// One phase result from one engine in one round.
struct Sample {
    workload: String,
    tag: &'static str,
    forward: bool,
    ops_per_sec: f64,
}

/// Runs `mode` for `rounds` rounds with the engine order alternated, then reports medians.
///
/// This is BENCHMARKS.md's noise-floor protocol rather than a convenience wrapper. A single round
/// cannot distinguish an engine difference from where the round sat, so the summary reports three
/// things: the median, the round-to-round range that says whether a median is readable at all, and
/// a position control. Odd rounds run the engines in the order [`ENGINE_TAGS`] lists and even
/// rounds reverse it, so an effect that follows position shows up as a fwd/rev ratio away from
/// 1.00x instead of masquerading as an engine result — which is exactly the trap that invalidated
/// this file's first set of numbers.
///
/// Every engine still runs in its own process; no process ever hosts two.
fn run_suite(mode: &str, rounds: usize) {
    let exe = std::env::current_exe().expect("current executable");
    let mut samples: Vec<Sample> = Vec::new();

    // A discarded round first. The first process of a sitting pays page-cache and allocator
    // warm-up that no later one does, and on ZFS it also runs against whatever the previous
    // workload left in the ARC.
    println!("=== warm-up (discarded) ===");
    for tag in ENGINE_TAGS {
        run_engine_in_child(&exe, mode, tag, "", "[warm-up] ");
    }

    for round in 1..=rounds {
        let forward = round % 2 == 1;
        let order = if forward { "fwd" } else { "rev" };
        let mut tags = ENGINE_TAGS;
        if !forward {
            tags.reverse();
        }
        println!("=== round {round}/{rounds} ({order}) ===");
        for tag in tags {
            // Rotating by the engine's own index, not by its position in this round's order, keeps
            // the assignment a bijection: within a round the three engines still read three
            // different copies, so no engine warms the file the next one reads and the single
            // cache drop before the suite still serves all three.
            let dataset = if mode == "cold" {
                let engine_index = ENGINE_TAGS
                    .iter()
                    .position(|&name| name == tag)
                    .expect("every tag is an engine");
                ENGINE_TAGS[(engine_index + round - 1) % ENGINE_TAGS.len()]
            } else {
                ""
            };
            let prefix = format!("[r{round}-{order}] ");
            for line in run_engine_in_child(&exe, mode, tag, dataset, &prefix) {
                if let Some(sample) = parse_sample(&line, tag, forward) {
                    samples.push(sample);
                }
            }
        }
    }

    // The baseline runs last and in its own child, on the same data-set generation the rounds just
    // read. Running it in the same sitting is the point: what it measures is a property of these
    // files now, and a rate carried over from an earlier sitting has measured 1.77× off.
    let baselines = if mode == "cold" {
        if !rounds.is_multiple_of(ENGINE_TAGS.len()) {
            println!(
                "note: {rounds} rounds does not divide by {} engines, so the data-set rotation is",
                ENGINE_TAGS.len()
            );
            println!("      unbalanced and some engine read the fast copy more often than another");
        }
        println!("=== baseline ===");
        let mut rates: Vec<(String, String, f64)> = Vec::new();
        for line in run_engine_in_child(&exe, "cold-baseline", "", "", "[baseline] ") {
            if let Some((tag, workload, ops_per_sec)) = parse_baseline(&line) {
                rates.push((tag, workload, ops_per_sec));
            }
        }
        rates
    } else {
        Vec::new()
    };

    print_suite_summary(&samples, rounds);
    print_baseline_summary(&samples, &baselines);
}

/// Recovers a rate from a [`report_baseline`] line, which names the data set rather than an engine.
fn parse_baseline(line: &str) -> Option<(String, String, f64)> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() != 7 {
        return None;
    }
    let tag = fields[0].strip_prefix("baseline:")?;
    fields[2].parse::<u64>().ok()?;
    let ops_per_sec = fields[5].parse::<f64>().ok()?;
    Some((tag.to_string(), fields[1].to_string(), ops_per_sec))
}

/// Each engine's median over what plain threads get from the same data sets.
///
/// The rotation makes every engine read every copy, so the denominator is the mean across the
/// copies and is the same for all three — it is not correcting a per-engine bias, because there is
/// no longer one to correct. What it reports is distance to the hardware: a column near 1.00 is an
/// engine getting everything the device has, and a low one is an engine that is the bottleneck.
///
/// The spread column is the size of the confound the rotation cancels. When it is near 1.00 the
/// copies agreed and the medians above could have been read directly; on APFS it has reached 1.78×.
fn print_baseline_summary(samples: &[Sample], baselines: &[(String, String, f64)]) {
    if baselines.is_empty() {
        return;
    }

    let mut workloads: Vec<&str> = Vec::new();
    for (_, workload, _) in baselines {
        if !workloads.contains(&workload.as_str()) {
            workloads.push(workload);
        }
    }

    println!();
    println!("engine over what plain threads get from the same data sets, rotation cancelling");
    println!("which copy each engine read; spread is max/min across the copies");
    print!("{:<28}", "workload");
    for tag in ENGINE_TAGS {
        print!(" {tag:>10}");
    }
    println!(" {:>9}", "spread");
    for workload in &workloads {
        let rates: Vec<f64> = baselines
            .iter()
            .filter(|(_, phase, _)| phase == workload)
            .map(|(_, _, rate)| *rate)
            .collect();
        let mean = rates.iter().sum::<f64>() / rates.len() as f64;
        print!("{workload:<28}");
        for tag in ENGINE_TAGS {
            let engine = median_of(samples, workload, tag, None);
            if mean > 0.0 {
                print!(" {:>10.2}", engine / mean);
            } else {
                print!(" {:>10}", "-");
            }
        }
        let low = rates.iter().copied().fold(f64::INFINITY, f64::min);
        let high = rates.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        if low > 0.0 {
            println!(" {:>8.2}x", high / low);
        } else {
            println!(" {:>9}", "-");
        }
    }
}

/// Recovers a phase result from a [`report`] line, ignoring headers and `pool_stats` lines.
fn parse_sample(line: &str, tag: &'static str, forward: bool) -> Option<Sample> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() != 7 {
        return None;
    }
    // The header has the same field count, so both numeric columns must parse.
    fields[2].parse::<u64>().ok()?;
    let ops_per_sec = fields[5].parse::<f64>().ok()?;
    Some(Sample {
        workload: fields[1].to_string(),
        tag,
        forward,
        ops_per_sec,
    })
}

fn samples_for(samples: &[Sample], workload: &str, tag: &str, order: Option<bool>) -> Vec<f64> {
    samples
        .iter()
        .filter(|sample| {
            sample.workload == workload
                && sample.tag == tag
                && order.is_none_or(|forward| forward == sample.forward)
        })
        .map(|sample| sample.ops_per_sec)
        .collect::<Vec<_>>()
}

fn median_of(samples: &[Sample], workload: &str, tag: &str, order: Option<bool>) -> f64 {
    let mut values = samples_for(samples, workload, tag, order);
    if values.is_empty() {
        return f64::NAN;
    }
    values.sort_by(|left, right| left.partial_cmp(right).expect("ops/s is never NaN"));
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        values[middle]
    } else {
        (values[middle - 1] + values[middle]) / 2.0
    }
}

fn range_of(samples: &[Sample], workload: &str, tag: &str) -> String {
    let values = samples_for(samples, workload, tag, None);
    let low = values.iter().copied().fold(f64::INFINITY, f64::min);
    let high = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    format!("{low:.0}-{high:.0}")
}

fn print_suite_summary(samples: &[Sample], rounds: usize) {
    let mut workloads: Vec<&str> = Vec::new();
    for sample in samples {
        if !workloads.contains(&sample.workload.as_str()) {
            workloads.push(&sample.workload);
        }
    }

    println!();
    println!("median of {rounds} rounds, one process per engine, engine order alternated");
    print!("{:<28}", "workload");
    for tag in ENGINE_TAGS {
        print!(" {tag:>10}");
    }
    println!();
    for workload in &workloads {
        print!("{workload:<28}");
        for tag in ENGINE_TAGS {
            print!(" {:>10.0}", median_of(samples, workload, tag, None));
        }
        println!();
    }

    println!();
    println!("ratios against the blocking baseline");
    print!("{:<28}", "workload");
    for tag in ENGINE_TAGS.iter().filter(|tag| **tag != "blocking") {
        print!(" {tag:>10}");
    }
    println!();
    for workload in &workloads {
        let blocking = median_of(samples, workload, "blocking", None);
        print!("{workload:<28}");
        for tag in ENGINE_TAGS.iter().filter(|tag| **tag != "blocking") {
            print!(
                " {:>9.2}x",
                median_of(samples, workload, tag, None) / blocking
            );
        }
        println!();
    }

    println!();
    println!("round-to-round range, which is what says whether a difference is readable");
    print!("{:<28}", "workload");
    for tag in ENGINE_TAGS {
        print!(" {tag:>19}");
    }
    println!();
    for workload in &workloads {
        print!("{workload:<28}");
        for tag in ENGINE_TAGS {
            print!(" {:>19}", range_of(samples, workload, tag));
        }
        println!();
    }

    if rounds > 1 {
        println!();
        println!("position control: median reverse-order / median forward-order");
        println!("a ratio far from 1.00x means engine order is leaking into the result");
        print!("{:<28}", "workload");
        for tag in ENGINE_TAGS {
            print!(" {tag:>10}");
        }
        println!();
        for workload in &workloads {
            print!("{workload:<28}");
            for tag in ENGINE_TAGS {
                let forward = median_of(samples, workload, tag, Some(true));
                let reverse = median_of(samples, workload, tag, Some(false));
                print!(" {:>9.2}x", reverse / forward);
            }
            println!();
        }
    }
}

/// The `blocking` engine's pool size: what `lore-base` builds, unless `LORE_BENCH_BLOCKING_THREADS`
/// overrides it.
///
/// The knob is the baseline's counterpart to `LORE_IO_POOL_THREADS`, and it exists because a cap
/// sweep against `loreio` alone cannot tell an engine result from a host one. Both engines sit on
/// the same host curve — reducing this pool reproduced `lore-io`'s 2.16× at cap 8 on Host B, which
/// is what established that the cap was not the variable — and without a knob that control means
/// patching this file.
fn blocking_pool_threads() -> usize {
    let cores = std::thread::available_parallelism().map_or(2, |count| count.get());
    let formula = std::cmp::min(2 * (cores + 1), 128);
    let Ok(value) = std::env::var("LORE_BENCH_BLOCKING_THREADS") else {
        return formula;
    };
    match value.trim().parse::<usize>() {
        Ok(threads) if threads > 0 => threads,
        _ => {
            eprintln!(
                "bench: unusable LORE_BENCH_BLOCKING_THREADS \"{value}\"; using {formula} instead"
            );
            formula
        }
    }
}

fn bench_root() -> PathBuf {
    std::env::var_os("LORE_BENCH_DIR").map_or_else(std::env::temp_dir, PathBuf::from)
}

/// How many files the read phases spread their reads over, from `LORE_BENCH_READ_SPREAD`.
///
/// The default is the larger read phase's concurrency, so that every in-flight read has a file of
/// its own. One file is the shape to avoid: `io_uring` serializes buffered work on a regular file
/// by inode, so a single-file phase measures that serialization and nothing else. It is also not
/// the storage layer's shape, which scatters addresses over 256 pack-file groups whose files roll
/// at 3 GiB — thousands of them on a server.
fn read_spread() -> usize {
    std::env::var("LORE_BENCH_READ_SPREAD")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|count| *count > 0)
        .unwrap_or(READ_LARGE_CONCURRENCY)
}

/// How many tasks drive a read phase, from `LORE_BENCH_DRIVERS`.
///
/// One task means one thread issues every operation, because `buffer_unordered` polls on whoever
/// drives it. That is invisible to an engine that offloads to a pool and decisive for one that
/// completes on the submitting thread: the same phase measured 206k with one driver and 1.03M with
/// sixteen. Lore drives its I/O from many tasks at once, so one is the unrepresentative case.
fn read_drivers() -> usize {
    std::env::var("LORE_BENCH_DRIVERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|count| *count > 0)
        .unwrap_or(16)
}

fn alloc_outside() -> bool {
    static OUTSIDE: OnceLock<bool> = OnceLock::new();
    *OUTSIDE.get_or_init(|| std::env::var_os("LORE_BENCH_ALLOC_OUTSIDE").is_some())
}

// ---------------------------------------------------------------------------
// Warm suite
// ---------------------------------------------------------------------------

async fn run_warm(engine: Engine) {
    let dir = TempDir::new();
    let driver = IoDriver::new(BackendKind::Psync).expect("psync backend");
    let data_path = dir.path.join(format!("data-{}", engine.tag()));

    // Written immediately before it is read, and by this process alone, so the read phases start
    // from a cache state this engine produced rather than one another engine left behind.
    write_random_file(&driver, &data_path, FILE_SIZE).await;

    print_header();
    read_phases(&engine, &data_path).await;
    bench_writes(&engine, &dir.path).await;
    bench_small_files(&engine, &dir.path).await;
    bench_repository_shaped(&engine, &dir.path).await;
}

/// The read phases alone, for profiling: `perf` then covers reads and the data-file write rather
/// than the whole suite.
async fn run_read_only(engine: Engine) {
    let dir = TempDir::new();
    let driver = IoDriver::new(BackendKind::Psync).expect("psync backend");
    let data_path = dir.path.join(format!("data-{}", engine.tag()));
    write_random_file(&driver, &data_path, FILE_SIZE).await;

    print_header();
    read_phases(&engine, &data_path).await;
}

async fn read_phases(engine: &Engine, data_path: &Path) {
    let offsets = random_offsets(READ_LARGE_OPS, FILE_SIZE, READ_LARGE_SIZE);
    read_phase(
        engine,
        data_path,
        "read-64KiB-warm-c64",
        READ_LARGE_SIZE,
        offsets,
        READ_LARGE_CONCURRENCY,
        read_spread(),
    )
    .await;
    let offsets = random_offsets(READ_SMALL_OPS, FILE_SIZE, READ_SMALL_SIZE);
    read_phase(
        engine,
        data_path,
        "read-4KiB-warm-c128",
        READ_SMALL_SIZE,
        offsets,
        READ_SMALL_CONCURRENCY,
        read_spread(),
    )
    .await;
}

async fn bench_writes(engine: &Engine, dir: &Path) {
    let path = dir.join(format!("write-{}", engine.tag()));
    let started = Instant::now();
    match engine {
        Engine::BlockingPool => {
            let file = Arc::new(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&path)
                    .expect("failed to create write file"),
            );
            stream::iter(0..WRITE_OPS)
                .map(|index| {
                    let file = Arc::clone(&file);
                    async move {
                        blocking_write_all_at(file, WRITE_SIZE, (index * WRITE_SIZE) as u64).await;
                    }
                })
                .buffer_unordered(WRITE_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
            let file = Arc::clone(&file);
            blocking_sync_data(file).await;
        }
        Engine::TokioFs => {
            stream::iter(0..WRITE_OPS)
                .map(|index| {
                    let path = path.clone();
                    async move {
                        tokio_fs_write_all_at(path, WRITE_SIZE, (index * WRITE_SIZE) as u64).await;
                    }
                })
                .buffer_unordered(WRITE_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
            // Opened for writing, not just reading: `FlushFileBuffers` needs write access on the
            // handle, so a read-only one fails with `Access is denied` on Windows. `fdatasync` on
            // a read-only descriptor succeeds on Linux, which is why this went unnoticed there.
            tokio::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .await
                .expect("open for sync")
                .sync_data()
                .await
                .expect("sync failed");
        }
        Engine::LoreIo(driver, _) => {
            let file = driver
                .open(
                    &path,
                    &OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(true),
                )
                .await
                .expect("failed to create write file");
            stream::iter(0..WRITE_OPS)
                .map(|index| {
                    let file = file.clone();
                    async move {
                        file.write_all_at(vec![0u8; WRITE_SIZE], (index * WRITE_SIZE) as u64)
                            .await
                            .expect("write failed");
                    }
                })
                .buffer_unordered(WRITE_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
            file.sync_data().await.expect("sync failed");
        }
    }
    report(
        engine,
        "write-256KiB-seq-c32+sync",
        WRITE_OPS,
        WRITE_OPS * WRITE_SIZE,
        started,
    );
    let _ = std::fs::remove_file(&path);
}

async fn bench_small_files(engine: &Engine, dir: &Path) {
    let subdir = dir.join(format!("small-{}", engine.tag()));
    std::fs::create_dir_all(&subdir).expect("failed to create small-file dir");

    let started = Instant::now();
    small_files_create_phase(engine, &subdir).await;
    small_files_read_phase(engine, &subdir).await;
    report(
        engine,
        "small-files-4KiB-wr+rd-c64",
        SMALL_FILE_COUNT * 2,
        SMALL_FILE_COUNT * SMALL_FILE_SIZE * 2,
        started,
    );
    let _ = std::fs::remove_dir_all(&subdir);
}

// ---------------------------------------------------------------------------
// Repository-shaped phases: a commit's reads and a sync's writes
// ---------------------------------------------------------------------------

/// Files in the mixed set. Enough to make the size distribution show, few enough that the set and
/// its written twin both fit a modest test filesystem.
const MIXED_FILE_COUNT: usize = 2048;

/// How many times over the set's bytes each phase passes. The set is sized to fit a modest test
/// filesystem, which makes one pass far too short to time — the fastest engine crossed it in
/// 0.12 s — so the passes rather than the set provide the duration.
const MIXED_PASSES: usize = 32;

/// Chunk sizes are normal, clamped to this range. A fragment store reads and writes whole
/// fragments, and content-defined chunking gives them a spread rather than one size.
const MIXED_CHUNK_MIN: usize = 32 * 1024;
const MIXED_CHUNK_MAX: usize = 256 * 1024;

/// Files at or below this size are handled whole, as the storage layer handles a small object:
/// one dispatch, no offsets.
const MIXED_WHOLE_FILE_LIMIT: u64 = MIXED_CHUNK_MIN as u64;

/// Driver tasks for the repository-shaped phases: one per core at least. A commit walks the tree
/// from every worker at once, so a phase driven by fewer measures the driver.
fn mixed_workers() -> usize {
    std::thread::available_parallelism()
        .map_or(2, |count| count.get())
        .max(16)
}

/// In-flight operations per driver task.
const MIXED_PER_WORKER: usize = 4;

/// One operation against the mixed set.
struct MixedOp {
    file: usize,
    offset: u64,
    len: usize,
    /// Handled as a whole file rather than at an offset.
    whole: bool,
}

/// A normally-distributed chunk size in `[MIXED_CHUNK_MIN, MIXED_CHUNK_MAX]`.
///
/// Box-Muller off the same generator the rest of the harness uses, centred on the midpoint with
/// the range at six standard deviations, then clamped. Clamping biases the tails slightly toward
/// the bounds, which matters less than the sizes being reproducible.
fn normal_chunk(rng: &mut XorShift) -> usize {
    let unit = |rng: &mut XorShift| (rng.next() >> 11) as f64 / (1u64 << 53) as f64;
    let first = unit(rng).max(f64::MIN_POSITIVE);
    let second = unit(rng);
    let normal = (-2.0 * first.ln()).sqrt() * (std::f64::consts::TAU * second).cos();
    let mean = (MIXED_CHUNK_MIN + MIXED_CHUNK_MAX) as f64 / 2.0;
    let deviation = (MIXED_CHUNK_MAX - MIXED_CHUNK_MIN) as f64 / 6.0;
    (mean + normal * deviation).clamp(MIXED_CHUNK_MIN as f64, MIXED_CHUNK_MAX as f64) as usize
}

/// File sizes for the mixed set: mostly small objects, most of the bytes in a few large ones.
///
/// The shape a repository has rather than a measured distribution from one — source trees differ
/// too much for a single sample to be more honest than a stated assumption.
fn mixed_file_sizes() -> Vec<u64> {
    let mut rng = XorShift::new(0x5eed_1a7e);
    (0..MIXED_FILE_COUNT)
        .map(|_| {
            match rng.next() % 100 {
                // Below a chunk, and always read and written whole. Log-uniform from 64 bytes, so
                // the tiny end is represented rather than swamped by the top of the range: a tree
                // has far more small files than a uniform draw over the same bounds would give.
                0..=59 => {
                    let decades = (32.0 * 1024.0 / 64.0f64).ln();
                    let unit = (rng.next() >> 11) as f64 / (1u64 << 53) as f64;
                    (64.0 * (unit * decades).exp()) as u64
                }
                60..=89 => 32 * 1024 + rng.next() % (1024 * 1024 - 32 * 1024),
                90..=98 => 1024 * 1024 + rng.next() % (7 * 1024 * 1024),
                _ => 8 * 1024 * 1024 + rng.next() % (24 * 1024 * 1024),
            }
        })
        .collect()
}

/// The operation list both repository-shaped phases execute, identical for every engine.
///
/// Large files are touched at scattered offsets in normally-distributed chunks, which is how a
/// fragment store reaches its contents; small files are touched whole. Building the list up front
/// keeps the generator's cost out of the timed region and makes every engine do the same work in
/// the same order.
fn mixed_ops(sizes: &[u64]) -> Vec<MixedOp> {
    let mut rng = XorShift::new(0x0b5_c0de);
    let mut ops = Vec::new();
    for _ in 0..MIXED_PASSES {
        for (file, &size) in sizes.iter().enumerate() {
            if size <= MIXED_WHOLE_FILE_LIMIT {
                ops.push(MixedOp {
                    file,
                    offset: 0,
                    len: size as usize,
                    whole: true,
                });
                continue;
            }
            let mut covered = 0u64;
            while covered < size {
                let len = normal_chunk(&mut rng).min(size as usize);
                let span = size - len as u64;
                let offset = if span == 0 { 0 } else { rng.next() % span };
                ops.push(MixedOp {
                    file,
                    offset,
                    len,
                    whole: false,
                });
                covered += len as u64;
            }
        }
    }
    // Interleave, so consecutive operations do not land on one file and the concurrency is spread
    // over the set the way a tree walk spreads it.
    let mut rng = XorShift::new(0x5caff01d);
    for index in (1..ops.len()).rev() {
        ops.swap(index, (rng.next() % (index as u64 + 1)) as usize);
    }
    ops
}

/// Per-engine handles onto the mixed set.
struct MixedHandles {
    paths: Vec<PathBuf>,
    files: MixedFiles,
}

enum MixedFiles {
    Blocking(Vec<Arc<File>>),
    TokioFs,
    LoreIo(IoDriver, Vec<IoFile>),
}

impl MixedHandles {
    async fn open(engine: &Engine, paths: Vec<PathBuf>, write: bool) -> MixedHandles {
        let files = match engine {
            Engine::BlockingPool => MixedFiles::Blocking(
                paths
                    .iter()
                    .map(|path| {
                        let mut options = std::fs::OpenOptions::new();
                        options.read(true).write(write);
                        Arc::new(options.open(path).expect("failed to open a mixed-set file"))
                    })
                    .collect(),
            ),
            Engine::TokioFs => MixedFiles::TokioFs,
            Engine::LoreIo(driver, _) => {
                let mut files = Vec::with_capacity(paths.len());
                for path in &paths {
                    files.push(
                        driver
                            .open(path, &OpenOptions::new().read(true).write(write))
                            .await
                            .expect("failed to open a mixed-set file"),
                    );
                }
                MixedFiles::LoreIo(driver.clone(), files)
            }
        };
        MixedHandles { paths, files }
    }

    async fn read(&self, op: &MixedOp) {
        match &self.files {
            MixedFiles::Blocking(files) => {
                if op.whole {
                    let path = self.paths[op.file].clone();
                    // The baseline engine is deliberately the tokio blocking pool: that is the
                    // path this crate replaces.
                    #[allow(clippy::disallowed_methods)]
                    tokio::task::spawn_blocking(move || {
                        std::fs::read(path).expect("whole-file read failed")
                    })
                    .await
                    .expect("blocking task failed");
                } else {
                    blocking_read_exact_at(Arc::clone(&files[op.file]), op.len, op.offset).await;
                }
            }
            MixedFiles::TokioFs => {
                if op.whole {
                    tokio::fs::read(&self.paths[op.file])
                        .await
                        .expect("whole-file read failed");
                } else {
                    tokio_fs_read_exact_at(self.paths[op.file].clone(), op.len, op.offset).await;
                }
            }
            MixedFiles::LoreIo(driver, files) => {
                if op.whole {
                    driver
                        .read_file_bytes(&self.paths[op.file])
                        .await
                        .expect("whole-file read failed");
                } else {
                    files[op.file]
                        .read_exact_at(op.len, op.offset)
                        .await
                        .expect("read failed");
                }
            }
        }
    }

    async fn write(&self, op: &MixedOp, payload: &Bytes) {
        let payload = payload.slice(0..op.len);
        match &self.files {
            MixedFiles::Blocking(files) => {
                if op.whole {
                    blocking_write_new_file(self.paths[op.file].clone(), payload).await;
                } else {
                    blocking_write_all_at(Arc::clone(&files[op.file]), op.len, op.offset).await;
                }
            }
            MixedFiles::TokioFs => {
                if op.whole {
                    tokio::fs::write(&self.paths[op.file], payload)
                        .await
                        .expect("whole-file write failed");
                } else {
                    tokio_fs_write_all_at(self.paths[op.file].clone(), op.len, op.offset).await;
                }
            }
            MixedFiles::LoreIo(driver, files) => {
                if op.whole {
                    driver
                        .write_file_bytes(&self.paths[op.file], payload, false)
                        .await
                        .expect("whole-file write failed");
                } else {
                    files[op.file]
                        .write_all_at(payload, op.offset)
                        .await
                        .expect("write failed");
                }
            }
        }
    }
}

/// Runs `ops` across [`mixed_workers`] driver tasks and reports the phase.
async fn run_mixed_phase(
    engine: &Engine,
    workload: &str,
    handles: Arc<MixedHandles>,
    ops: Arc<Vec<MixedOp>>,
    writing: bool,
) {
    let workers = mixed_workers();
    let bytes: usize = ops.iter().map(|op| op.len).sum();
    let count = ops.len();
    let payload = Bytes::from(vec![0x5au8; MIXED_CHUNK_MAX]);

    // The set was just written, and on both filesystems tested that writeback would otherwise land
    // inside the measurement. Flushing first cost the phase its widest ranges and a position
    // control of 0.81 on the baseline engine.
    sync_filesystem();
    std::thread::sleep(std::time::Duration::from_secs(2));

    let started = Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for worker in 0..workers {
        let handles = Arc::clone(&handles);
        let ops = Arc::clone(&ops);
        let payload = payload.clone();
        // The benchmark builds and owns its runtime, so there is no lore runtime to spawn onto and
        // no LORE_CONTEXT to propagate.
        #[allow(clippy::disallowed_methods)]
        set.spawn(async move {
            let indices: Vec<usize> = (worker..ops.len()).step_by(workers).collect();
            stream::iter(indices)
                .map(|index| {
                    let handles = Arc::clone(&handles);
                    let ops = Arc::clone(&ops);
                    let payload = payload.clone();
                    async move {
                        let op = &ops[index];
                        if writing {
                            handles.write(op, &payload).await;
                        } else {
                            handles.read(op).await;
                        }
                    }
                })
                .buffer_unordered(MIXED_PER_WORKER)
                .collect::<Vec<_>>()
                .await;
        });
    }
    while let Some(joined) = set.join_next().await {
        joined.expect("a mixed-set worker panicked");
    }

    report(engine, workload, count, bytes, started);
}

/// A commit's reads and a sync's writes: the same heterogeneous file set, reached from every core
/// at once, scattered through large files in normally-distributed chunks and whole for small ones.
async fn bench_repository_shaped(engine: &Engine, root: &Path) {
    let sizes = mixed_file_sizes();
    let ops = Arc::new(mixed_ops(&sizes));

    let read_dir = root.join("mixed-read");
    std::fs::create_dir_all(&read_dir).expect("failed to create the mixed-set dir");
    let read_paths = materialise_mixed_set(&read_dir, &sizes);
    let handles = Arc::new(MixedHandles::open(engine, read_paths, false).await);
    run_mixed_phase(
        engine,
        "commit-read-mixed-scattered",
        handles,
        Arc::clone(&ops),
        false,
    )
    .await;
    let _ = std::fs::remove_dir_all(&read_dir);

    let write_dir = root.join("mixed-write");
    std::fs::create_dir_all(&write_dir).expect("failed to create the mixed-set dir");
    let write_paths = materialise_mixed_set(&write_dir, &sizes);
    let handles = Arc::new(MixedHandles::open(engine, write_paths, true).await);
    run_mixed_phase(engine, "sync-write-mixed-scattered", handles, ops, true).await;
    let _ = std::fs::remove_dir_all(&write_dir);
}

/// Creates the mixed set at its sizes, outside any timed region.
fn materialise_mixed_set(dir: &Path, sizes: &[u64]) -> Vec<PathBuf> {
    let block = vec![0xc3u8; 1024 * 1024];
    sizes
        .iter()
        .enumerate()
        .map(|(index, &size)| {
            use std::io::Write;
            let path = dir.join(format!("object-{index}"));
            let mut file = File::create(&path).expect("failed to create a mixed-set file");
            let mut written = 0u64;
            while written < size {
                let take = std::cmp::min(block.len() as u64, size - written) as usize;
                file.write_all(&block[..take])
                    .expect("failed to write a mixed-set file");
                written += take as u64;
            }
            path
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Cold suite
// ---------------------------------------------------------------------------

/// The cold phase sizes one `prepare-cold` chose, and the measurements they came from.
///
/// Written beside the data sets and read by every cold mode. Recording it rather than recomputing
/// it is what keeps a suite comparable: the sizing probe would return a slightly different rate
/// each time it ran, and engines sized differently are not running the same phase.
struct ColdPlan {
    large_ops: usize,
    small_ops: usize,
    large_rate: f64,
    small_rate: f64,
}

impl ColdPlan {
    fn large_file_size(&self) -> u64 {
        self.large_ops as u64 * READ_LARGE_SIZE as u64
    }

    /// The 4 KiB phase reads one block per [`COLD_READ_STRIDE`] region, so its file is the stride
    /// times the operation count rather than the block size times it.
    fn small_file_size(&self) -> u64 {
        self.small_ops as u64 * COLD_READ_STRIDE
    }

    fn total_bytes(&self, engines: usize) -> u64 {
        (self.large_file_size() + self.small_file_size()) * engines as u64
    }

    fn write(&self, dir: &Path) {
        let text = format!(
            "large_ops={}\nsmall_ops={}\nlarge_rate={}\nsmall_rate={}\n",
            self.large_ops, self.small_ops, self.large_rate, self.small_rate
        );
        std::fs::write(dir.join(COLD_MANIFEST_NAME), text).expect("failed to write cold manifest");
    }

    fn read(dir: &Path) -> ColdPlan {
        let path = dir.join(COLD_MANIFEST_NAME);
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "cold manifest not readable at {} ({error}) — run prepare-cold first",
                path.display()
            )
        });
        let field = |name: &str| -> f64 {
            text.lines()
                .find_map(|line| line.strip_prefix(&format!("{name}="))?.parse().ok())
                .unwrap_or_else(|| panic!("cold manifest has no usable {name}"))
        };
        ColdPlan {
            large_ops: field("large_ops") as usize,
            small_ops: field("small_ops") as usize,
            large_rate: field("large_rate"),
            small_rate: field("small_rate"),
        }
    }

    fn report(&self, engines: usize) {
        println!(
            "cold plan: 64KiB phase {} ops ({:.2} GiB/engine, ~{:.2}s at {:.0} ops/s)",
            self.large_ops,
            self.large_file_size() as f64 / (1024.0 * 1024.0 * 1024.0),
            self.large_ops as f64 / self.large_rate,
            self.large_rate
        );
        println!(
            "cold plan: 4KiB phase  {} ops ({:.2} GiB/engine, ~{:.2}s at {:.0} ops/s)",
            self.small_ops,
            self.small_file_size() as f64 / (1024.0 * 1024.0 * 1024.0),
            self.small_ops as f64 / self.small_rate,
            self.small_rate
        );
        println!(
            "cold plan: {:.1} GiB total across {engines} engines",
            self.total_bytes(engines) as f64 / (1024.0 * 1024.0 * 1024.0)
        );
        for (phase, ops, rate) in [
            ("read-64KiB-cold-c64", self.large_ops, self.large_rate),
            ("read-4KiB-cold-rec-c128", self.small_ops, self.small_rate),
        ] {
            let seconds = ops as f64 / rate;
            if seconds < SHORT_PHASE_SECONDS {
                println!(
                    "cold plan: warning — {phase} projects {seconds:.3}s, under the \
                     {SHORT_PHASE_SECONDS:.2}s floor; raise {COLD_BUDGET_VAR} above the current \
                     {} GiB to lengthen it",
                    cold_budget_gib()
                );
            }
        }
    }
}

fn cold_budget_gib() -> u64 {
    std::env::var(COLD_BUDGET_VAR)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|gib| *gib > 0)
        .unwrap_or(COLD_DEFAULT_BUDGET_GIB)
}

/// Reports whether [`evict_from_page_cache`] actually evicts on this filesystem.
///
/// The comparison is the cold phase's own access pattern — permuted 64 KiB reads at the phase's
/// concurrency — before and after the eviction, and not a sequential pass. The pattern matters
/// more than it sounds: a single-threaded sequential read of a partly-evicted file on this
/// benchmark's own volume measured 0.61 of its cached rate and looked like a working eviction,
/// where the same file read the way a phase reads it was still returning cache at seven times what
/// the device gives. Concurrency is what exposes pages that a purge left on the standby list.
///
/// It has to run before the sizing probe, because a probe against a file that was not evicted
/// measures cache bandwidth and would size every cold phase from it.
///
/// Returns the ratio of post-eviction to cached throughput; below [`COLD_EVICTION_MAX_RATIO`] the
/// eviction is working.
async fn check_eviction(path: &Path) -> f64 {
    let slots = COLD_PROBE_FILE_SIZE / READ_LARGE_SIZE as u64;
    let offsets = Arc::new(permuted_offsets(
        slots,
        slots as usize,
        READ_LARGE_SIZE as u64,
    ));
    // The engine, not plain threads. A thread-per-request reader is itself the bottleneck on some
    // platforms — on Windows concurrent access to one file through threads plateaus around 90k
    // ops/s — and a reader that saturates below the difference it is meant to detect reports a
    // working eviction whatever the cache holds. The engine is what the phases use and is not
    // bounded there, so it can tell cache from device.
    let driver = IoDriver::new(BackendKind::Auto).expect("a driver for the eviction check");
    let rate = async || {
        engine_read_rate(
            &driver,
            path,
            READ_LARGE_SIZE,
            Arc::clone(&offsets),
            READ_LARGE_CONCURRENCY,
        )
        .await
    };

    rate().await;
    let cached = rate().await;
    evict_from_page_cache(path);
    let evicted = rate().await;
    let ratio = evicted / cached;
    println!(
        "eviction check: {cached:.0} ops/s cached, {evicted:.0} ops/s after eviction, \
         ratio {ratio:.2}"
    );
    if ratio > COLD_EVICTION_MAX_RATIO {
        println!(
            "eviction check: warning — a read after eviction should be far slower than a cached \
             one. Cold phases on this filesystem will measure cache."
        );
    }
    ratio
}

/// Measures what the device gives for one cold phase's access pattern, in operations per second.
///
/// Plain threads with a handle each, which is what [`cold_baseline`] measures against and the
/// ceiling an engine can approach but not beat. The file is evicted before every pass, so each
/// pass reads the device.
fn probe_cold_rate(path: &Path, size: usize, stride: u64, concurrency: usize) -> f64 {
    let slots = COLD_PROBE_FILE_SIZE / stride;
    let offsets = permuted_offsets(slots, slots as usize, stride);
    (0..COLD_PROBE_PASSES)
        .map(|_| cold_baseline(path, size, &offsets, concurrency))
        .fold(0.0, f64::max)
}

/// Chooses cold phase sizes from what the device gives, bounded by [`COLD_BUDGET_VAR`].
async fn plan_cold(dir: &Path, engines: usize) -> ColdPlan {
    let probe = dir.join("probe");
    let driver = IoDriver::new(BackendKind::Psync).expect("psync backend");
    write_random_file(&driver, &probe, COLD_PROBE_FILE_SIZE).await;
    sync_filesystem();

    check_eviction(&probe).await;
    let large_rate = probe_cold_rate(
        &probe,
        READ_LARGE_SIZE,
        READ_LARGE_SIZE as u64,
        READ_LARGE_CONCURRENCY,
    );
    let small_rate = probe_cold_rate(
        &probe,
        READ_SMALL_SIZE,
        COLD_READ_STRIDE,
        READ_SMALL_CONCURRENCY,
    );
    let _ = std::fs::remove_file(&probe);
    println!(
        "device probe: {large_rate:.0} ops/s at 64 KiB, {small_rate:.0} ops/s at 4 KiB strided"
    );

    // The budget is split in the ratio of the two phases' unbounded demands, so that neither is
    // starved when the cap binds and neither is padded when it does not.
    let budget = cold_budget_gib() * 1024 * 1024 * 1024;
    let want_large = (large_rate * COLD_TARGET_SECS) as u64 * READ_LARGE_SIZE as u64;
    let want_small = (small_rate * COLD_TARGET_SECS) as u64 * COLD_READ_STRIDE;
    let wanted = (want_large + want_small) * engines as u64;
    let scale = if wanted > budget {
        budget as f64 / wanted as f64
    } else {
        1.0
    };

    // `scale` already brings the total inside the budget, so the floor is the only other bound.
    // A device slow enough to hit the floor asks for less than the budget anyway.
    let ops = |rate: f64| ((rate * COLD_TARGET_SECS * scale) as usize).max(COLD_MIN_OPS);
    ColdPlan {
        large_ops: ops(large_rate),
        small_ops: ops(small_rate),
        large_rate,
        small_rate,
    }
}

/// Checks eviction on the benchmark filesystem without preparing anything.
///
/// A cold suite is only cold if [`evict_from_page_cache`] works on the filesystem under it, and
/// that is a property of the mount rather than of the platform — the mechanism is per-file and can
/// be defeated by another open handle, a filter driver, or a filesystem that ignores the request.
/// `prepare-cold` runs this before sizing anything, because a probe against a file that was not
/// evicted measures cache bandwidth and would size every cold phase from it. This mode runs the
/// same check on its own, so a filesystem can be qualified before the data sets are written.
/// Returns whether eviction works, which the caller reports as the process exit status.
async fn run_check_eviction() -> bool {
    let dir = bench_root().join(COLD_DIR_NAME);
    std::fs::create_dir_all(&dir).expect("failed to create cold bench dir");
    let path = dir.join("eviction-check");
    let driver = IoDriver::new(BackendKind::Psync).expect("psync backend");
    write_random_file(&driver, &path, COLD_PROBE_FILE_SIZE).await;
    sync_filesystem();
    let ratio = check_eviction(&path).await;
    let _ = std::fs::remove_file(&path);
    ratio <= COLD_EVICTION_MAX_RATIO
}

async fn prepare_cold() {
    let dir = bench_root().join(COLD_DIR_NAME);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("failed to create cold bench dir");
    let driver = IoDriver::new(BackendKind::Psync).expect("psync backend");

    let plan = plan_cold(&dir, ENGINE_TAGS.len()).await;
    plan.report(ENGINE_TAGS.len());
    plan.write(&dir);

    // The small-file trees go first so the data-file traffic written after
    // them ages them out of cache before the drop; data written last is
    // what survives a cache shrink.
    for tag in ENGINE_TAGS {
        let subdir = dir.join(format!("small-{tag}"));
        std::fs::create_dir_all(&subdir).expect("failed to create small-file dir");
        let payload = Bytes::from(vec![0xabu8; SMALL_FILE_SIZE]);
        stream::iter(0..SMALL_FILE_COUNT)
            .map(|index| {
                let path = subdir.join(format!("file-{index}"));
                let payload = payload.clone();
                let driver = driver.clone();
                async move {
                    driver
                        .write_file_bytes(path, payload, false)
                        .await
                        .expect("create failed");
                }
            })
            .buffer_unordered(SMALL_FILE_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
    }
    for tag in ENGINE_TAGS {
        write_random_file(
            &driver,
            &dir.join(format!("data64k-{tag}")),
            plan.large_file_size(),
        )
        .await;
        write_random_file(
            &driver,
            &dir.join(format!("data4k-{tag}")),
            plan.small_file_size(),
        )
        .await;
    }
    sync_filesystem();

    println!("cold data prepared in {}", dir.display());
    println!("now drop caches, then run the cold suite:");
    println!("  sudo sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches'");
    println!("  cargo run --release -p lore-io --example bench -- cold");
}

/// Runs the cold phases against the data set named by `dataset`, defaulting to the engine's own.
///
/// The two are separate because the copies are not equivalent: which one an engine is given is
/// worth up to 1.78× on this benchmark's own access pattern, for reasons that live in the drive
/// rather than in anything the filesystem exposes. A fixed assignment therefore biases an engine
/// for the whole run, which is why [`run_suite`] rotates it.
async fn run_cold(engine: Engine, dataset: Option<&str>) {
    let dir = bench_root().join(COLD_DIR_NAME);
    assert!(
        dir.is_dir(),
        "cold data not found in {} — run prepare-cold first",
        dir.display()
    );

    let plan = ColdPlan::read(&dir);
    let tag = dataset.unwrap_or_else(|| engine.tag());
    println!("cold suite (assumes caches were dropped after prepare-cold), data set {tag}");
    print_header();
    {
        let path = dir.join(format!("data64k-{tag}"));
        evict_from_page_cache(&path);
        let slots = plan.large_ops as u64;
        let offsets = permuted_offsets(slots, slots as usize, READ_LARGE_SIZE as u64);
        let rate = read_phase(
            &engine,
            &path,
            "read-64KiB-cold-c64",
            READ_LARGE_SIZE,
            offsets,
            READ_LARGE_CONCURRENCY,
            COLD_SPREAD,
        )
        .await;
        check_phase_was_cold(rate, plan.large_rate);

        let path = dir.join(format!("data4k-{tag}"));
        evict_from_page_cache(&path);
        let slots = plan.small_ops as u64;
        let offsets = permuted_offsets(slots, slots as usize, COLD_READ_STRIDE);
        let rate = read_phase(
            &engine,
            &path,
            "read-4KiB-cold-rec-c128",
            READ_SMALL_SIZE,
            offsets,
            READ_SMALL_CONCURRENCY,
            COLD_SPREAD,
        )
        .await;
        check_phase_was_cold(rate, plan.small_rate);

        let subdir = dir.join(format!("small-{tag}"));
        for index in 0..SMALL_FILE_COUNT {
            evict_from_page_cache(&subdir.join(format!("file-{index}")));
        }
        let started = Instant::now();
        small_files_read_phase(&engine, &subdir).await;
        report(
            &engine,
            "small-files-4KiB-rd-cold-c64",
            SMALL_FILE_COUNT,
            SMALL_FILE_COUNT * SMALL_FILE_SIZE,
            started,
        );
    }
}

// ---------------------------------------------------------------------------
// Phases shared between suites
// ---------------------------------------------------------------------------

enum Engine {
    BlockingPool,
    TokioFs,
    /// A `lore-io` driver, carrying the tag that reselects this backend in a child process.
    LoreIo(IoDriver, &'static str),
}

impl Engine {
    fn name(&self) -> String {
        match self {
            Engine::BlockingPool => "blocking-pool".to_string(),
            Engine::TokioFs => "tokio-fs".to_string(),
            Engine::LoreIo(driver, _) => format!("lore-io({})", driver.backend_name()),
        }
    }

    fn tag(&self) -> &'static str {
        match self {
            Engine::BlockingPool => "blocking",
            Engine::TokioFs => "tokiofs",
            Engine::LoreIo(_, tag) => tag,
        }
    }
}

/// Every engine, in the order [`ENGINE_TAGS`] names them.
#[cfg(target_os = "linux")]
const ENGINE_TAGS: [&str; 4] = ["blocking", "tokiofs", "loreio", "uring"];
#[cfg(target_family = "windows")]
const ENGINE_TAGS: [&str; 4] = ["blocking", "tokiofs", "loreio", "iocp"];
#[cfg(not(any(target_os = "linux", target_family = "windows")))]
const ENGINE_TAGS: [&str; 3] = ["blocking", "tokiofs", "loreio"];

/// The outcome of naming an engine, which separates a machine that cannot run one from a tag that
/// names nothing: a kernel without `io_uring` should skip that engine, not fail the run.
enum Selection {
    Engine(Engine),
    Unavailable(std::io::Error),
    Unknown,
}

fn select_engine(tag: &str) -> Selection {
    match tag {
        "blocking" => Selection::Engine(Engine::BlockingPool),
        "tokiofs" => Selection::Engine(Engine::TokioFs),
        "loreio" => match IoDriver::new(BackendKind::Psync) {
            Ok(driver) => Selection::Engine(Engine::LoreIo(driver, "loreio")),
            Err(error) => Selection::Unavailable(error),
        },
        #[cfg(target_os = "linux")]
        "uring" => match IoDriver::new(BackendKind::Uring) {
            Ok(driver) => Selection::Engine(Engine::LoreIo(driver, "uring")),
            Err(error) => Selection::Unavailable(error),
        },
        #[cfg(target_family = "windows")]
        "iocp" => match IoDriver::new(BackendKind::Iocp) {
            Ok(driver) => Selection::Engine(Engine::LoreIo(driver, "iocp")),
            Err(error) => Selection::Unavailable(error),
        },
        _ => Selection::Unknown,
    }
}

/// What the device gives for one cold phase, issued from plain threads with no engine in the path.
///
/// Every engine reads its own copy of the cold data set, so that one engine's process cannot delete
/// or warm what another still needs. Those copies do not read back at the same rate: on APFS they
/// have measured up to 1.62× apart, written seconds apart by the same code with the same contents,
/// which is larger than any engine difference this benchmark reports. A fixed assignment of engine
/// to file is invisible to the suite's own controls, because rounds, alternation and the position
/// control all vary *when* an engine runs and this varies *what it reads*.
///
/// So each engine's cold result is divided by its own file's rate here. The ratio is what carries
/// meaning; the ops/s are a property of a file on a drive on a day.
///
/// Threads rather than concurrency: this issues the same permuted `pread`s at the phase's own
/// concurrency, one thread per in-flight request, which is the ceiling an engine dispatching the
/// same syscalls can reach and not a target it should beat.
///
/// **A handle per thread, not one shared between them.** A synchronous Windows handle is a
/// serialization point — the I/O manager runs one operation on it at a time however many threads
/// submit — so a shared handle turns this into a single-threaded measurement and the ratios taken
/// against it are meaningless. The engines under test do not have that problem, because the
/// handles they open are overlapped. Nothing on Unix is affected either way: `pread` on a shared
/// descriptor takes no such lock.
fn cold_baseline(path: &Path, size: usize, offsets: &[u64], concurrency: usize) -> f64 {
    evict_from_page_cache(path);
    permuted_read_rate(path, size, offsets, concurrency)
}

/// The permuted read of [`cold_baseline`] issued through the engine rather than through threads,
/// in operations per second. One shared handle, the concurrency the phase uses.
///
/// Driven by one task per in-flight request rather than by `buffer_unordered` on the calling
/// thread, for the reason the read phases are: a single driving task submits from a single worker,
/// which for a backend that completes on the submitting thread measures the driver instead of the
/// backend. Here that would make the reader the bottleneck, and a saturated reader reports the same
/// rate cached and evicted whatever the cache holds.
async fn engine_read_rate(
    driver: &IoDriver,
    path: &Path,
    size: usize,
    offsets: Arc<Vec<u64>>,
    concurrency: usize,
) -> f64 {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    let file = Arc::new(
        driver
            .open(path, &OpenOptions::new().read(true))
            .await
            .expect("failed to open the probe file"),
    );
    let next = Arc::new(AtomicUsize::new(0));
    let started = Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..concurrency {
        let file = Arc::clone(&file);
        let next = Arc::clone(&next);
        let offsets = Arc::clone(&offsets);
        // The benchmark drives its own tasks deliberately: routing them through the shared runtime
        // would put the harness's own dispatch in the measurement.
        #[allow(clippy::disallowed_methods)]
        set.spawn(async move {
            loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(&offset) = offsets.get(index) else {
                    break;
                };
                file.read_exact_at(size, offset).await.expect("read failed");
            }
        });
    }
    while let Some(joined) = set.join_next().await {
        joined.expect("an eviction-check task panicked");
    }
    offsets.len() as f64 / started.elapsed().as_secs_f64()
}

/// The permuted read of [`cold_baseline`] without the eviction, so that [`check_eviction`] can run
/// the same pattern against a cached file and against an evicted one.
fn permuted_read_rate(path: &Path, size: usize, offsets: &[u64], concurrency: usize) -> f64 {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    let next = AtomicUsize::new(0);
    let started = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..concurrency {
            scope.spawn(|| {
                let file = File::open(path).expect("failed to open data file");
                let mut buffer = vec![0u8; size];
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&offset) = offsets.get(index) else {
                        break;
                    };
                    read_exact_at_impl(&file, &mut buffer, offset).expect("read failed");
                }
            });
        }
    });
    offsets.len() as f64 / started.elapsed().as_secs_f64()
}

/// The whole-file counterpart of [`cold_baseline`], for the phase that opens every file it reads.
fn cold_baseline_small_files(subdir: &Path, count: usize, concurrency: usize) -> f64 {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    for index in 0..count {
        evict_from_page_cache(&subdir.join(format!("file-{index}")));
    }
    let next = AtomicUsize::new(0);
    let started = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..concurrency {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= count {
                        break;
                    }
                    std::fs::read(subdir.join(format!("file-{index}"))).expect("read failed");
                }
            });
        }
    });
    count as f64 / started.elapsed().as_secs_f64()
}

/// Runs [`cold_baseline`] over every engine's copy of the cold data set.
///
/// Output is one `baseline` line per file in the same column layout the phases use, so a suite can
/// parse both from one child's stdout.
fn run_cold_baseline() {
    let dir = bench_root().join(COLD_DIR_NAME);
    assert!(
        dir.is_dir(),
        "cold data not found in {} — run prepare-cold first",
        dir.display()
    );

    let plan = ColdPlan::read(&dir);
    println!("cold baseline: plain threads, one per in-flight request, no engine");
    print_header();
    for tag in ENGINE_TAGS {
        let path = dir.join(format!("data64k-{tag}"));
        let slots = plan.large_ops as u64;
        let offsets = permuted_offsets(slots, slots as usize, READ_LARGE_SIZE as u64);
        let ops = offsets.len();
        report_baseline(
            tag,
            "read-64KiB-cold-c64",
            ops,
            ops * READ_LARGE_SIZE,
            cold_baseline(&path, READ_LARGE_SIZE, &offsets, READ_LARGE_CONCURRENCY),
        );

        let path = dir.join(format!("data4k-{tag}"));
        let slots = plan.small_ops as u64;
        let offsets = permuted_offsets(slots, slots as usize, COLD_READ_STRIDE);
        let ops = offsets.len();
        report_baseline(
            tag,
            "read-4KiB-cold-rec-c128",
            ops,
            ops * READ_SMALL_SIZE,
            cold_baseline(&path, READ_SMALL_SIZE, &offsets, READ_SMALL_CONCURRENCY),
        );

        report_baseline(
            tag,
            "small-files-4KiB-rd-cold-c64",
            SMALL_FILE_COUNT,
            SMALL_FILE_COUNT * SMALL_FILE_SIZE,
            cold_baseline_small_files(
                &dir.join(format!("small-{tag}")),
                SMALL_FILE_COUNT,
                SMALL_FILE_CONCURRENCY,
            ),
        );
    }
}

/// The files a read phase reads, and the handles each engine needs onto them.
///
/// Every engine reads the same set, so the workload's shape is a property of the phase rather than
/// of the engine under test. Before this existed the spread applied to `lore-io` alone, which made
/// any run with it set a comparison between different workloads.
enum ReadHandles {
    Blocking(Vec<Arc<File>>),
    TokioFs(Vec<PathBuf>),
    LoreIo(Vec<IoFile>),
}

impl ReadHandles {
    async fn read(&self, slot: usize, size: usize, offset: u64) {
        match self {
            ReadHandles::Blocking(files) => {
                blocking_read_exact_at(Arc::clone(&files[slot]), size, offset).await;
            }
            ReadHandles::TokioFs(paths) => {
                tokio_fs_read_exact_at(paths[slot].clone(), size, offset).await;
            }
            ReadHandles::LoreIo(files) => {
                files[slot]
                    .read_exact_at(size, offset)
                    .await
                    .expect("read failed");
            }
        }
    }
}

/// The `spread` files a read phase reads, together holding `FILE_SIZE`.
///
/// Splitting rather than copying keeps the working set the same at every spread. Growing it with
/// the file count would cool the reads as the count rose, and every engine would converge on being
/// device-bound rather than on each other — which measured like parity once, convincingly.
fn spread_files(path: &Path, spread: usize) -> Vec<PathBuf> {
    if spread <= 1 {
        return vec![path.to_path_buf()];
    }
    let per_file = FILE_SIZE / spread as u64;
    (0..spread)
        .map(|index| {
            let part = path.with_extension(format!("spread{spread}-{index}"));
            if !part.exists() {
                let source = File::open(path).expect("failed to open the data file");
                let mut destination =
                    File::create(&part).expect("failed to create a data file part");
                std::io::copy(&mut source.take(per_file), &mut destination)
                    .expect("failed to write a data file part");
            }
            part
        })
        .collect()
}

async fn open_read_handles(engine: &Engine, paths: &[PathBuf]) -> ReadHandles {
    match engine {
        Engine::BlockingPool => ReadHandles::Blocking(
            paths
                .iter()
                .map(|path| Arc::new(File::open(path).expect("failed to open a data file")))
                .collect(),
        ),
        Engine::TokioFs => ReadHandles::TokioFs(paths.to_vec()),
        Engine::LoreIo(driver, _) => {
            let mut files = Vec::with_capacity(paths.len());
            for path in paths {
                files.push(
                    driver
                        .open(path, &OpenOptions::new().read(true))
                        .await
                        .expect("failed to open a data file"),
                );
            }
            ReadHandles::LoreIo(files)
        }
    }
}

async fn read_phase(
    engine: &Engine,
    path: &Path,
    workload: &str,
    size: usize,
    offsets: Vec<u64>,
    concurrency: usize,
    spread: usize,
) -> f64 {
    let drivers = read_drivers();
    let paths = spread_files(path, spread);
    let handles = Arc::new(open_read_handles(engine, &paths).await);
    let offsets: Vec<u64> = if spread > 1 {
        let span = FILE_SIZE / spread as u64 - size as u64;
        offsets.into_iter().map(|offset| offset % span).collect()
    } else {
        offsets
    };

    let ops = offsets.len();
    let started = Instant::now();
    if drivers > 1 {
        let per_driver = concurrency.div_ceil(drivers).max(1);
        let mut set = tokio::task::JoinSet::new();
        for driver in 0..drivers {
            let chunk: Vec<(usize, u64)> = offsets
                .iter()
                .enumerate()
                .skip(driver)
                .step_by(drivers)
                .map(|(index, offset)| (index, *offset))
                .collect();
            let handles = Arc::clone(&handles);
            // The benchmark builds and owns its runtime, so there is no lore runtime to spawn onto
            // and no LORE_CONTEXT to propagate.
            #[allow(clippy::disallowed_methods)]
            set.spawn(async move {
                stream::iter(chunk)
                    .map(|(index, offset)| {
                        let handles = Arc::clone(&handles);
                        async move { handles.read(index % spread, size, offset).await }
                    })
                    .buffer_unordered(per_driver)
                    .collect::<Vec<_>>()
                    .await;
            });
        }
        while let Some(joined) = set.join_next().await {
            joined.expect("a read driver panicked");
        }
    } else {
        stream::iter(offsets.into_iter().enumerate())
            .map(|(index, offset)| {
                let handles = Arc::clone(&handles);
                async move { handles.read(index % spread, size, offset).await }
            })
            .buffer_unordered(concurrency)
            .collect::<Vec<_>>()
            .await;
    }

    let rate = report(engine, workload, ops, ops * size, started);
    if let Engine::LoreIo(driver, _) = engine {
        let stats = lore_io::pool_stats();
        println!(
            "{:<18} {:<28} threads {}/{} peak {}  queue peak {}",
            "",
            "  pool",
            stats.threads,
            stats.max_threads,
            stats.threads_high_water,
            stats.queue_high_water
        );
        report_backend(driver);
    }
    rate
}

/// Warns when a phase labelled cold read faster than the device probe that sized it.
///
/// This is the check that catches an eviction the harness believed in and the filesystem did not
/// honour, and it is the most reliable one available: the probe rate in the manifest is what plain
/// threads got from an evicted file, and a phase reading several times that has to be reading
/// memory. It costs nothing, because both numbers already exist by the time a phase ends.
fn check_phase_was_cold(achieved: f64, probed: f64) {
    if achieved > probed * COLD_SUSPECT_RATIO {
        println!(
            "{:<18} {:<28} {achieved:.0} ops/s is {:.1}x the {probed:.0} ops/s the device \
             probe measured — this phase was not cold",
            "",
            "warning",
            achieved / probed
        );
        println!(
            "{:<18} {:<28} run `bench check-eviction` on this filesystem",
            "", ""
        );
    }
}

/// Prints the ring's batching factor and inline-completion share, which are what say whether the
/// `uring` backend is doing less work per operation than the pool or more.
#[cfg(target_os = "linux")]
fn report_backend(driver: &IoDriver) {
    let Some(stats) = driver.uring_stats() else {
        return;
    };
    let completions = (stats.inline + stats.reaped).max(1);
    println!(
        "{:<18} {:<28} {:.1} ops/enter  {:.0}% inline  ({} submits, {} enters)",
        "",
        "  ring",
        stats.submits as f64 / stats.enters.max(1) as f64,
        100.0 * stats.inline as f64 / completions as f64,
        stats.submits,
        stats.enters
    );
}

/// The completion port's counterpart. There is no batching factor to report — one operation is one
/// `ReadFile` — so the number that carries is the inline share: the operations the kernel finished
/// during the issuing call, which cost no packet, no reaper batch and no cross-thread wake. A low
/// share is the backend paying a round trip per operation, which is what the syscall pool costs
/// too, and is the reading to compare against `loreio`'s pool line above.
#[cfg(target_family = "windows")]
fn report_backend(driver: &IoDriver) {
    let Some(stats) = driver.iocp_stats() else {
        return;
    };
    let completions = (stats.inline + stats.reaped).max(1);
    println!(
        "{:<18} {:<28} {:.0}% inline  ({} submits, {} reaped)",
        "",
        "  port",
        100.0 * stats.inline as f64 / completions as f64,
        stats.submits,
        stats.reaped
    );
}

#[cfg(not(any(target_os = "linux", target_family = "windows")))]
fn report_backend(_driver: &IoDriver) {}

async fn small_files_create_phase(engine: &Engine, subdir: &Path) {
    let payload = Bytes::from(vec![0xabu8; SMALL_FILE_SIZE]);
    match engine {
        Engine::BlockingPool => {
            stream::iter(0..SMALL_FILE_COUNT)
                .map(|index| {
                    let path = subdir.join(format!("file-{index}"));
                    let payload = payload.clone();
                    async move { blocking_write_new_file(path, payload).await }
                })
                .buffer_unordered(SMALL_FILE_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
        }
        Engine::TokioFs => {
            stream::iter(0..SMALL_FILE_COUNT)
                .map(|index| {
                    let path = subdir.join(format!("file-{index}"));
                    let payload = payload.clone();
                    async move {
                        tokio::fs::write(path, payload)
                            .await
                            .expect("create failed");
                    }
                })
                .buffer_unordered(SMALL_FILE_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
        }
        Engine::LoreIo(driver, _) => {
            stream::iter(0..SMALL_FILE_COUNT)
                .map(|index| {
                    let path = subdir.join(format!("file-{index}"));
                    let payload = payload.clone();
                    let driver = driver.clone();
                    async move {
                        driver
                            .write_file_bytes(path, payload, false)
                            .await
                            .expect("create failed");
                    }
                })
                .buffer_unordered(SMALL_FILE_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
        }
    }
}

async fn small_files_read_phase(engine: &Engine, subdir: &Path) {
    match engine {
        Engine::BlockingPool => {
            stream::iter(0..SMALL_FILE_COUNT)
                .map(|index| {
                    let path = subdir.join(format!("file-{index}"));
                    async move { blocking_read_whole_file(path).await }
                })
                .buffer_unordered(SMALL_FILE_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
        }
        Engine::TokioFs => {
            stream::iter(0..SMALL_FILE_COUNT)
                .map(|index| {
                    let path = subdir.join(format!("file-{index}"));
                    async move {
                        tokio::fs::read(path).await.expect("read failed");
                    }
                })
                .buffer_unordered(SMALL_FILE_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
        }
        Engine::LoreIo(driver, _) => {
            stream::iter(0..SMALL_FILE_COUNT)
                .map(|index| {
                    let path = subdir.join(format!("file-{index}"));
                    let driver = driver.clone();
                    async move {
                        driver.read_file_bytes(&path).await.expect("read failed");
                    }
                })
                .buffer_unordered(SMALL_FILE_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
        }
    }
}

// ---------------------------------------------------------------------------
// Data preparation and cache eviction
// ---------------------------------------------------------------------------

async fn write_random_file(driver: &IoDriver, path: &Path, size: u64) {
    let file: IoFile = driver
        .open(
            path,
            &OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true),
        )
        .await
        .expect("failed to create data file");
    // One incompressible chunk, stamped with its index so no two are identical. Filling every byte
    // from the generator instead costs more than the write does once a data set is tens of
    // gigabytes, and what the contents have to be is unique and incompressible, not unpredictable.
    let chunk = 1024 * 1024;
    let mut rng = XorShift::new(0x5eed ^ size);
    let mut template = vec![0u8; chunk];
    for word in template.as_chunks_mut::<8>().0 {
        *word = rng.next().to_le_bytes();
    }
    // The tail matters: a size derived from a measured rate is not a whole number of chunks, and
    // a file written short by the remainder ends a phase in `UnexpectedEof` rather than a result.
    let mut written = 0u64;
    let mut index = 0u64;
    while written < size {
        let len = chunk.min((size - written) as usize);
        let mut buffer = template[..len].to_vec();
        buffer[..8].copy_from_slice(&index.to_le_bytes());
        file.write_all_at(buffer, written)
            .await
            .expect("failed to write data file");
        written += len as u64;
        index += 1;
    }
    file.sync_data().await.expect("failed to sync data file");
}

fn random_offsets(ops: usize, file_size: u64, size: usize) -> Vec<u64> {
    let mut rng = XorShift::new(0xdeadbeef ^ size as u64);
    let slots = file_size / size as u64;
    (0..ops)
        .map(|_| (rng.next() % slots) * size as u64)
        .collect()
}

/// Offsets covering `take` distinct slots in random order, so a cold run
/// never re-reads a block it already pulled into cache.
fn permuted_offsets(slots: u64, take: usize, stride: u64) -> Vec<u64> {
    let mut rng = XorShift::new(0xc01dcafe ^ slots);
    let mut indices: Vec<u64> = (0..slots).collect();
    for i in (1..indices.len()).rev() {
        let j = (rng.next() % (i as u64 + 1)) as usize;
        indices.swap(i, j);
    }
    indices.truncate(take);
    indices.into_iter().map(|index| index * stride).collect()
}

/// Drops a file's clean pages from the page cache. Effective on page-cache
/// filesystems (ext4, xfs); a no-op on ZFS, whose ARC requires the
/// `drop_caches` step from the cold-suite instructions.
#[cfg(all(target_family = "unix", not(target_vendor = "apple")))]
fn evict_from_page_cache(path: &Path) {
    use std::os::fd::AsRawFd;
    let Ok(file) = File::open(path) else {
        return;
    };
    let _ = file.sync_all();
    // Safety: Calling OS functions
    unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
}

/// Drops a file's cached pages on Apple platforms, which have no `posix_fadvise`.
///
/// `msync(MS_INVALIDATE)` over a mapping of the whole file invalidates that file's resident pages
/// in the unified buffer cache, and needs no privilege — unlike `purge`, which is what
/// `drop_caches` is here and which refuses without root. `F_NOCACHE` and `F_GLOBAL_NOCACHE` are
/// the candidates that look right and neither evicts anything: they change how a descriptor
/// reads, not what the cache holds.
///
/// Verified rather than assumed: a 256 MiB file re-read at 17-18 GiB/s before the call and
/// 3.3 GiB/s after it across three rounds, which is this machine's SSD sequential rate, and
/// 4 KiB whole files at 16 us before and 43 us after. The same probe measured no change at all
/// from either `F_NOCACHE` spelling.
#[cfg(all(target_family = "unix", target_vendor = "apple"))]
fn evict_from_page_cache(path: &Path) {
    use std::os::fd::AsRawFd;
    let Ok(file) = File::open(path) else {
        return;
    };
    let _ = file.sync_all();
    let Ok(length) = file.metadata().map(|metadata| metadata.len() as usize) else {
        return;
    };
    if length == 0 {
        return;
    }
    // Safety: Calling OS functions
    unsafe {
        let mapping = libc::mmap(
            std::ptr::null_mut(),
            length,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        );
        if mapping != libc::MAP_FAILED {
            libc::msync(mapping, length, libc::MS_INVALIDATE);
            libc::munmap(mapping, length);
        }
    }
}

/// Drops a file's cached pages on Windows, which has no `posix_fadvise`.
///
/// Opening a file with `FILE_FLAG_NO_BUFFERING` makes the cache manager flush and purge that
/// file's section, so the next buffered read of it reaches the device. That is the per-file
/// equivalent of the `posix_fadvise` call above and needs no privilege, unlike clearing the
/// standby list, which is what the machine-wide tools do and what `drop_caches` is on Linux.
/// Verified rather than assumed: a 1 GiB file re-read at 4.6 GiB/s before the purge and 469 MiB/s
/// after it, which is this drive's sequential rate.
#[cfg(target_family = "windows")]
fn evict_from_page_cache(path: &Path) {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;

    let _ = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_NO_BUFFERING)
        .open(path);
}

#[cfg(not(any(target_family = "unix", target_family = "windows")))]
fn evict_from_page_cache(_path: &Path) {}

#[cfg(target_family = "unix")]
fn sync_filesystem() {
    // Safety: Calling OS functions
    unsafe { libc::sync() };
}

/// No machine-wide flush on Windows: `FlushFileBuffers` on a volume handle needs administrator
/// rights, which nothing else about running this benchmark does. The settle between engine
/// children still runs, and each cold phase purges the files it is about to read.
#[cfg(not(target_family = "unix"))]
fn sync_filesystem() {}

// ---------------------------------------------------------------------------
// Blocking-pool engine
// ---------------------------------------------------------------------------

// The blocking-pool engine deliberately reproduces today's dispatch shape
// (tokio::fs / lore_spawn_blocking! both round-trip through the tokio
// blocking pool), so it uses spawn_blocking directly as the baseline.
#[allow(clippy::disallowed_methods)]
async fn blocking_read_exact_at(file: Arc<File>, size: usize, offset: u64) {
    // `LORE_BENCH_ALLOC_OUTSIDE` moves the allocation to the calling task, which is where lore-io
    // does it. Both spellings are `vec![0u8; size]`; what differs by default is the thread it runs
    // on — the baseline allocates on the pool thread that will receive the read and frees on the
    // worker that awaits it, lore-io does both on the worker.
    if alloc_outside() {
        let mut buffer = vec![0u8; size];
        tokio::task::spawn_blocking(move || {
            read_exact_at_impl(&file, &mut buffer, offset).expect("read failed");
            buffer
        })
        .await
        .expect("blocking task failed");
    } else {
        tokio::task::spawn_blocking(move || {
            let mut buffer = vec![0u8; size];
            read_exact_at_impl(&file, &mut buffer, offset).expect("read failed");
            buffer
        })
        .await
        .expect("blocking task failed");
    }
}

#[allow(clippy::disallowed_methods)]
async fn blocking_write_all_at(file: Arc<File>, size: usize, offset: u64) {
    tokio::task::spawn_blocking(move || {
        let buffer = vec![0u8; size];
        write_all_at_impl(&file, &buffer, offset).expect("write failed");
    })
    .await
    .expect("blocking task failed");
}

#[allow(clippy::disallowed_methods)]
async fn blocking_sync_data(file: Arc<File>) {
    tokio::task::spawn_blocking(move || file.sync_data().expect("sync failed"))
        .await
        .expect("blocking task failed");
}

#[allow(clippy::disallowed_methods)]
async fn blocking_write_new_file(path: PathBuf, payload: Bytes) {
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .expect("create failed");
        file.write_all(payload.as_ref()).expect("write failed");
        file.metadata().expect("stat failed")
    })
    .await
    .expect("blocking task failed");
}

#[allow(clippy::disallowed_methods)]
async fn blocking_read_whole_file(path: PathBuf) {
    tokio::task::spawn_blocking(move || std::fs::read(path).expect("read failed"))
        .await
        .expect("blocking task failed");
}

// ---------------------------------------------------------------------------
// tokio::fs engine
// ---------------------------------------------------------------------------

// tokio::fs has no positional read or write: `tokio::fs::File` is AsyncSeek + AsyncRead, so an
// offset is reached by seeking, and the file offset is per file description. `try_clone` is a
// `dup`, which shares that offset, so concurrent operations at different offsets cannot share a
// handle — each one opens its own. That open is not an artefact of the benchmark; it is what the
// API forces on any caller doing concurrent positional I/O, and it is the reason a migration to
// tokio::fs would cost more than the blocking-pool baseline it would replace.
async fn tokio_fs_read_exact_at(path: PathBuf, size: usize, offset: u64) {
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncSeekExt;

    let mut file = tokio::fs::File::open(&path).await.expect("open failed");
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .expect("seek failed");
    let mut buffer = vec![0u8; size];
    file.read_exact(&mut buffer).await.expect("read failed");
}

async fn tokio_fs_write_all_at(path: PathBuf, size: usize, offset: u64) {
    use tokio::io::AsyncSeekExt;
    use tokio::io::AsyncWriteExt;

    let mut file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .await
        .expect("open failed");
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .expect("seek failed");
    file.write_all(&vec![0u8; size])
        .await
        .expect("write failed");
}

#[cfg(target_family = "unix")]
fn read_exact_at_impl(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buffer, offset)
}

#[cfg(target_family = "unix")]
fn write_all_at_impl(file: &File, buffer: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buffer, offset)
}

#[cfg(target_family = "windows")]
fn read_exact_at_impl(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buffer.len() {
        let read = file.seek_read(&mut buffer[done..], offset + done as u64)?;
        assert!(read > 0, "unexpected end of file");
        done += read;
    }
    Ok(())
}

#[cfg(target_family = "windows")]
fn write_all_at_impl(file: &File, buffer: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buffer.len() {
        done += file.seek_write(&buffer[done..], offset + done as u64)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Reporting and utilities
// ---------------------------------------------------------------------------

fn print_header() {
    println!(
        "{:<18} {:<28} {:>8} {:>9} {:>8} {:>10} {:>9}",
        "engine", "workload", "ops", "MiB", "secs", "ops/s", "MiB/s"
    );
}

/// Shortest a phase may run before its result is worth less than it looks.
///
/// Phase sizes are fixed in the source and were chosen against the devices available at the time,
/// so a faster one shrinks them: the cold phases are 4096 operations, about a second on a SATA SSD
/// and 90 ms on an `NVMe`. A phase that short measures frequency ramp and cache state alongside the
/// engine, and no number of repeats recovers the signal — this prints rather than adjusts because
/// the fixed sizes are what make two people's runs comparable.
const SHORT_PHASE_SECONDS: f64 = 0.25;

fn report(engine: &Engine, workload: &str, ops: usize, bytes: usize, started: Instant) -> f64 {
    let seconds = started.elapsed().as_secs_f64();
    let mib = bytes as f64 / (1024.0 * 1024.0);
    println!(
        "{:<18} {:<28} {:>8} {:>9.1} {:>8.3} {:>10.0} {:>9.1}",
        engine.name(),
        workload,
        ops,
        mib,
        seconds,
        ops as f64 / seconds,
        mib / seconds
    );
    if seconds < SHORT_PHASE_SECONDS {
        println!(
            "{:<18} {:<28} ran {seconds:.3}s, under the {SHORT_PHASE_SECONDS:.2}s floor — this device outpaces the phase size",
            "", "warning",
        );
    }
    ops as f64 / seconds
}

/// A [`run_cold_baseline`] line, tagged by the engine whose data set it measured rather than by an
/// engine that ran, in the column layout [`report`] uses.
fn report_baseline(tag: &str, workload: &str, ops: usize, bytes: usize, ops_per_sec: f64) {
    let mib = bytes as f64 / (1024.0 * 1024.0);
    let seconds = ops as f64 / ops_per_sec;
    println!(
        "{:<18} {:<28} {:>8} {:>9.1} {:>8.3} {:>10.0} {:>9.1}",
        format!("baseline:{tag}"),
        workload,
        ops,
        mib,
        seconds,
        ops_per_sec,
        mib / seconds
    );
}

struct XorShift {
    state: u64,
}

impl XorShift {
    fn new(seed: u64) -> XorShift {
        XorShift { state: seed | 1 }
    }

    fn next(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> TempDir {
        let path = bench_root().join(format!("lore-io-bench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("failed to create bench dir");
        TempDir { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
