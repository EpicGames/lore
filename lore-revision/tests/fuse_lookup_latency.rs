// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Warm LOOKUP latency through a real mount while the tree changes, under writers or back-to-back
//! syncs, held to its budgets on a quiet machine: the harness `lore-fuse/bench/m2/latency.sh`
//! runs.
//!
//! A process of its own: a measured window switches process-wide counters on and off and reads
//! their process totals, which another test would add to, or lose counts of its own to; and the
//! budgets mean something only with nothing else running.
#![cfg(target_os = "linux")]

use std::collections::HashSet;
use std::ffi::CString;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use lore_base::test_util::TempDir;
use lore_fuse::abi::Opcode;
use lore_fuse::init::InitConfig;
use lore_fuse::metrics::Histogram;
use lore_fuse::metrics::Metrics;
use lore_fuse::mount::MountOptions;
use lore_fuse::mount::unmount_path;
use lore_fuse::session::Session;
use lore_fuse::session::SessionConfig;
use lore_fuse::skip_without_fuse;
use lore_fuse::transport::device::DeviceTransport;
use lore_revision::filter::FilterMode;
use lore_revision::fs::filesystem_provider::InstanceOperationImpl;
use lore_revision::fs::filesystem_provider::StaticDispatchInstanceOperation;
use lore_revision::fs::fuse::attr::AttrContext;
use lore_revision::fs::fuse::filesystem::FuseFilesystem;
use lore_revision::fs::fuse::filesystem::WriteStep;
use lore_revision::fs::fuse::inode_number::Identity;
use lore_revision::fs::fuse::inode_number::IdentityKind;
use lore_revision::fs::fuse::inode_number::derive_inode_number;
use lore_revision::fs::fuse::operation::FuseOperation;
use lore_revision::fs::fuse::swap::swap_mounted;
use lore_revision::lore::RepositoryId;
use lore_revision::node::NodeID;
use lore_revision::node::read_contention;
use lore_revision::repository::RepositoryContext;
use lore_revision::state;
use lore_revision::state::State;

#[path = "support/bench_stats.rs"]
mod bench_stats;
#[path = "support/fuse_fixture.rs"]
mod fuse_fixture;
#[path = "helper.rs"]
mod helper;

/// Attributes owned by the user running the test, as a mount's are: the kernel checks a write
/// through a read-write mount against them.
fn attr_context() -> AttrContext {
    AttrContext::for_mounting_user(1 << 17, 1_700_000_000_000)
}

/// How long a wait on the writers, the syncs or the measured LOOKUPs may take.
const WAIT_LIMIT: Duration = Duration::from_secs(60);

/// Waits until `count` reaches `at_least`, failing once `stopped` answers true or [`WAIT_LIMIT`]
/// passes. Call it before the work is asked to stop.
async fn wait_for_first_changes(count: &AtomicU64, at_least: u64, stopped: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT_LIMIT;
    while count.load(Ordering::SeqCst) < at_least {
        assert!(!stopped(), "the work stopped before its first changes");
        assert!(
            Instant::now() < deadline,
            "{} of {at_least} first changes in {WAIT_LIMIT:?}",
            count.load(Ordering::SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// Joins `threads` once every one has finished, failing where [`WAIT_LIMIT`] passes first.
async fn join_within_limit<T>(threads: Vec<std::thread::JoinHandle<T>>) -> Vec<T> {
    let deadline = Instant::now() + WAIT_LIMIT;
    while !threads.iter().all(std::thread::JoinHandle::is_finished) {
        assert!(
            Instant::now() < deadline,
            "threads still run after {WAIT_LIMIT:?}"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    threads
        .into_iter()
        .map(|thread| thread.join().expect("a thread"))
        .collect()
}

/// Unmounts on drop, lazily if a plain unmount is refused.
struct UnmountGuard(PathBuf);

impl Drop for UnmountGuard {
    fn drop(&mut self) {
        if unmount_path(&self.0, false).is_err() {
            let _ = unmount_path(&self.0, true);
        }
    }
}

struct Mounted {
    // Field order is drop order: the session stops before the unmount.
    session: Arc<Session<FuseFilesystem, DeviceTransport>>,
    _guard: UnmountGuard,
    root: PathBuf,
    _dir: TempDir,
}

/// Mounts `fs` read-only with `workers` session workers.
async fn mount(fs: Arc<FuseFilesystem>, workers: usize) -> Mounted {
    let dir = TempDir::new("lore-fuse-lookup-latency-");
    let options = MountOptions {
        fsname: "lore:lookup-latency".to_string(),
        max_read: 1 << 17,
        fusermount: None,
    };
    let mount = Arc::new(lore_fuse::mount::mount(dir.path(), &options).expect("mount"));
    let guard = UnmountGuard(dir.path().to_path_buf());
    let session = Session::start(
        fs.clone(),
        Arc::new(DeviceTransport::new(mount)),
        SessionConfig {
            workers,
            init: InitConfig {
                zero_message_open: true,
                ..InitConfig::default()
            },
        },
    )
    .await
    .expect("session must negotiate against a real kernel");
    fs.set_notifier(session.notifier().expect("notifier"))
        .expect("the notification thread");
    Mounted {
        session,
        _guard: guard,
        root: dir.path().to_path_buf(),
        _dir: dir,
    }
}

/// The LOOKUP round-trip budget at the median (plan: "p50 < 20 µs").
const LOOKUP_P50_BUDGET: Duration = Duration::from_micros(20);
/// The LOOKUP round-trip budget at the 99th percentile (plan: "p99 < 100 µs").
const LOOKUP_P99_BUDGET: Duration = Duration::from_micros(100);
/// Directories of the tree the latency tests measure in, each of [`WIDE_FILES`] tracked files.
const WIDE_DIRS: usize = 4;
const WIDE_FILES: usize = 2000;
/// Bytes each concurrent write puts in its file.
const WRITE_BYTES: usize = 4096;
/// Files a sync rewrites, spread over every directory.
const SYNC_CHANGES: usize = 400;

/// Whether `lore-fuse/bench/m2/latency.sh` runs this, past `common.sh`'s quiet-machine gate and
/// pinned to the P-cores: only then do the LOOKUP budgets count.
fn quiet_machine() -> bool {
    std::env::var_os("LORE_QUIET_MACHINE").is_some()
}

/// `dNN/fNNNNN` for [`WIDE_DIRS`] directories of [`WIDE_FILES`] files, each file's contents its
/// own name.
fn wide_tree() -> Vec<(String, Vec<u8>)> {
    (0..WIDE_DIRS)
        .flat_map(|dir| {
            (0..WIDE_FILES).map(move |file| {
                let path = format!("d{dir:02}/f{file:05}");
                let contents = path.clone().into_bytes();
                (path, contents)
            })
        })
        .collect()
}

/// Mounts `fs` read-write with `workers` session workers.
async fn mount_writable(fs: Arc<FuseFilesystem>, workers: usize) -> Mounted {
    let dir = TempDir::new("lore-fuse-lookup-latency-write-");
    let options = MountOptions {
        fsname: "lore:lookup-latency-write".to_string(),
        max_read: 1 << 17,
        fusermount: None,
    };
    let mount = Arc::new(
        lore_fuse::mount::mount_with_access(
            dir.path(),
            &options,
            lore_fuse::mount::MountAccess::ReadWrite,
        )
        .expect("read-write mount"),
    );
    let guard = UnmountGuard(dir.path().to_path_buf());
    let session = Session::start(
        fs.clone(),
        Arc::new(DeviceTransport::new(mount)),
        SessionConfig {
            workers,
            init: InitConfig {
                zero_message_open: true,
                ..InitConfig::read_write()
            },
        },
    )
    .await
    .expect("session must negotiate against a real kernel");
    fs.set_notifier(session.notifier().expect("notifier"))
        .expect("the notification thread");
    Mounted {
        session,
        _guard: guard,
        root: dir.path().to_path_buf(),
        _dir: dir,
    }
}

/// LOOKUP round trips measured once each: whether the measured directories' listing was built
/// during each, the LOOKUPs that reached the service meanwhile, and what the service did then.
struct Lookups {
    samples: Vec<u64>,
    built: Vec<bool>,
    reached: u64,
    window: Window,
}

/// Where a round trip counts as slow in the service's log2 histograms: 2^17 ns, 131 µs.
const SLOW_NS: u64 = 1 << 17;

/// The opcodes [`Window`] reads service times of, RENAME2 counted as RENAME.
const WINDOW_OPCODES: [(&str, &[Opcode]); 4] = [
    ("LOOKUP", &[Opcode::Lookup]),
    ("CREATE", &[Opcode::Create]),
    ("RENAME", &[Opcode::Rename, Opcode::Rename2]),
    ("UNLINK", &[Opcode::Unlink]),
];

/// What the service did during a measured window, from counters on only for it: service times
/// by opcode ([`WINDOW_OPCODES`]), the requests deferred, chain steps of finds by name, and block
/// reads that found a writer. A deferred request's service time is its inline attempt only. The
/// counters are process-wide, so the window is exact under `--test-threads=1`.
struct Window {
    service: [Histogram; 4],
    deferred: u64,
    chain_steps: u64,
    contended_reads: u64,
}

impl Window {
    /// Starts a window on `mounted`'s session; [`Self::end`] closes it.
    fn start(mounted: &Mounted) -> Self {
        state::step_counting::set_chain_step_counting(true);
        read_contention::set_contended_read_counting(true);
        Self {
            service: service_times(mounted.session.metrics()),
            deferred: mounted.session.deferred_requests(),
            chain_steps: state::step_counting::total_chain_steps(),
            contended_reads: read_contention::contended_reads(),
        }
    }

    /// What happened since [`Self::start`].
    fn end(self, mounted: &Mounted) -> Self {
        let service = service_times(mounted.session.metrics());
        let window = Self {
            service: std::array::from_fn(|i| service[i].since(&self.service[i])),
            deferred: mounted.session.deferred_requests() - self.deferred,
            chain_steps: state::step_counting::total_chain_steps() - self.chain_steps,
            contended_reads: read_contention::contended_reads() - self.contended_reads,
        };
        state::step_counting::set_chain_step_counting(false);
        read_contention::set_contended_read_counting(false);
        window
    }

    /// Prints the window under `label`, per measured LOOKUP where `measured` is their count.
    fn report(&self, label: &str, measured: usize) {
        for ((name, _), times) in WINDOW_OPCODES.iter().zip(&self.service) {
            println!(
                "{label}: service {name}: {} requests, p50 {} ns, p99 {} ns, {} from {SLOW_NS} ns",
                times.count(),
                times.percentile_ns(50),
                times.percentile_ns(99),
                times.at_least_ns(SLOW_NS)
            );
        }
        println!(
            "{label}: requests deferred: {}, timed above for their inline attempt only",
            self.deferred
        );
        let per = |total: u64, of: u64| {
            if of == 0 {
                "-".to_string()
            } else {
                format!("{:.1}", total as f64 / of as f64)
            }
        };
        let (creates, renames) = (self.service[1].count(), self.service[2].count());
        println!(
            "{label}: chain steps: {} in the window, per CREATE {}, per RENAME {}, per CREATE or \
             RENAME {}",
            self.chain_steps,
            per(self.chain_steps, creates),
            per(self.chain_steps, renames),
            per(self.chain_steps, creates + renames)
        );
        println!(
            "{label}: contended block reads: {} in the window, by every request; per measured \
             LOOKUP {}",
            self.contended_reads,
            per(self.contended_reads, measured as u64)
        );
    }
}

/// The service-time histograms of [`WINDOW_OPCODES`] as `metrics` holds them now.
fn service_times(metrics: &Metrics) -> [Histogram; 4] {
    std::array::from_fn(|i| {
        WINDOW_OPCODES[i]
            .1
            .iter()
            .fold(Histogram::default(), |sum, &opcode| {
                sum.plus(&metrics.buckets(opcode))
            })
    })
}

/// Counts listing builds of the directories `nodes` from here on, through `fs`'s listing hook.
fn count_builds(fs: &FuseFilesystem, nodes: Vec<NodeID>) -> Arc<AtomicU64> {
    let builds = Arc::new(AtomicU64::new(0));
    let counted = builds.clone();
    fs.set_listing_hook(Arc::new(move |node| {
        if nodes.contains(&node) {
            counted.fetch_add(1, Ordering::SeqCst);
        }
        Box::pin(async {})
    }));
    builds
}

/// The node of directory `name` below the root of `fs`'s State.
async fn dir_node(fs: &FuseFilesystem, repository: &Arc<RepositoryContext>, name: &str) -> NodeID {
    fs.state()
        .find_subnode(
            repository.clone(),
            lore_revision::node::ROOT_NODE,
            lore_storage::hash::hash_string(name),
        )
        .await
        .expect("a directory of the tree")
}

/// Stats each of `paths` once from one thread, a LOOKUP each for a name the kernel holds no dentry
/// for, each asserted to succeed; with `builds` read around each, and `mounted`'s LOOKUP count
/// across them.
async fn timed_lookups(mounted: &Mounted, paths: Vec<CString>, builds: &Arc<AtomicU64>) -> Lookups {
    let metrics = mounted.session.metrics();
    let before = metrics.snapshot(Opcode::Lookup).count;
    let window = Window::start(mounted);
    let (done, finished) = tokio::sync::oneshot::channel();
    let builds = builds.clone();
    std::thread::spawn(move || {
        let measured: Vec<(u64, bool)> = paths
            .iter()
            .map(|path| {
                let built = builds.load(Ordering::SeqCst);
                let elapsed = bench_stats::statx_ns(path, libc::AT_SYMLINK_NOFOLLOW);
                (elapsed, builds.load(Ordering::SeqCst) != built)
            })
            .collect();
        let _ = done.send(measured);
    });
    let (samples, built): (Vec<u64>, Vec<bool>) = tokio::time::timeout(WAIT_LIMIT, finished)
        .await
        .expect("the LOOKUPs finish")
        .expect("lookup thread")
        .into_iter()
        .unzip();
    // A worker counts an inline LOOKUP after it sends the reply, so the last count can land just
    // after its stat returns.
    let counted = Instant::now() + Duration::from_secs(1);
    let mut reached = metrics.snapshot(Opcode::Lookup).count - before;
    while reached < samples.len() as u64 && Instant::now() < counted {
        tokio::time::sleep(Duration::from_millis(1)).await;
        reached = metrics.snapshot(Opcode::Lookup).count - before;
    }
    Lookups {
        samples,
        built,
        reached,
        window: window.end(mounted),
    }
}

/// Reports `lookups` under `label`: their latency, the share during which the measured listing
/// was built, and that share among the slowest tenth and hundredth. On a quiet machine, with
/// `budget`, holds them to it.
fn report_lookups(label: &str, lookups: Lookups, budget: bool) {
    let count = lookups.samples.len();
    let mut sorted = lookups.samples.clone();
    let summary = bench_stats::Summary::of(&mut sorted);
    let p90 = sorted[count * 9 / 10];
    let share = |above: u64| {
        let slow: Vec<bool> = lookups
            .samples
            .iter()
            .zip(&lookups.built)
            .filter(|(sample, _)| **sample > above)
            .map(|(_, built)| *built)
            .collect();
        let built = slow.iter().filter(|built| **built).count();
        format!("{built} of {}", slow.len())
    };
    let mut quick: Vec<u64> = lookups
        .samples
        .iter()
        .zip(&lookups.built)
        .filter(|(_, built)| !**built)
        .map(|(sample, _)| *sample)
        .collect();
    let mut rebuilt: Vec<u64> = lookups
        .samples
        .iter()
        .zip(&lookups.built)
        .filter(|(_, built)| **built)
        .map(|(sample, _)| *sample)
        .collect();
    let (quick, rebuilt) = (
        bench_stats::Summary::of(&mut quick),
        bench_stats::Summary::of(&mut rebuilt),
    );
    println!(
        "{label}: {count} LOOKUPs ({} reached the service): p50 {} ns, p90 {p90} ns, p99 {} ns, \
         max {} ns{}",
        lookups.reached,
        summary.p50,
        summary.p99,
        summary.max,
        if quiet_machine() && budget {
            ""
        } else {
            " (budgets not asserted)"
        }
    );
    println!(
        "{label}: listing built during {} LOOKUPs ({:.1}%): above p90 {}, above p99 {}; with a \
         build p50 {} ns, without p50 {} ns p99 {} ns",
        rebuilt.count,
        100.0 * rebuilt.count as f64 / count as f64,
        share(p90),
        share(summary.p99),
        rebuilt.p50,
        quick.p50,
        quick.p99
    );
    println!(
        "{label}: round trips from {SLOW_NS} ns: {}",
        lookups
            .samples
            .iter()
            .filter(|&&sample| sample >= SLOW_NS)
            .count()
    );
    lookups.window.report(label, count);
    assert!(
        lookups.reached >= count as u64,
        "every stat must reach the service as a LOOKUP"
    );
    if quiet_machine() && budget {
        assert!(
            Duration::from_nanos(summary.p50) < LOOKUP_P50_BUDGET,
            "{label}: LOOKUP p50 {} ns over {LOOKUP_P50_BUDGET:?}",
            summary.p50
        );
        assert!(
            Duration::from_nanos(summary.p99) < LOOKUP_P99_BUDGET,
            "{label}: LOOKUP p99 {} ns over {LOOKUP_P99_BUDGET:?}",
            summary.p99
        );
    }
}

/// What a writer does in its directory.
#[derive(Clone, Copy)]
enum Writes {
    /// Saves through a temporary: create, write, rename over a name, unlink the save before.
    Saves,
    /// New files only: create and write.
    Creates,
}

/// One writer's `writes` in `dir` until `stop`, counting each request in `ops`. Saves create
/// `wT-N.tmp`, write it, rename it to `wT-N.o` and unlink the save before; creates write `wT-N.o`.
/// Leaves its last file in place and answers its name.
// The writes go through the mount, as a build's do.
#[allow(clippy::disallowed_methods)]
fn write_until(
    dir: &Path,
    (writer, writes): (usize, Writes),
    stop: &AtomicBool,
    ops: &AtomicU64,
) -> PathBuf {
    let bytes = vec![b'w'; WRITE_BYTES];
    let mut previous: Option<PathBuf> = None;
    let mut save = 0usize;
    loop {
        let saved = dir.join(format!("w{writer}-{save}.o"));
        match writes {
            Writes::Saves => {
                let temporary = dir.join(format!("w{writer}-{save}.tmp"));
                let mut file = std::fs::File::create_new(&temporary).expect("create");
                file.write_all(&bytes).expect("write");
                drop(file);
                std::fs::rename(&temporary, &saved).expect("rename");
                if let Some(previous) = previous.replace(saved) {
                    std::fs::remove_file(previous).expect("unlink");
                }
            }
            Writes::Creates => {
                let mut file = std::fs::File::create_new(&saved).expect("create");
                file.write_all(&bytes).expect("write");
                previous = Some(saved);
            }
        }
        ops.fetch_add(4, Ordering::SeqCst);
        save += 1;
        if stop.load(Ordering::SeqCst) {
            return previous.expect("a save");
        }
    }
}

/// Warm LOOKUPs in `d00` while one writer per entry of `writers` writes in its directory:
/// every LOOKUP finds its file, the writes run throughout and leave no temporary. Reports the
/// LOOKUPs under `label`, holding them to the budget on a quiet machine where `budget`.
// The writers and lookups go through the mount, as a build's do.
#[allow(clippy::disallowed_methods)]
async fn lookups_under_writers(label: &str, writers: [(&str, Writes); 4], budget: bool) {
    let tree = wide_tree();
    let files: Vec<(&str, &[u8])> = tree
        .iter()
        .map(|(path, contents)| (path.as_str(), contents.as_slice()))
        .collect();
    let fixture = Box::pin(fuse_fixture::committed(&files)).await;
    let external = TempDir::new("lore-fuse-lookup-latency-external-");
    let fs = lore_base::runtime::LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            FuseFilesystem::new_writable(
                fixture.repository.clone(),
                fixture.state.clone(),
                attr_context(),
                external.path(),
            ),
        )
        .await
        .expect("writable provider");
    let measured_node = dir_node(&fs, &fixture.repository, "d00").await;
    let builds = count_builds(&fs, vec![measured_node]);
    let held = fs.clone();
    // Inodes gone at a FORGET, which reclaim may send for a measured file before it is checked.
    let forgotten = Arc::new(std::sync::Mutex::new(HashSet::new()));
    fs.set_write_hook(Arc::new({
        let forgotten = forgotten.clone();
        move |step| {
            if let WriteStep::Forgotten { ino } = step {
                forgotten.lock().expect("forgotten").insert(ino);
            }
            true
        }
    }));
    let mounted = mount_writable(fs, 8).await;
    let measured = mounted.root.join("d00");
    // The listing is built here, so every measured LOOKUP finds it warm or patched.
    std::fs::symlink_metadata(measured.join("f00000")).expect("warm-up stat");
    // The measured files with an inode, by the number the provider derives for each. The writers'
    // own LOOKUPs reach the service too, so their count says nothing of the measured ones.
    let names: Vec<String> = (1..WIDE_FILES).map(|file| format!("f{file:05}")).collect();
    let dir_ino = std::os::unix::fs::MetadataExt::ino(
        &std::fs::symlink_metadata(&measured).expect("stat d00"),
    );
    let with_inodes = || {
        names
            .iter()
            .filter(|name| {
                let ino = derive_inode_number(&Identity {
                    parent_ino: dir_ino,
                    name,
                    name_hash: lore_storage::hash::hash_string(name),
                    kind: IdentityKind::File,
                });
                held.inodes().get(ino).is_some()
                    || forgotten.lock().expect("forgotten").contains(&ino)
            })
            .count()
    };
    assert_eq!(
        with_inodes(),
        0,
        "no measured file has an inode, so the kernel holds no dentry a stat could answer from"
    );
    forgotten.lock().expect("forgotten").clear();

    let stop = Arc::new(AtomicBool::new(false));
    let ops = Arc::new(AtomicU64::new(0));
    let threads: Vec<_> = writers
        .into_iter()
        .enumerate()
        .map(|(writer, (dir, writes))| {
            let (dir, stop, ops) = (mounted.root.join(dir), stop.clone(), ops.clone());
            std::thread::spawn(move || {
                (
                    dir.clone(),
                    write_until(&dir, (writer, writes), &stop, &ops),
                )
            })
        })
        .collect();
    wait_for_first_changes(&ops, 4 * 4, || {
        threads.iter().any(|thread| thread.is_finished())
    })
    .await;

    let paths = names
        .iter()
        .map(|name| bench_stats::c_path(&measured.join(name)))
        .collect();
    let ops_before = ops.load(Ordering::SeqCst);
    let builds_before = builds.load(Ordering::SeqCst);
    let lookups = timed_lookups(&mounted, paths, &builds).await;
    let builds_during = builds.load(Ordering::SeqCst) - builds_before;
    let ops_during = ops.load(Ordering::SeqCst) - ops_before;
    stop.store(true, Ordering::SeqCst);
    assert_eq!(
        with_inodes(),
        names.len(),
        "every measured stat reached the service as a LOOKUP"
    );
    // Every writer returns before any directory is listed: a writer still saving in the same
    // directory legitimately shows its temporary.
    let finished: Vec<(PathBuf, PathBuf)> = join_within_limit(threads).await;
    for (dir, last) in finished {
        assert_eq!(
            std::fs::metadata(&last)
                .expect("a writer's last file")
                .len(),
            WRITE_BYTES as u64
        );
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .expect("list")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| name.starts_with('w') && name.ends_with(".tmp"))
            .collect();
        // A name the listing shows but a stat cannot find is a stale READDIR.
        let (present, absent): (Vec<String>, Vec<String>) = leftovers
            .into_iter()
            .partition(|name| std::fs::symlink_metadata(dir.join(name)).is_ok());
        assert_eq!(present, Vec::<String>::new(), "temporaries left in {dir:?}");
        assert_eq!(
            absent,
            Vec::<String>::new(),
            "READDIR of {dir:?} listed renamed-away names"
        );
    }

    println!(
        "{label}: {ops_during} write requests and {builds_during} builds of the measured listing \
         during the LOOKUPs"
    );
    assert!(ops_during > 0, "the writes must run during the LOOKUPs");
    report_lookups(label, lookups, budget);
    tokio::time::timeout(Duration::from_secs(5), mounted.session.shutdown())
        .await
        .expect("shutdown must complete promptly");
}

/// Warm LOOKUPs in one directory while four threads create, write, rename and unlink, two in the
/// same directory and two in others: every LOOKUP finds its file and the writes run throughout.
/// On a quiet machine the LOOKUPs keep to p50 < 20 µs and p99 < 100 µs.
#[tokio::test(flavor = "multi_thread")]
async fn warm_lookups_under_concurrent_writes_stay_within_budget() {
    skip_without_fuse!();
    lookups_under_writers(
        "LOOKUP under concurrent writes",
        [
            ("d00", Writes::Saves),
            ("d00", Writes::Saves),
            ("d01", Writes::Saves),
            ("d02", Writes::Saves),
        ],
        true,
    )
    .await;
}

/// Task 14 diagnostic: the same LOOKUPs with the four writers' saves only in other directories,
/// which leaves the measured listing current, so what remains of the tail is contention.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "task 14 diagnostic: run through lore-fuse/bench/m2/latency.sh"]
async fn warm_lookups_with_saves_only_in_other_directories() {
    skip_without_fuse!();
    lookups_under_writers(
        "LOOKUP with saves elsewhere",
        [
            ("d01", Writes::Saves),
            ("d01", Writes::Saves),
            ("d02", Writes::Saves),
            ("d03", Writes::Saves),
        ],
        false,
    )
    .await;
}

/// Task 14 diagnostic: the same LOOKUPs while four writers only create files in the measured
/// directory, so a LOOKUP finds the listing patched by up to 64 attaches and walks them first.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "task 14 diagnostic: run through lore-fuse/bench/m2/latency.sh"]
async fn warm_lookups_among_creates_in_their_directory() {
    skip_without_fuse!();
    lookups_under_writers(
        "LOOKUP among creates",
        [
            ("d00", Writes::Creates),
            ("d00", Writes::Creates),
            ("d00", Writes::Creates),
            ("d00", Writes::Creates),
        ],
        false,
    )
    .await;
}

/// Warm LOOKUPs in two directories while syncs swap the mount between two revisions back to back,
/// each rewriting [`SYNC_CHANGES`] files across every directory, the measured ones included: every
/// LOOKUP finds its file and a sync completes during them. On a quiet machine the LOOKUPs keep to
/// p50 < 20 µs and p99 < 100 µs.
#[tokio::test(flavor = "multi_thread")]
async fn warm_lookups_during_a_sync_stay_within_budget() {
    skip_without_fuse!();
    let (immutable, mutable, execution) =
        fuse_fixture::test_store_create().await.expect("test store");
    let tree = wide_tree();
    let (instance, first, second, rewritten) = lore_base::runtime::LORE_CONTEXT
        .scope(execution.clone(), async {
            let instance = fuse_fixture::test_repository_create(
                immutable,
                mutable,
                RepositoryId::from(uuid::Uuid::now_v7()),
            )
            .await;
            for dir in 0..WIDE_DIRS {
                std::fs::create_dir_all(instance.path.join(format!("d{dir:02}")))
                    .expect("a directory of the tree");
            }
            for (path, contents) in &tree {
                fuse_fixture::test_file_write(&instance.path.join(path), contents);
            }
            let first = fuse_fixture::test_commit_tree(&instance, "sync from").await;
            for (path, _) in tree.iter().step_by(tree.len() / SYNC_CHANGES) {
                fuse_fixture::test_file_write(
                    &instance.path.join(path),
                    format!("synced {path}").as_bytes(),
                );
            }
            let second = fuse_fixture::test_commit_tree(&instance, "sync to").await;
            let diff = |from: &Arc<State>, to: &Arc<State>| {
                state::diff_collect(
                    instance.repository.clone(),
                    from.clone(),
                    instance.repository.clone(),
                    to.clone(),
                    None,
                    FilterMode::View,
                )
            };
            let rewritten = diff(&first, &second).await.expect("diff").len();
            (instance, first, second, rewritten)
        })
        .await;
    // Kept to the end: its directory holds the repository's `.lore`.
    let repository = instance.repository.clone();
    assert!(
        rewritten >= SYNC_CHANGES,
        "the sync rewrites {rewritten} files"
    );

    let fs = lore_base::runtime::LORE_CONTEXT
        .scope(
            execution.clone(),
            FuseFilesystem::new(repository.clone(), first.clone(), attr_context()),
        )
        .await
        .expect("provider");
    let measured_nodes = vec![
        dir_node(&fs, &repository, "d00").await,
        dir_node(&fs, &repository, "d01").await,
    ];
    let builds = count_builds(&fs, measured_nodes);
    let mounted = mount(fs.clone(), 8).await;
    let measured = [mounted.root.join("d00"), mounted.root.join("d01")];
    for dir in &measured {
        std::fs::symlink_metadata(dir.join("f00000")).expect("warm-up stat");
    }

    let stop = Arc::new(AtomicBool::new(false));
    let syncs = Arc::new(AtomicU64::new(0));
    // The task carries its own execution scope.
    #[allow(clippy::disallowed_methods)]
    let syncer = {
        let (fs, stop, syncs) = (fs.clone(), stop.clone(), syncs.clone());
        tokio::spawn(
            lore_base::runtime::LORE_CONTEXT.scope(execution.clone(), async move {
                let operation = InstanceOperationImpl::new(
                    StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
                    true,
                );
                let mut at_first = true;
                while !stop.load(Ordering::SeqCst) {
                    let to = if at_first {
                        second.clone()
                    } else {
                        first.clone()
                    };
                    swap_mounted(&fs, &operation, repository.clone(), to)
                        .await
                        .expect("sync");
                    at_first = !at_first;
                    syncs.fetch_add(1, Ordering::SeqCst);
                }
            }),
        )
    };
    // Syncs run back to back from here, so the LOOKUPs overlap one throughout.
    wait_for_first_changes(&syncs, 1, || syncer.is_finished()).await;

    let paths = measured
        .iter()
        .flat_map(|dir| {
            (1..WIDE_FILES).map(move |file| bench_stats::c_path(&dir.join(format!("f{file:05}"))))
        })
        .collect();
    let syncs_before = syncs.load(Ordering::SeqCst);
    let builds_before = builds.load(Ordering::SeqCst);
    let lookups = timed_lookups(&mounted, paths, &builds).await;
    let builds_during = builds.load(Ordering::SeqCst) - builds_before;
    let syncs_during = syncs.load(Ordering::SeqCst) - syncs_before;
    stop.store(true, Ordering::SeqCst);
    tokio::time::timeout(WAIT_LIMIT, syncer)
        .await
        .expect("the syncs stop")
        .expect("syncer");

    println!(
        "syncs of {SYNC_CHANGES} changes completed during the LOOKUPs: {syncs_during}, with \
         {builds_during} builds of the measured listings"
    );
    assert!(syncs_during > 0, "a sync must complete during the LOOKUPs");
    report_lookups("LOOKUP during a sync", lookups, true);
    tokio::time::timeout(Duration::from_secs(5), mounted.session.shutdown())
        .await
        .expect("shutdown must complete promptly");
}
