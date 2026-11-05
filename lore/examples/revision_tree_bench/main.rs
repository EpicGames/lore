// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Cost of the C API's revision-tree node operations, for an A/B of two revisions of `State`'s
//! node interface. Built only on demand: `cargo run --release -p lore --example
//! revision_tree_bench --features revision-tree-bench -- [options]`.
//!
//! Each workload loads a fresh revision tree over an in-memory store and drives one C API entry
//! point through its Rust form, one call at a time on a `current_thread` runtime, timing each
//! call. A workload prints one line:
//!
//! ```text
//! BENCH name=<workload> ops=<n> secs=<s> ops_per_sec=<x> p50_ns=<a> p99_ns=<b> brackets=<m>
//! ```
//!
//! Options: `--ops N` (calls per workload, default 2000), `--only a,b` (workloads by name,
//! default all), `--list` (print the workload names, one per line, and end), `--perf-ctl <fifo>
//! --perf-ack <fifo>` (enable a `perf stat --control=fifo:<ctl>,<ack> -D -1` only across each
//! workload's calls, its setup between them left out).

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use lore::interface::LoreEventCallback;
use lore::interface::LoreGlobalArgs;
use lore::revision_tree::add::LoreRevisionTreeAddArgs;
use lore::revision_tree::add::LoreRevisionTreeAddEntry;
use lore::revision_tree::add::add;
use lore::revision_tree::commit::LoreRevisionTreeCommitArgs;
use lore::revision_tree::commit::LoreRevisionTreeCommitOptions;
use lore::revision_tree::commit::commit;
use lore::revision_tree::delete::LoreRevisionTreeDeleteArgs;
use lore::revision_tree::delete::LoreRevisionTreeDeleteEntry;
use lore::revision_tree::delete::delete;
use lore::revision_tree::handle::LoreRevisionTree;
use lore::revision_tree::load::LoreRevisionTreeLoadArgs;
use lore::revision_tree::load::load;
use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetArgs;
use lore::revision_tree::metadata_set::LoreRevisionTreeMetadataSetEntry;
use lore::revision_tree::metadata_set::metadata_set;
use lore::revision_tree::modify::LoreRevisionTreeModifyArgs;
use lore::revision_tree::modify::LoreRevisionTreeModifyEntry;
use lore::revision_tree::modify::modify;
use lore::revision_tree::move_node::LoreRevisionTreeMoveArgs;
use lore::revision_tree::move_node::LoreRevisionTreeMoveEntry;
use lore::revision_tree::move_node::move_node;
use lore::storage::handle as storage_handle;
use lore::storage::store::in_memory_for_tests;
use lore_base::test_util::TempDir;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreMetadata;
use lore_revision::interface::LoreNodeType;
use lore_revision::interface::LoreString;
use lore_revision::metadata::BRANCH;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;

const WORKLOADS: &[&str] = &[
    "loop-null",
    "bracket-null",
    "capi-add",
    "capi-move",
    "capi-modify",
    "capi-delete-added",
    "capi-delete-tracked",
    "capi-commit",
    "capi-delete-readd-commit",
];

/// The one workload a gate build carries, from `STATE_BENCH_ONLY` at build time. Every other
/// workload's code is then left out, so it cannot change how the carried one compiles.
const ONLY: Option<&str> = option_env!("STATE_BENCH_ONLY");

/// Files each add call adds, and each deleted or committed directory holds.
const BATCH: usize = 16;

/// The `perf stat` control and acknowledgement FIFOs, when given.
static PERF: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();

thread_local! {
    /// The FIFOs, opened at the first command. Every command comes from the one runtime thread.
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

/// The brackets [`untimed`] made in the current workload, for subtracting their cost.
static BRACKETS: AtomicU64 = AtomicU64::new(0);

/// Runs `work`, a workload's setup between its calls, with the instruction count stopped.
async fn untimed<F: std::future::Future>(work: F) -> F::Output {
    perf(b"disable\n");
    let output = work.await;
    BRACKETS.fetch_add(1, Ordering::Relaxed);
    perf(b"enable\n");
    output
}

struct Options {
    ops: usize,
    only: Option<Vec<String>>,
}

fn parse() -> Options {
    let mut options = Options {
        ops: 2000,
        only: None,
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
            "--ops" => options.ops = value().parse().unwrap_or_else(|_| usage("bad --ops")),
            "--only" => {
                let names: Vec<String> = value().split(',').map(str::to_string).collect();
                for name in &names {
                    if !WORKLOADS.contains(&name.as_str()) {
                        usage(&format!("unknown workload {name}"));
                    }
                }
                options.only = Some(names);
            }
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
    options
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
    eprintln!("revision_tree_bench: {error}");
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
    // Before the sandbox, which `--list` and a usage error would otherwise leave behind.
    let options = parse();
    let sandbox = TempDir::new("lore-capi-bench-");
    // Safety: no other thread exists yet, so nothing reads the environment meanwhile. As the unit
    // tests do, the bench keeps away from a running service and the user's settings.
    unsafe {
        std::env::set_var("LORE_USE_SERVICE", "0");
        std::env::set_var("LORE_GLOBAL_PATH", sandbox.path());
        std::env::set_var("LORE_AUTH_PATH", sandbox.path());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("the runtime starts");
    for &name in WORKLOADS {
        if ONLY.is_some_and(|only| only != name)
            || options
                .only
                .as_ref()
                .is_some_and(|only| !only.iter().any(|o| o == name))
        {
            continue;
        }
        let outcome = runtime.block_on(run(name, options.ops));
        outcome.print(name);
    }
}

struct Outcome {
    ops: usize,
    elapsed: Duration,
    latencies: Vec<u32>,
    brackets: u64,
}

impl Outcome {
    fn print(mut self, name: &str) {
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
            "BENCH name={name} ops={} secs={secs:.4} ops_per_sec={:.0} p50_ns={} p99_ns={} \
             brackets={}",
            self.ops,
            self.ops as f64 / secs,
            at(0.50),
            at(0.99),
            self.brackets,
        );
    }
}

/// The events of one call.
type Events = Arc<Mutex<Vec<LoreEvent>>>;

fn callback(events: &Events) -> LoreEventCallback {
    let events = events.clone();
    Some(Box::new(move |event: &LoreEvent| {
        events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event.clone());
    }))
}

fn taken(events: &Events) -> Vec<LoreEvent> {
    std::mem::take(
        &mut *events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

/// A revision tree loaded over a fresh in-memory store, on a branch of its own so it commits.
async fn tree(label: &str) -> LoreRevisionTree {
    let store = in_memory_for_tests(label).await;
    let store_handle = storage_handle::register(store);
    let events = Events::default();
    let status = load(
        LoreGlobalArgs::default(),
        LoreRevisionTreeLoadArgs {
            store: store_handle,
            repository: Partition::from(uuid::Uuid::now_v7()),
            revision_hash: Hash::default(),
        },
        callback(&events),
    )
    .await;
    assert_eq!(status, 0, "the load succeeds");
    let handle_id = taken(&events)
        .iter()
        .find_map(|event| match event {
            LoreEvent::RevisionTreeLoaded(data) => Some(data.handle_id),
            _ => None,
        })
        .expect("the load reports its handle");
    let tree = LoreRevisionTree { handle_id };
    let status = metadata_set(
        LoreGlobalArgs::default(),
        LoreRevisionTreeMetadataSetArgs {
            batch_id: 1,
            handle: tree,
            entries: LoreArray::from_vec(vec![LoreRevisionTreeMetadataSetEntry {
                entry_id: 1,
                key: LoreString::from_str(BRANCH),
                value: LoreMetadata::Context(Context::from(uuid::Uuid::now_v7())),
            }]),
        },
        callback(&events),
    )
    .await;
    assert_eq!(status, 0, "the branch is set");
    tree
}

fn entry(
    entry_id: u64,
    parent: NodeID,
    name: &str,
    kind: LoreNodeType,
) -> LoreRevisionTreeAddEntry {
    LoreRevisionTreeAddEntry {
        entry_id,
        parent_node_id: parent,
        parent_entry_index: 0,
        name: LoreString::from_str(name),
        kind: kind as u32,
        mode: 0o644,
        size: 10,
        address: Address {
            hash: Hash::from_u64(entry_id + 1),
            context: Context::from(uuid::Uuid::now_v7()),
        },
    }
}

/// One add call; the node each entry produced, in entry order.
async fn add_batch(tree: LoreRevisionTree, entries: Vec<LoreRevisionTreeAddEntry>) -> Vec<NodeID> {
    let events = Events::default();
    let status = add(
        LoreGlobalArgs::default(),
        LoreRevisionTreeAddArgs {
            batch_id: 1,
            handle: tree,
            entries: LoreArray::from_vec(entries),
        },
        callback(&events),
    )
    .await;
    assert_eq!(status, 0, "the add succeeds");
    taken(&events)
        .iter()
        .filter_map(|event| match event {
            LoreEvent::RevisionTreeAddComplete(data) => Some(data.node_id),
            _ => None,
        })
        .collect()
}

/// A directory of BATCH files under `parent`; the directory.
async fn directory_of_files(tree: LoreRevisionTree, parent: NodeID, name: &str) -> NodeID {
    let dir = add_batch(tree, vec![entry(0, parent, name, LoreNodeType::Directory)]).await[0];
    let files = (0..BATCH)
        .map(|i| entry(i as u64 + 1, dir, &format!("f{i}"), LoreNodeType::File))
        .collect();
    add_batch(tree, files).await;
    dir
}

async fn delete_one(tree: LoreRevisionTree, node: NodeID) {
    let events = Events::default();
    let status = delete(
        LoreGlobalArgs::default(),
        LoreRevisionTreeDeleteArgs {
            batch_id: 1,
            handle: tree,
            entries: LoreArray::from_vec(vec![LoreRevisionTreeDeleteEntry {
                entry_id: 1,
                node_id: node,
            }]),
        },
        callback(&events),
    )
    .await;
    assert_eq!(status, 0, "the delete succeeds");
}

async fn move_one(tree: LoreRevisionTree, node: NodeID, parent: NodeID, name: &str) {
    let events = Events::default();
    let status = move_node(
        LoreGlobalArgs::default(),
        LoreRevisionTreeMoveArgs {
            batch_id: 1,
            handle: tree,
            entries: LoreArray::from_vec(vec![LoreRevisionTreeMoveEntry {
                entry_id: 1,
                node_id: node,
                destination_parent_id: parent,
                dst_name: LoreString::from_str(name),
            }]),
        },
        callback(&events),
    )
    .await;
    assert_eq!(status, 0, "the move succeeds");
}

async fn modify_one(tree: LoreRevisionTree, node: NodeID, size: u64) {
    let events = Events::default();
    let status = modify(
        LoreGlobalArgs::default(),
        LoreRevisionTreeModifyArgs {
            batch_id: 1,
            handle: tree,
            entries: LoreArray::from_vec(vec![LoreRevisionTreeModifyEntry {
                entry_id: 1,
                node_id: node,
                mode: 0o644,
                size,
                address: Address {
                    hash: Hash::from_u64(size + 1),
                    context: Context::default(),
                },
            }]),
        },
        callback(&events),
    )
    .await;
    assert_eq!(status, 0, "the modify succeeds");
}

async fn commit_tree(tree: LoreRevisionTree) {
    let events = Events::default();
    let status = commit(
        LoreGlobalArgs::default(),
        LoreRevisionTreeCommitArgs {
            id: 1,
            handle: tree,
            options: LoreRevisionTreeCommitOptions { remote_write: 0 },
        },
        callback(&events),
    )
    .await;
    assert_eq!(status, 0, "the commit succeeds");
}

fn nanos(began: Instant) -> u32 {
    u32::try_from(began.elapsed().as_nanos()).unwrap_or(u32::MAX)
}

async fn run(name: &str, n: usize) -> Outcome {
    let name = ONLY.unwrap_or(name);
    let tree = tree(name).await;
    let mut latencies = Vec::with_capacity(n);
    let mut ops = n;
    perf(b"enable\n");
    let began = Instant::now();
    match name {
        // The timing loop alone, and the loop with one empty bracket per call: the costs the
        // other workloads' counts carry beside their calls.
        "loop-null" | "bracket-null" => {
            for _ in 0..n {
                if name == "bracket-null" {
                    untimed(async {}).await;
                }
                let began = Instant::now();
                latencies.push(nanos(began));
            }
        }
        // Adds of BATCH files per call, into directories made outside the count.
        "capi-add" => {
            for i in 0..n {
                let (dir, files) = untimed(async {
                    let dir = add_batch(
                        tree,
                        vec![entry(
                            0,
                            ROOT_NODE,
                            &format!("d{i}"),
                            LoreNodeType::Directory,
                        )],
                    )
                    .await[0];
                    let files: Vec<_> = (0..BATCH)
                        .map(|f| entry(f as u64 + 1, dir, &format!("f{f}"), LoreNodeType::File))
                        .collect();
                    (dir, files)
                })
                .await;
                let began = Instant::now();
                add_batch(tree, files).await;
                latencies.push(nanos(began));
                let _ = dir;
            }
        }
        // One file moved back and forth between two directories of BATCH files each.
        "capi-move" => {
            let (a, b, moved) = untimed(async {
                let a = directory_of_files(tree, ROOT_NODE, "a").await;
                let b = directory_of_files(tree, ROOT_NODE, "b").await;
                let moved =
                    add_batch(tree, vec![entry(100, a, "moved", LoreNodeType::File)]).await[0];
                (a, b, moved)
            })
            .await;
            for i in 0..n {
                let target = if i % 2 == 0 { b } else { a };
                let began = Instant::now();
                move_one(tree, moved, target, "moved").await;
                latencies.push(nanos(began));
            }
        }
        // Content rewrites of one of BATCH files, cycled, in a directory made outside the count.
        "capi-modify" => {
            let files = untimed(async {
                let dir = add_batch(
                    tree,
                    vec![entry(0, ROOT_NODE, "m", LoreNodeType::Directory)],
                )
                .await[0];
                let entries: Vec<_> = (0..BATCH)
                    .map(|f| entry(f as u64 + 1, dir, &format!("f{f}"), LoreNodeType::File))
                    .collect();
                add_batch(tree, entries).await
            })
            .await;
            for i in 0..n {
                let began = Instant::now();
                modify_one(tree, files[i % files.len()], i as u64).await;
                latencies.push(nanos(began));
            }
        }
        // Deletes of an added directory of BATCH files, made outside the count.
        "capi-delete-added" => {
            for i in 0..n {
                let dir = untimed(directory_of_files(tree, ROOT_NODE, &format!("x{i}"))).await;
                let began = Instant::now();
                delete_one(tree, dir).await;
                latencies.push(nanos(began));
            }
        }
        // Deletes of a committed directory of BATCH files, made and committed outside the count.
        "capi-delete-tracked" => {
            ops = n.min(500);
            let dirs = untimed(async {
                let mut dirs = Vec::with_capacity(ops);
                for i in 0..ops {
                    dirs.push(directory_of_files(tree, ROOT_NODE, &format!("y{i}")).await);
                }
                commit_tree(tree).await;
                dirs
            })
            .await;
            for dir in dirs {
                let began = Instant::now();
                delete_one(tree, dir).await;
                latencies.push(nanos(began));
            }
        }
        // Commits of a directory of BATCH files added, outside the count, since the last one.
        "capi-commit" => {
            ops = n.min(200);
            for i in 0..ops {
                untimed(directory_of_files(tree, ROOT_NODE, &format!("z{i}"))).await;
                let began = Instant::now();
                commit_tree(tree).await;
                latencies.push(nanos(began));
            }
        }
        // A delete of an added directory of BATCH files, its re-add under the same names, and the
        // commit that follows, which writes whatever blocks the re-add took.
        "capi-delete-readd-commit" => {
            ops = n.min(200);
            for i in 0..ops {
                let name = untimed(async { format!("w{i}") }).await;
                let dir = untimed(directory_of_files(tree, ROOT_NODE, &name)).await;
                let began = Instant::now();
                delete_one(tree, dir).await;
                directory_of_files(tree, ROOT_NODE, &name).await;
                commit_tree(tree).await;
                latencies.push(nanos(began));
            }
        }
        other => usage(&format!("unknown workload {other}")),
    }
    let elapsed = began.elapsed();
    perf(b"disable\n");
    Outcome {
        ops,
        elapsed,
        latencies,
        brackets: BRACKETS.swap(0, Ordering::Relaxed),
    }
}
