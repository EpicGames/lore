// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Throughput and latency of `State`'s node operations, for an A/B of two revisions of the node
//! interface. Built only on demand: `cargo run --release -p lore-revision --example state_bench
//! --features state-bench -- [options]`.
//!
//! Each workload runs on a fresh in-memory State, every block resident, so the numbers are the
//! node operations' own: their locks, walks and bookkeeping. With `--cold`, and in `sweep-loaded`,
//! the State is read back from the store instead, so its blocks load on demand. Every operation's
//! latency is recorded, and a workload prints one line:
//!
//! ```text
//! BENCH name=<workload> threads=<t> readers=<r> ops=<n> secs=<s> ops_per_sec=<x>
//!       p50_ns=<a> p99_ns=<b> p999_ns=<c> retries=<k> walks=<w> brackets=<m>
//! ```
//!
//! Options: `--threads 1,4,8` (writer tasks, default `1,4,8`), `--readers R` (walkers over the
//! contended directory during the shared workloads, default 0), `--ops N` (operations per writer,
//! default 20000), `--runs K` (repeats of each workload, default 1), `--only a,b` (workloads by
//! name, default all), `--current-thread` (one writer on a `current_thread` runtime, for
//! instruction counts), `--perf-ctl <fifo> --perf-ack <fifo>` (enable a `perf stat
//! --control=fifo:<ctl>,<ack> -D -1` only across each workload's timed section; with
//! `--current-thread`, the setup a workload does between its operations is left out too),
//! `--cold` (the discard workloads run on a State written to the store and read back, so their
//! blocks load on demand), `--depth D` (the directories, all settled, above the file
//! `served-unlink-settled-deep` unlinks, default 9), `--sizes` (print the sizes of the structures
//! the node operations hold, and end), `--list` (print the workload names, one per line, and end).
//!
//! The `served-*` workloads drive a node operation as the VFS provider drives it on the State
//! it serves; `swap-copy` and `copy-subtree` as a swap copies a subtree, `swap-copy-file` as it
//! copies a file; `discard-files` as a commit's post-pass discards; `discard-dirs` as the callers
//! that free a directory's subtree at once discard; `sweep-detached` and `sweep-loaded` as the
//! stage's and the commits' sweep of detached slots meets a session's removals and the blocks it
//! loaded.

mod ops;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use ops::Repo;

const WORKLOADS: &[&str] = &[
    "loop-null",
    "bracket-null",
    "add-own",
    "add-shared",
    "attach-own",
    "attach-shared",
    "detach-head",
    "reattach-pingpong",
    "move-staged-serial",
    "rewrite",
    "mark-early-out",
    "mark-fresh",
    "mark-staged",
    "mark-leaf-fresh",
    "mark-moved-subtree",
    "clear-dirty-sibling",
    "clear-dirty-sibling-tail",
    "served-mark-early-out",
    "served-mark-climb",
    "served-create",
    "served-create-settled",
    "served-copy-up",
    "served-settle",
    "served-settle-subdirs",
    "served-settle-wide",
    "served-settle-dirty",
    "served-rmdir",
    "served-rmdir-settled",
    "served-rename-over",
    "served-rename-over-settled",
    "served-unlink",
    "served-unlink-settled",
    "served-unlink-settled-deep",
    "served-rename",
    "served-rename-settled",
    "mount-new-dir",
    "mount-tracked-dir",
    "swap-copy",
    "swap-copy-file",
    "copy-subtree",
    "discard-files",
    "discard-dirs",
    "discard-dirs-spread",
    "sweep-detached",
    "sweep-loaded",
];

/// The one workload a gate build carries, from `STATE_BENCH_ONLY` at build time. Every other
/// workload's code is then left out, so it cannot change how the carried one compiles.
const ONLY: Option<&str> = option_env!("STATE_BENCH_ONLY");

/// Depth of the directory chain the marking workloads mark under.
const MARK_DEPTH: usize = 8;

/// Names per directory in the attach workloads, which walk the chain to check the name: kept
/// small enough that a walk costs about what a real directory's does.
const ATTACH_NAMES: usize = 512;

/// Files in each directory the `discard-dirs` workload discards.
const DISCARD_FILES: usize = 16;

/// Files per operation the `sweep-loaded` workload stores, so its blocks number about
/// `ops * SWEEP_LOADED_FILES / 512`.
const SWEEP_LOADED_FILES: usize = 4;

/// Files in each directory the `swap-copy` workload copies.
const COPY_FILES: usize = 16;

/// Local files a mount adds into each new directory in `mount-new-dir`.
const MOUNT_NEW_FILES: usize = 10_000;

/// Tracked files in each directory `mount-tracked-dir` mounts over, and the local files it adds or
/// rewrites there: every tenth local file has a tracked one's name.
const MOUNT_TRACKED_FILES: usize = 10_000;
const MOUNT_LOCAL_FILES: usize = 1_000;

struct Options {
    threads: Vec<usize>,
    readers: usize,
    ops: usize,
    runs: usize,
    only: Option<Vec<String>>,
    current_thread: bool,
    cold: bool,
    depth: usize,
}

/// The `perf stat` control and acknowledgement FIFOs, when given.
static PERF: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();

/// Whether [`untimed`] stops the count: only with one writer on a `current_thread` runtime, where
/// nothing else runs meanwhile.
static BRACKET: AtomicBool = AtomicBool::new(false);

/// The brackets [`untimed`] made in the current workload, for subtracting their cost.
static BRACKETS: AtomicU64 = AtomicU64::new(0);

/// Runs `work`, a workload's setup inside its timed loop, with the instruction count stopped.
async fn untimed<F: std::future::Future>(work: F) -> F::Output {
    let bracket = BRACKET.load(Ordering::Relaxed);
    if bracket {
        perf(b"disable\n");
    }
    let output = work.await;
    if bracket {
        BRACKETS.fetch_add(1, Ordering::Relaxed);
        perf(b"enable\n");
    }
    output
}

thread_local! {
    /// The `perf stat` control and acknowledgement FIFOs, opened at the first command. Every
    /// command comes from the thread driving the runtime.
    static PERF_FIFOS: std::cell::RefCell<Option<(std::fs::File, std::fs::File)>> =
        const { std::cell::RefCell::new(None) };
}

/// Sends `command`, a line, to `perf stat` and waits for its acknowledgement; nothing without
/// `--perf-ctl`.
fn perf(command: &'static [u8]) {
    use std::io::Read as _;
    use std::io::Write as _;
    let Some((ctl, ack)) = PERF.get() else {
        return;
    };
    PERF_FIFOS.with_borrow_mut(|fifos| {
        let (control, acknowledged) = fifos.get_or_insert_with(|| {
            let control = std::fs::OpenOptions::new()
                .write(true)
                .open(ctl)
                .expect("the perf control FIFO opens");
            let acknowledged = std::fs::File::open(ack).expect("the perf ack FIFO opens");
            (control, acknowledged)
        });
        control.write_all(command).expect("perf takes the command");
        let mut answer = [0_u8; 5];
        let _ = acknowledged.read(&mut answer);
    });
}

fn parse() -> Options {
    let mut options = Options {
        threads: vec![1, 4, 8],
        readers: 0,
        ops: 20_000,
        runs: 1,
        only: None,
        current_thread: false,
        cold: false,
        depth: 9,
    };
    let mut perf_ctl = None;
    let mut perf_ack = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .unwrap_or_else(|| usage(&format!("{arg} needs a value")))
        };
        match arg.as_str() {
            "--threads" => {
                options.threads = value()
                    .split(',')
                    .map(|t| t.parse().unwrap_or_else(|_| usage("bad --threads")))
                    .collect();
            }
            "--readers" => {
                options.readers = value().parse().unwrap_or_else(|_| usage("bad --readers"));
            }
            "--ops" => options.ops = value().parse().unwrap_or_else(|_| usage("bad --ops")),
            "--runs" => options.runs = value().parse().unwrap_or_else(|_| usage("bad --runs")),
            "--only" => {
                let names: Vec<String> = value().split(',').map(str::to_string).collect();
                for name in &names {
                    if !WORKLOADS.contains(&name.as_str()) {
                        usage(&format!("unknown workload {name}"));
                    }
                }
                options.only = Some(names);
            }
            "--current-thread" => options.current_thread = true,
            "--cold" => options.cold = true,
            "--depth" => {
                options.depth = value().parse().unwrap_or_else(|_| usage("bad --depth"));
                if options.depth == 0 {
                    usage("--depth is at least 1");
                }
            }
            "--sizes" => sizes(),
            "--list" => list(),
            "--perf-ctl" => perf_ctl = Some(value()),
            "--perf-ack" => perf_ack = Some(value()),
            _ => usage(&format!("unknown option {arg}")),
        }
    }
    match (perf_ctl, perf_ack) {
        (Some(ctl), Some(ack)) => {
            let _ = PERF.set((ctl, ack));
        }
        (None, None) => {}
        _ => usage("--perf-ctl and --perf-ack go together"),
    }
    if options.current_thread {
        if options.readers > 0 {
            usage("--readers needs a multi-thread runtime");
        }
        options.threads = vec![1];
        BRACKET.store(PERF.get().is_some(), Ordering::Relaxed);
    }
    options
}

/// Prints the sizes of the structures a node operation holds or grows, one `SIZE` line each, and
/// ends the bench.
#[allow(clippy::exit)]
fn sizes() -> ! {
    println!("SIZE Node={}", size_of::<lore_revision::node::Node>());
    println!(
        "SIZE NodeBlock={}",
        size_of::<lore_revision::node::NodeBlock>()
    );
    #[cfg(target_os = "linux")]
    println!(
        "SIZE InodeEntry={}",
        size_of::<lore_revision::fs::fuse::inode::InodeEntry>()
    );
    std::process::exit(0);
}

/// Prints the workloads this build runs, one name per line, and ends the bench.
#[allow(clippy::exit)]
fn list() -> ! {
    for name in WORKLOADS {
        if ONLY.is_none_or(|only| only == *name) {
            println!("{name}");
        }
    }
    std::process::exit(0);
}

/// Ends the bench with status 2, as a usage error.
#[allow(clippy::exit)]
fn usage(error: &str) -> ! {
    eprintln!("state_bench: {error}");
    eprintln!("workloads: {}", WORKLOADS.join(", "));
    std::process::exit(2);
}

fn main() {
    const {
        assert!(
            !lore_revision::state::node::HOOKS_ENABLED,
            "a bench never builds with state-hooks"
        );
    }
    let options = parse();
    for &threads in &options.threads {
        let runtime = if options.current_thread {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("the runtime starts")
        } else {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(threads.max(1) + options.readers)
                .enable_all()
                .build()
                .expect("the runtime starts")
        };
        for &name in WORKLOADS {
            if ONLY.is_some_and(|only| only != name)
                || options
                    .only
                    .as_ref()
                    .is_some_and(|only| !only.iter().any(|o| o == name))
            {
                continue;
            }
            for _ in 0..options.runs {
                let result = runtime.block_on(run(name, threads, &options));
                result.print(name, threads, options.readers);
            }
        }
    }
}

struct Outcome {
    ops: usize,
    elapsed: Duration,
    latencies: Vec<u32>,
    retries: u64,
    walks: u64,
    brackets: u64,
}

impl Outcome {
    fn print(mut self, name: &str, threads: usize, readers: usize) {
        self.latencies.sort_unstable();
        let at = |q: f64| {
            let len = self.latencies.len();
            if len == 0 {
                return 0;
            }
            self.latencies[((len as f64 * q) as usize).min(len - 1)]
        };
        let secs = self.elapsed.as_secs_f64();
        println!(
            "BENCH name={name} threads={threads} readers={readers} ops={} secs={secs:.4} \
             ops_per_sec={:.0} p50_ns={} p99_ns={} p999_ns={} retries={} walks={} brackets={}",
            self.ops,
            self.ops as f64 / secs,
            at(0.50),
            at(0.99),
            at(0.999),
            self.retries,
            self.walks,
            self.brackets,
        );
    }
}

async fn repository() -> Repo {
    let execution = Arc::new(
        lore_revision::interface::ExecutionContext::new_client_with_user_id(
            lore_revision::interface::LoreGlobalArgs::default(),
            lore_revision::relay::EventDispatcher::no_dispatch(),
            "state-bench".to_string(),
        ),
    );
    LORE_CONTEXT
        .scope(execution, async {
            let immutable = lore_storage::local::immutable_store::create(
                None::<&str>,
                lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
                false,
                lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
            )
            .await
            .expect("an in-memory immutable store");
            let mutable = lore_storage::local::mutable_store::create(
                None::<&str>,
                lore_storage::MutableStoreSettings::default(),
                immutable.clone(),
            )
            .await
            .expect("an in-memory mutable store");
            Arc::new(RepositoryContext::new_null_context(immutable, mutable))
        })
        .await
}

fn execution() -> Arc<dyn std::any::Any + Send + Sync> {
    Arc::new(
        lore_revision::interface::ExecutionContext::new_client_with_user_id(
            lore_revision::interface::LoreGlobalArgs::default(),
            lore_revision::relay::EventDispatcher::no_dispatch(),
            "state-bench".to_string(),
        ),
    )
}

/// Runs `per_task` on `threads` tasks at once, each told its index, timing every operation it
/// reports; meanwhile `readers` tasks walk `watched` until the writers are done.
async fn timed<F, Fut>(
    state: &Arc<State>,
    repo: &Repo,
    threads: usize,
    readers: usize,
    watched: NodeID,
    per_task: F,
) -> (Duration, Vec<u32>, u64)
where
    F: Fn(usize) -> Fut,
    Fut: std::future::Future<Output = Vec<u32>> + Send + 'static,
{
    let done = Arc::new(AtomicBool::new(false));
    let walks = Arc::new(AtomicU64::new(0));
    let mut walkers = Vec::new();
    for _ in 0..readers {
        let (state, repo, done, walks) = (state.clone(), repo.clone(), done.clone(), walks.clone());
        walkers.push(lore_base::lore_spawn!(async move {
            while !done.load(Ordering::Relaxed) {
                ops::walk(&state, &repo, watched).await;
                walks.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    let start = Arc::new(tokio::sync::Barrier::new(threads + 1));
    let mut writers = Vec::new();
    for task in 0..threads {
        let work = per_task(task);
        let start = start.clone();
        writers.push(lore_base::lore_spawn!(async move {
            start.wait().await;
            work.await
        }));
    }
    perf(b"enable\n");
    // Started before this task's own arrival releases the writers: on a `current_thread`
    // runtime the last writer to arrive releases them and runs its whole share before this task
    // is polled again.
    let began = Instant::now();
    start.wait().await;
    let mut latencies = Vec::new();
    for writer in writers {
        latencies.extend(writer.await.expect("a writer finishes"));
    }
    let elapsed = began.elapsed();
    perf(b"disable\n");
    done.store(true, Ordering::Relaxed);
    for walker in walkers {
        walker.await.expect("a walker finishes");
    }
    (elapsed, latencies, walks.load(Ordering::Relaxed))
}

fn nanos(began: Instant) -> u32 {
    u32::try_from(began.elapsed().as_nanos()).unwrap_or(u32::MAX)
}

/// Per task, a chain of `MARK_DEPTH` directories below its own directory, named after `prefix`,
/// and 64 files in the deepest; the files.
async fn leaves_under_chain(
    state: &State,
    repo: &Repo,
    own: &[NodeID],
    prefix: &str,
) -> Arc<Vec<Vec<NodeID>>> {
    let mut leaves = Vec::with_capacity(own.len());
    for (task, &top) in own.iter().enumerate() {
        let mut parent = top;
        for level in 0..MARK_DEPTH {
            let name = format!("{prefix}{task}-{level}");
            parent = ops::add(state, repo, parent, ops::directory(&name), &name).await;
        }
        let mut ids = Vec::with_capacity(64);
        for i in 0..64 {
            let name = format!("leaf{i}");
            ids.push(ops::add(state, repo, parent, ops::file(&name), &name).await);
        }
        leaves.push(ids);
    }
    Arc::new(leaves)
}

async fn run(name: &str, threads: usize, options: &Options) -> Outcome {
    let repo = repository().await;
    LORE_CONTEXT
        .scope(execution(), run_scoped(name, threads, options, repo))
        .await
}

async fn run_scoped(name: &str, threads: usize, options: &Options, repo: Repo) -> Outcome {
    let name = ONLY.unwrap_or(name);
    let state = State::new();
    let n = options.ops;
    let readers = options.readers;
    let shared = ops::add(&state, &repo, ROOT_NODE, ops::directory("shared"), "shared").await;
    let mut own = Vec::with_capacity(threads);
    for task in 0..threads {
        let name = format!("own-{task}");
        own.push(ops::add(&state, &repo, ROOT_NODE, ops::directory(&name), &name).await);
    }
    let own = Arc::new(own);
    let mut ran_on = state.clone();

    let (elapsed, latencies, walks, ops) = match name {
        // The timing loop alone, and the loop with one empty bracket per operation: the costs the
        // other workloads' counts carry beside their operations.
        "loop-null" | "bracket-null" => {
            let bracketed = name == "bracket-null";
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |_| async move {
                let mut latencies = Vec::with_capacity(n);
                for _ in 0..n {
                    if bracketed {
                        untimed(async {}).await;
                    }
                    let began = Instant::now();
                    latencies.push(nanos(began));
                }
                latencies
            })
            .await;
            (e, l, w, n * threads)
        }
        // Serial-mode adds of fresh names, each task into its own directory, or all into one.
        "add-own" | "add-shared" => {
            let into_shared = name == "add-shared";
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, own) = (state.clone(), repo.clone(), own.clone());
                async move {
                    let parent = if into_shared { shared } else { own[task] };
                    let mut latencies = Vec::with_capacity(n);
                    for i in 0..n {
                        let name = format!("f{task}-{i}");
                        let began = Instant::now();
                        ops::add(&state, &repo, parent, ops::file(&name), &name).await;
                        latencies.push(nanos(began));
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, n * threads)
        }
        // Checked attaches, each looking its name up by walking the chain, of names cycled
        // through a directory of at most ATTACH_NAMES: one detach per attach keeps it that size.
        "attach-own" | "attach-shared" => {
            let into_shared = name == "attach-shared";
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, own) = (state.clone(), repo.clone(), own.clone());
                async move {
                    let parent = if into_shared { shared } else { own[task] };
                    let mut latencies = Vec::with_capacity(n);
                    let mut live = std::collections::VecDeque::with_capacity(ATTACH_NAMES);
                    for i in 0..n {
                        let name = format!("a{task}-{i}");
                        let began = Instant::now();
                        let attached =
                            ops::attach(&state, &repo, parent, ops::file(&name), &name).await;
                        latencies.push(nanos(began));
                        live.push_back(attached.expect("a fresh name attaches"));
                        if live.len() >= ATTACH_NAMES / threads.max(1) {
                            let old = live.pop_front().expect("a live node");
                            ops::detach(&state, &repo, old).await;
                        }
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, n * threads)
        }
        // Detaches of the head of each task's own chain, the cheapest unlink.
        "detach-head" => {
            let mut added = Vec::with_capacity(threads);
            for (task, &parent) in own.iter().enumerate() {
                let mut ids = Vec::with_capacity(n);
                for i in 0..n {
                    let name = format!("d{task}-{i}");
                    ids.push(ops::add(&state, &repo, parent, ops::file(&name), &name).await);
                }
                added.push(ids);
            }
            let added = Arc::new(added);
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, added) = (state.clone(), repo.clone(), added.clone());
                async move {
                    let mut latencies = Vec::with_capacity(n);
                    for &id in added[task].iter().rev() {
                        let began = Instant::now();
                        ops::detach(&state, &repo, id).await;
                        latencies.push(nanos(began));
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, n * threads)
        }
        // Checked moves of a file back and forth between two directories of each task's own.
        "reattach-pingpong" => {
            let mut pairs = Vec::with_capacity(threads);
            for task in 0..threads {
                let a_name = format!("pa{task}");
                let b_name = format!("pb{task}");
                let a = ops::add(&state, &repo, ROOT_NODE, ops::directory(&a_name), &a_name).await;
                let b = ops::add(&state, &repo, ROOT_NODE, ops::directory(&b_name), &b_name).await;
                for i in 0..32 {
                    let name = format!("filler{i}");
                    ops::add(&state, &repo, a, ops::file(&name), &name).await;
                    ops::add(&state, &repo, b, ops::file(&name), &name).await;
                }
                let moved = ops::add(&state, &repo, a, ops::file("moved"), "moved").await;
                pairs.push((a, b, moved));
            }
            let pairs = Arc::new(pairs);
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, pairs) = (state.clone(), repo.clone(), pairs.clone());
                async move {
                    let (a, b, moved) = pairs[task];
                    let mut latencies = Vec::with_capacity(n);
                    for i in 0..n {
                        let target = if i % 2 == 0 { b } else { a };
                        let began = Instant::now();
                        let ok = ops::reattach(&state, &repo, moved, target, "moved").await;
                        latencies.push(nanos(began));
                        assert!(ok, "the move lands");
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, n * threads)
        }
        // Staged moves, one task: the serial move with its staging marks.
        "move-staged-serial" => {
            let a = ops::add(&state, &repo, ROOT_NODE, ops::directory("ma"), "ma").await;
            let b = ops::add(&state, &repo, ROOT_NODE, ops::directory("mb"), "mb").await;
            for i in 0..32 {
                let name = format!("filler{i}");
                ops::add(&state, &repo, a, ops::file(&name), &name).await;
                ops::add(&state, &repo, b, ops::file(&name), &name).await;
            }
            let moved = ops::add(&state, &repo, a, ops::file("moved"), "moved").await;
            let serial = n.min(20_000);
            let (e, l, w) = timed(&state, &repo, 1, readers, shared, |_| {
                let (state, repo) = (state.clone(), repo.clone());
                async move {
                    let mut latencies = Vec::with_capacity(serial);
                    for i in 0..serial {
                        let target = if i % 2 == 0 { b } else { a };
                        let began = Instant::now();
                        ops::move_staged(&state, &repo, moved, target, "moved").await;
                        latencies.push(nanos(began));
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, serial)
        }
        // Content rewrites of files each task owns.
        "rewrite" => {
            let mut files = Vec::with_capacity(threads);
            for (task, &parent) in own.iter().enumerate() {
                let mut ids = Vec::with_capacity(64);
                for i in 0..64 {
                    let name = format!("r{task}-{i}");
                    ids.push(ops::add(&state, &repo, parent, ops::file(&name), &name).await);
                }
                files.push(ids);
            }
            let files = Arc::new(files);
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, files) = (state.clone(), repo.clone(), files.clone());
                async move {
                    let mut latencies = Vec::with_capacity(n);
                    for i in 0..n {
                        let id = files[task][i % 64];
                        let began = Instant::now();
                        ops::rewrite(&state, &repo, id, i as u64).await;
                        latencies.push(nanos(began));
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, n * threads)
        }
        // Marks under a chain of MARK_DEPTH directories per task, each arm one adapter call:
        // early-out marks of leaves whose ancestors are already dirty, fresh marks each followed by
        // a clear of the path, and staged marks.
        "mark-early-out" => {
            let leaves = leaves_under_chain(&state, &repo, &own, "m").await;
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, leaves) = (state.clone(), repo.clone(), leaves.clone());
                async move {
                    let mut latencies = Vec::with_capacity(n);
                    for i in 0..n {
                        let id = leaves[task][i % 64];
                        let began = Instant::now();
                        ops::mark_dirty(&state, &repo, id).await;
                        latencies.push(nanos(began));
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, n * threads)
        }
        "mark-fresh" => {
            let leaves = leaves_under_chain(&state, &repo, &own, "m").await;
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, leaves) = (state.clone(), repo.clone(), leaves.clone());
                async move {
                    let mut latencies = Vec::with_capacity(n);
                    for i in 0..n {
                        let id = leaves[task][i % 64];
                        let began = Instant::now();
                        ops::mark_fresh(&state, &repo, id).await;
                        latencies.push(nanos(began));
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, n * threads)
        }
        "mark-staged" => {
            let leaves = leaves_under_chain(&state, &repo, &own, "m").await;
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, leaves) = (state.clone(), repo.clone(), leaves.clone());
                async move {
                    let mut latencies = Vec::with_capacity(n);
                    for i in 0..n {
                        let id = leaves[task][i % 64];
                        let began = Instant::now();
                        ops::mark_staged(&state, &repo, id).await;
                        latencies.push(nanos(began));
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, n * threads)
        }
        // Unforced marks of a clean leaf whose directory a dirty sibling keeps dirty, each leaf
        // cleared again outside the timing: the mark sets the leaf and stops at its directory.
        "mark-leaf-fresh" => {
            let leaves = leaves_under_chain(&state, &repo, &own, "l").await;
            for ids in leaves.iter() {
                ops::mark_dirty(&state, &repo, ids[63]).await;
            }
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, leaves) = (state.clone(), repo.clone(), leaves.clone());
                async move {
                    let mut latencies = Vec::with_capacity(n);
                    for i in 0..n {
                        let id = leaves[task][i % 63];
                        let began = Instant::now();
                        ops::mark_dirty(&state, &repo, id).await;
                        latencies.push(nanos(began));
                        untimed(ops::clear_dirty(&state, &repo, id)).await;
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, n * threads)
        }
        // The descendants of a moved directory, MARK_DEPTH levels down, each marked once as
        // `mark_subtree_moved` marks them, the directory and its ancestors marked first.
        "mark-moved-subtree" => {
            let mut children = Vec::with_capacity(threads);
            for (task, &top) in own.iter().enumerate() {
                let mut parent = top;
                for level in 0..MARK_DEPTH {
                    let name = format!("v{task}-{level}");
                    parent = ops::add(&state, &repo, parent, ops::directory(&name), &name).await;
                }
                let moved = ops::add(&state, &repo, parent, ops::directory("moved"), "moved").await;
                ops::mark_moved(&state, &repo, moved).await;
                let mut ids = Vec::with_capacity(n);
                for i in 0..n {
                    let name = format!("o{i}");
                    ids.push(ops::add(&state, &repo, moved, ops::file(&name), &name).await);
                }
                children.push(ids);
            }
            let children = Arc::new(children);
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, children) = (state.clone(), repo.clone(), children.clone());
                async move {
                    let mut latencies = Vec::with_capacity(n);
                    for &id in &children[task] {
                        let began = Instant::now();
                        ops::mark_moved(&state, &repo, id).await;
                        latencies.push(nanos(began));
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, n * threads)
        }
        // Clears of a leaf marked dirty outside the timing, under MARK_DEPTH directories, beside a
        // sibling that stays dirty: the clear stops at the leaf's directory. The dirty sibling is
        // the newest file, at the head of the chain, or with `-tail` the oldest, at its tail.
        "clear-dirty-sibling" | "clear-dirty-sibling-tail" => {
            let tail = name == "clear-dirty-sibling-tail";
            let leaves = leaves_under_chain(&state, &repo, &own, "q").await;
            let dirty = if tail { 0 } else { 63 };
            for ids in leaves.iter() {
                ops::mark_dirty(&state, &repo, ids[dirty]).await;
            }
            let first = usize::from(tail);
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, leaves) = (state.clone(), repo.clone(), leaves.clone());
                async move {
                    let mut latencies = Vec::with_capacity(n);
                    for i in 0..n {
                        let id = leaves[task][first + i % 63];
                        untimed(ops::mark_dirty(&state, &repo, id)).await;
                        let began = Instant::now();
                        ops::clear_dirty(&state, &repo, id).await;
                        latencies.push(nanos(began));
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, n * threads)
        }
        // Directory marks of a directory already marked: the served early-out.
        "served-mark-early-out" => {
            for &dir in own.iter() {
                ops::served_mark_dir(&state, &repo, dir).await;
            }
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, own) = (state.clone(), repo.clone(), own.clone());
                async move {
                    let mut latencies = Vec::with_capacity(n);
                    for _ in 0..n {
                        let began = Instant::now();
                        ops::served_mark_dir(&state, &repo, own[task]).await;
                        latencies.push(nanos(began));
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, n * threads)
        }
        // A directory mark through MARK_DEPTH clean levels, each level settled clean again
        // before the next mark, outside the timing.
        "served-mark-climb" => {
            let mut paths = Vec::with_capacity(threads);
            for (task, &top) in own.iter().enumerate() {
                let mut path = vec![top];
                for level in 0..MARK_DEPTH {
                    let name = format!("c{task}-{level}");
                    let parent = *path.last().expect("a level");
                    path.push(ops::add(&state, &repo, parent, ops::directory(&name), &name).await);
                }
                paths.push(path);
            }
            let paths = Arc::new(paths);
            let climbs = n.min(5_000);
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, paths) = (state.clone(), repo.clone(), paths.clone());
                async move {
                    let path = &paths[task];
                    let mut latencies = Vec::with_capacity(climbs);
                    for _ in 0..climbs {
                        untimed(async {
                            for &dir in path.iter().rev() {
                                ops::served_settle(&state, &repo, dir).await;
                            }
                        })
                        .await;
                        let began = Instant::now();
                        ops::served_mark_dir(&state, &repo, *path.last().expect("a leaf")).await;
                        latencies.push(nanos(began));
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, climbs * threads)
        }
        // Creates into a dirty directory, or with `-settled` into one settled clean before each,
        // outside the timing; each created file is detached again, outside the timing, so the
        // chain stays short.
        "served-create" | "served-create-settled" => {
            let settled = name == "served-create-settled";
            for &dir in own.iter() {
                ops::served_mark_dir(&state, &repo, dir).await;
            }
            let creates = n.min(20_000);
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, own) = (state.clone(), repo.clone(), own.clone());
                async move {
                    let dir = own[task];
                    let mut latencies = Vec::with_capacity(creates);
                    for i in 0..creates {
                        let name = untimed(async {
                            if settled {
                                let cleared = ops::served_settle(&state, &repo, dir).await;
                                assert!(cleared, "the directory settles");
                            }
                            format!("s{task}-{i}")
                        })
                        .await;
                        let began = Instant::now();
                        let created =
                            ops::served_create(&state, &repo, dir, ops::file(&name), &name).await;
                        latencies.push(nanos(began));
                        let created = created.expect("a fresh name is created");
                        untimed(ops::detach(&state, &repo, created)).await;
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, creates * threads)
        }
        // Copy-ups of files in each task's own directory.
        "served-copy-up" => {
            let mut files = Vec::with_capacity(threads);
            for (task, &dir) in own.iter().enumerate() {
                let mut ids = Vec::with_capacity(64);
                for i in 0..64 {
                    let name = format!("u{task}-{i}");
                    ids.push(ops::add(&state, &repo, dir, ops::file(&name), &name).await);
                }
                files.push(ids);
            }
            let files = Arc::new(files);
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, files, own) =
                    (state.clone(), repo.clone(), files.clone(), own.clone());
                async move {
                    let mut latencies = Vec::with_capacity(n);
                    for i in 0..n {
                        let began = Instant::now();
                        ops::served_copy_up(
                            &state,
                            &repo,
                            own[task],
                            files[task][i % 64],
                            i as u64,
                        )
                        .await;
                        latencies.push(nanos(began));
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, n * threads)
        }
        // Settles refused by a dirty child: a directory of 64 files whose oldest, the last a
        // walk of its chain reaches, stays dirty.
        "served-settle-dirty" => {
            for (task, &dir) in own.iter().enumerate() {
                let mut first = None;
                for i in 0..64 {
                    let name = format!("t{task}-{i}");
                    let added = ops::add(&state, &repo, dir, ops::file(&name), &name).await;
                    first.get_or_insert(added);
                }
                ops::mark_dirty(&state, &repo, first.expect("a file")).await;
            }
            let settles = n.min(20_000);
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, own) = (state.clone(), repo.clone(), own.clone());
                async move {
                    let mut latencies = Vec::with_capacity(settles);
                    for _ in 0..settles {
                        let began = Instant::now();
                        let settled = ops::served_settle(&state, &repo, own[task]).await;
                        latencies.push(nanos(began));
                        assert!(!settled, "a directory holding a dirty file stays dirty");
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, settles * threads)
        }
        // Settles of a directory of 64 clean files, marked dirty again outside the timing.
        // Variants: 16 of the 64 children directories, or 4096 file children.
        "served-settle" | "served-settle-subdirs" | "served-settle-wide" => {
            let (children, dirs) = match name {
                "served-settle-subdirs" => (64, 16),
                "served-settle-wide" => (4096, 0),
                _ => (64, 0),
            };
            for (task, &dir) in own.iter().enumerate() {
                for i in 0..children {
                    let name = format!("t{task}-{i}");
                    let node = if i < dirs {
                        ops::directory(&name)
                    } else {
                        ops::file(&name)
                    };
                    ops::add(&state, &repo, dir, node, &name).await;
                }
            }
            let settles = if children > 64 {
                n.min(1_000)
            } else {
                n.min(20_000)
            };
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, own) = (state.clone(), repo.clone(), own.clone());
                async move {
                    let mut latencies = Vec::with_capacity(settles);
                    for _ in 0..settles {
                        untimed(ops::served_mark_dir(&state, &repo, own[task])).await;
                        let began = Instant::now();
                        let settled = ops::served_settle(&state, &repo, own[task]).await;
                        latencies.push(nanos(began));
                        assert!(settled, "a directory of clean files settles");
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, settles * threads)
        }
        // RMDIRs of empty directories made outside the timing; with `-settled`, their directory
        // settled clean before each, outside the timing.
        "served-rmdir" | "served-rmdir-settled" => {
            let settled = name == "served-rmdir-settled";
            let rmdirs = n.min(20_000);
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, own) = (state.clone(), repo.clone(), own.clone());
                async move {
                    let mut latencies = Vec::with_capacity(rmdirs);
                    for i in 0..rmdirs {
                        let dir = untimed(async {
                            let name = format!("e{task}-{i}");
                            let dir =
                                ops::add(&state, &repo, own[task], ops::directory(&name), &name)
                                    .await;
                            if settled {
                                let cleared = ops::served_settle(&state, &repo, own[task]).await;
                                assert!(cleared, "the directory settles");
                            }
                            dir
                        })
                        .await;
                        let began = Instant::now();
                        let removed = ops::served_rmdir(&state, &repo, dir).await;
                        latencies.push(nanos(began));
                        assert!(removed, "an empty directory is removed");
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, rmdirs * threads)
        }
        // Renames of a directory onto an empty directory, both made outside the timing; with
        // `-settled`, their directory settled clean before each, outside the timing.
        "served-rename-over" | "served-rename-over-settled" => {
            let settled = name == "served-rename-over-settled";
            let renames = n.min(20_000);
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, own) = (state.clone(), repo.clone(), own.clone());
                async move {
                    let dir = own[task];
                    let mut latencies = Vec::with_capacity(renames);
                    for i in 0..renames {
                        let (moved, holder, target) = untimed(async {
                            let source = format!("src{task}-{i}");
                            let target = format!("dst{task}-{i}");
                            let made = (
                                ops::add(&state, &repo, dir, ops::directory(&source), &source)
                                    .await,
                                ops::add(&state, &repo, dir, ops::directory(&target), &target)
                                    .await,
                                target,
                            );
                            if settled {
                                let cleared = ops::served_settle(&state, &repo, dir).await;
                                assert!(cleared, "the directory settles");
                            }
                            made
                        })
                        .await;
                        let began = Instant::now();
                        let renamed =
                            ops::served_rename_over(&state, &repo, moved, dir, &target, holder)
                                .await;
                        latencies.push(nanos(began));
                        assert!(renamed, "the rename lands");
                        untimed(ops::detach(&state, &repo, moved)).await;
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, renames * threads)
        }
        // Unlinks of files made outside the timing. With `-settled`, every directory above the
        // file, one or with `-settled-deep` `--depth` of them, is settled clean before each,
        // outside the timing, so the unlink's marks climb through them.
        "served-unlink" | "served-unlink-settled" | "served-unlink-settled-deep" => {
            let levels = match name {
                "served-unlink" => 0,
                "served-unlink-settled" => 1,
                _ => options.depth,
            };
            let mut paths = Vec::with_capacity(threads);
            for (task, &top) in own.iter().enumerate() {
                let mut path = vec![top];
                for level in 1..levels {
                    let name = format!("w{task}-{level}");
                    let parent = *path.last().expect("a level");
                    path.push(ops::add(&state, &repo, parent, ops::directory(&name), &name).await);
                }
                paths.push(path);
            }
            let paths = Arc::new(paths);
            let unlinks = n.min(20_000);
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, paths) = (state.clone(), repo.clone(), paths.clone());
                async move {
                    let path = &paths[task];
                    let dir = *path.last().expect("a directory");
                    let mut latencies = Vec::with_capacity(unlinks);
                    for i in 0..unlinks {
                        let file = untimed(async {
                            let name = format!("g{task}-{i}");
                            let file = ops::add(&state, &repo, dir, ops::file(&name), &name).await;
                            if levels > 0 {
                                for &level in path.iter().rev() {
                                    let cleared = ops::served_settle(&state, &repo, level).await;
                                    assert!(cleared, "every directory above the file settles");
                                }
                            }
                            file
                        })
                        .await;
                        let began = Instant::now();
                        ops::served_unlink(&state, &repo, file).await;
                        latencies.push(nanos(began));
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, unlinks * threads)
        }
        // Renames of a file back and forth between two directories of each task's own, with the
        // marks the served provider makes; with `-settled`, both directories settled clean before
        // each, outside the timing.
        "served-rename" | "served-rename-settled" => {
            let settled = name == "served-rename-settled";
            let mut pairs = Vec::with_capacity(threads);
            for task in 0..threads {
                let a_name = format!("ra{task}");
                let b_name = format!("rb{task}");
                let a = ops::add(&state, &repo, ROOT_NODE, ops::directory(&a_name), &a_name).await;
                let b = ops::add(&state, &repo, ROOT_NODE, ops::directory(&b_name), &b_name).await;
                for i in 0..32 {
                    let name = format!("filler{i}");
                    ops::add(&state, &repo, a, ops::file(&name), &name).await;
                    ops::add(&state, &repo, b, ops::file(&name), &name).await;
                }
                let moved = ops::add(&state, &repo, a, ops::file("moved"), "moved").await;
                pairs.push((a, b, moved));
            }
            let pairs = Arc::new(pairs);
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, pairs) = (state.clone(), repo.clone(), pairs.clone());
                async move {
                    let (a, b, moved) = pairs[task];
                    let mut latencies = Vec::with_capacity(n);
                    for i in 0..n {
                        let to = if i % 2 == 0 { b } else { a };
                        if settled {
                            untimed(async {
                                for dir in [a, b] {
                                    let cleared = ops::served_settle(&state, &repo, dir).await;
                                    assert!(cleared, "both directories settle");
                                }
                            })
                            .await;
                        }
                        let began = Instant::now();
                        let ok = ops::served_rename(&state, &repo, moved, to, "moved").await;
                        latencies.push(nanos(began));
                        assert!(ok, "the rename lands");
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, n * threads)
        }
        // A mount's adds of local files into new local directories, MOUNT_NEW_FILES in each, as a
        // VFS provider's mount would add them: each file added with its local-content mark.
        "mount-new-dir" => {
            let names: Arc<Vec<(String, u64)>> = Arc::new(
                (0..MOUNT_NEW_FILES)
                    .map(|f| {
                        let name = format!("n{f}");
                        let hash = lore_storage::hash::hash_string(&name);
                        (name, hash)
                    })
                    .collect(),
            );
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, own, names) =
                    (state.clone(), repo.clone(), own.clone(), names.clone());
                async move {
                    let mut dir = 0;
                    let mut latencies = Vec::with_capacity(n);
                    for i in 0..n {
                        if i % MOUNT_NEW_FILES == 0 {
                            dir = untimed(async {
                                let name = format!("new{}", i / MOUNT_NEW_FILES);
                                ops::mount_add_dir(&state, &repo, own[task], &name).await
                            })
                            .await;
                        }
                        let (name, hash) = &names[i % MOUNT_NEW_FILES];
                        let began = Instant::now();
                        ops::mount_add(&state, &repo, dir, (name, *hash)).await;
                        latencies.push(nanos(began));
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, n * threads)
        }
        // A mount's pass over tracked directories of MOUNT_TRACKED_FILES files for
        // MOUNT_LOCAL_FILES local files each, a tenth of them named as tracked files: one
        // operation is one directory's pass, its index read included.
        "mount-tracked-dir" => {
            let mounts = n.min(16);
            let mut dirs = Vec::with_capacity(threads);
            for (task, &top) in own.iter().enumerate() {
                let mut ids = Vec::with_capacity(mounts);
                for k in 0..mounts {
                    let name = format!("tracked{task}-{k}");
                    let dir = ops::add(&state, &repo, top, ops::directory(&name), &name).await;
                    for f in 0..MOUNT_TRACKED_FILES {
                        let name = format!("t{f}");
                        ops::add(&state, &repo, dir, ops::file(&name), &name).await;
                    }
                    ids.push(dir);
                }
                dirs.push(ids);
            }
            let dirs = Arc::new(dirs);
            let entries: Arc<Vec<(String, u64)>> = Arc::new(
                (0..MOUNT_LOCAL_FILES)
                    .map(|f| {
                        let name = if f % 10 == 0 {
                            format!("t{}", f * 10)
                        } else {
                            format!("l{f}")
                        };
                        let hash = lore_storage::hash::hash_string(&name);
                        (name, hash)
                    })
                    .collect(),
            );
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, dirs, entries) =
                    (state.clone(), repo.clone(), dirs.clone(), entries.clone());
                async move {
                    let mut latencies = Vec::with_capacity(mounts);
                    for &dir in &dirs[task] {
                        let began = Instant::now();
                        ops::mount_tracked(&state, &repo, dir, &entries).await;
                        latencies.push(nanos(began));
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, mounts * threads)
        }
        // A swap's copy of a directory of COPY_FILES files, which must stand clean once copied;
        // each copy is detached again outside the timing, so the directory it lands in stays short.
        "swap-copy" => {
            let copies = n.min(20_000);
            let children: Arc<Vec<String>> =
                Arc::new((0..COPY_FILES).map(|f| format!("k{f}")).collect());
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, own, children) =
                    (state.clone(), repo.clone(), own.clone(), children.clone());
                async move {
                    let dir = own[task];
                    let mut latencies = Vec::with_capacity(copies);
                    for i in 0..copies {
                        let name = untimed(async { format!("copy{task}-{i}") }).await;
                        let began = Instant::now();
                        let copied = ops::swap_copy(&state, &repo, dir, &name, &children).await;
                        latencies.push(nanos(began));
                        untimed(async {
                            assert!(
                                ops::is_clean(&state, &repo, copied).await,
                                "a swap's copy stands clean"
                            );
                            ops::detach(&state, &repo, copied).await;
                        })
                        .await;
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, copies * threads)
        }
        // A swap's copy of one file, as a swap copies every file it adds; each copy is detached
        // again outside the timing, so the directory it lands in stays short.
        "swap-copy-file" => {
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, own) = (state.clone(), repo.clone(), own.clone());
                async move {
                    let dir = own[task];
                    let mut latencies = Vec::with_capacity(n);
                    for i in 0..n {
                        let name = untimed(async { format!("copy{task}-{i}") }).await;
                        let began = Instant::now();
                        let copied = ops::swap_copy_file(&state, &repo, dir, &name).await;
                        latencies.push(nanos(began));
                        untimed(ops::detach(&state, &repo, copied)).await;
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, n * threads)
        }
        // A swap's copy of a directory of N files: each child added into the directory the copy
        // builds out of every chain, which is published once filled, outside the timing.
        "copy-subtree" => {
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, own) = (state.clone(), repo.clone(), own.clone());
                async move {
                    let mut build = ops::copy_root(&state, &repo, own[task], "copied").await;
                    let mut latencies = Vec::with_capacity(n);
                    for i in 0..n {
                        let name = format!("k{i}");
                        let began = Instant::now();
                        ops::copy_child(&mut build, &name).await;
                        latencies.push(nanos(began));
                    }
                    untimed(ops::copy_publish(build)).await;
                    latencies
                }
            })
            .await;
            (e, l, w, n * threads)
        }
        // Discards of files, newest first, in one batched free per task, as a commit's post-pass
        // frees the files it collected; each file is an operation.
        "discard-files" => {
            let mut added = Vec::with_capacity(threads);
            for (task, &dir) in own.iter().enumerate() {
                let mut ids = Vec::with_capacity(n);
                for i in 0..n {
                    let name = format!("x{task}-{i}");
                    ids.push(ops::add(&state, &repo, dir, ops::file(&name), &name).await);
                }
                // Newest first, as the free takes them.
                ids.reverse();
                added.push(ids);
            }
            let added = Arc::new(added);
            let state = if options.cold {
                ops::cold(&state, &repo).await
            } else {
                state.clone()
            };
            ran_on = state.clone();
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, added) = (state.clone(), repo.clone(), added.clone());
                async move {
                    let began = Instant::now();
                    ops::discard_batch(&state, &repo, &added[task]).await;
                    let each = nanos(began) / u32::try_from(n.max(1)).unwrap_or(u32::MAX);
                    // The per-operation timing every other workload runs, which the instruction
                    // gate subtracts as `loop-null`: run here too, so the subtraction cancels.
                    let mut latencies = Vec::with_capacity(n);
                    for _ in 0..n {
                        let began = Instant::now();
                        latencies.push(nanos(began));
                    }
                    latencies.fill(each);
                    latencies
                }
            })
            .await;
            (e, l, w, n * threads)
        }
        // Discards of directories of DISCARD_FILES files each, the whole subtree in one pass,
        // newest first.
        // `discard-dirs-spread` adds the files round-robin over all the directories, so each
        // directory's children lie far apart, in many blocks.
        "discard-dirs" | "discard-dirs-spread" => {
            let spread = name == "discard-dirs-spread";
            let dirs_per_task = (n / DISCARD_FILES).max(1);
            let mut added = Vec::with_capacity(threads);
            for (task, &parent) in own.iter().enumerate() {
                let mut ids = Vec::with_capacity(dirs_per_task);
                for i in 0..dirs_per_task {
                    let name = format!("y{task}-{i}");
                    let dir = ops::add(&state, &repo, parent, ops::directory(&name), &name).await;
                    if !spread {
                        for f in 0..DISCARD_FILES {
                            let name = format!("z{f}");
                            ops::add(&state, &repo, dir, ops::file(&name), &name).await;
                        }
                    }
                    ids.push(dir);
                }
                if spread {
                    for f in 0..DISCARD_FILES {
                        let name = format!("z{f}");
                        for &dir in &ids {
                            ops::add(&state, &repo, dir, ops::file(&name), &name).await;
                        }
                    }
                }
                added.push(ids);
            }
            let added = Arc::new(added);
            let state = if options.cold {
                ops::cold(&state, &repo).await
            } else {
                state.clone()
            };
            ran_on = state.clone();
            let (e, l, w) = timed(&state, &repo, threads, readers, shared, |task| {
                let (state, repo, added) = (state.clone(), repo.clone(), added.clone());
                async move {
                    let mut latencies = Vec::with_capacity(dirs_per_task);
                    for &id in added[task].iter().rev() {
                        let began = Instant::now();
                        ops::discard(&state, &repo, id).await;
                        latencies.push(nanos(began));
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, dirs_per_task * threads)
        }
        // Sweeps after one detach each, one task, as the stage's or a commit's sweep meets a
        // session's removals: each scans the one block the detach flagged and frees the file.
        "sweep-detached" => {
            let dir = ops::add(&state, &repo, ROOT_NODE, ops::directory("sw"), "sw").await;
            let serial = n.min(20_000);
            let mut files = Vec::with_capacity(serial);
            for i in 0..serial {
                let name = format!("s{i}");
                files.push(ops::add(&state, &repo, dir, ops::file(&name), &name).await);
            }
            // The grabs flagged every block; this sweep takes their flags.
            ops::sweep(&state, &repo).await;
            let files = Arc::new(files);
            // No readers: a sweep runs with nothing beside it.
            let (e, l, w) = timed(&state, &repo, 1, 0, shared, |_| {
                let (state, repo, files) = (state.clone(), repo.clone(), files.clone());
                async move {
                    let mut latencies = Vec::with_capacity(files.len());
                    // Newest first, each the chain's head, so its detach walks nothing.
                    for &file in files.iter().rev() {
                        untimed(ops::detach(&state, &repo, file)).await;
                        let began = Instant::now();
                        ops::sweep(&state, &repo).await;
                        latencies.push(nanos(began));
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, serial)
        }
        // Sweeps of a block just loaded each, one task, as a committing session's sweep scans
        // each block it loaded once: nothing detached to free, the scan alone.
        "sweep-loaded" => {
            let dir = ops::add(&state, &repo, ROOT_NODE, ops::directory("sl"), "sl").await;
            for i in 0..n * SWEEP_LOADED_FILES {
                let name = format!("l{i}");
                ops::add(&state, &repo, dir, ops::file(&name), &name).await;
            }
            let cold = ops::cold(&state, &repo).await;
            // Block 0 reads the tree, which gives the block count; its sweep is setup.
            ops::load_block(&cold, &repo, 0).await;
            ops::sweep(&cold, &repo).await;
            let blocks = cold.block_count();
            ran_on = cold.clone();
            // No readers: a sweep runs with nothing beside it.
            let (e, l, w) = timed(&cold, &repo, 1, 0, shared, |_| {
                let (cold, repo) = (cold.clone(), repo.clone());
                async move {
                    let mut latencies = Vec::with_capacity(blocks);
                    for index in 1..blocks {
                        untimed(ops::load_block(&cold, &repo, index)).await;
                        let began = Instant::now();
                        ops::sweep(&cold, &repo).await;
                        latencies.push(nanos(began));
                    }
                    latencies
                }
            })
            .await;
            (e, l, w, blocks - 1)
        }
        other => usage(&format!("unknown workload {other}")),
    };
    Outcome {
        ops,
        elapsed,
        latencies,
        retries: ops::retries(&ran_on),
        walks,
        brackets: BRACKETS.swap(0, Ordering::Relaxed),
    }
}
