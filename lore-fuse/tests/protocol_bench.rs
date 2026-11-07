// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Task 17's per-opcode zero-allocation assertion (spec: "Allocation discipline"), in a process of
//! its own: allocation counting is a process-wide switch, and a test in a shared process switching
//! it off during this test's measurement would let an opcode that allocates pass.
#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::io::Read as _;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use lore_base::test_util::TempDir;
use lore_fuse::abi::Opcode;
use lore_fuse::abi::ROOT_ID;
use lore_fuse::init::InitConfig;
use lore_fuse::mount::unmount_path;
use lore_fuse::session::Session;
use lore_fuse::session::SessionConfig;
use lore_fuse::skip_without_fuse;
use lore_fuse::transport::Notifier;
use lore_fuse::transport::device::DeviceTransport;

#[path = "support/synthetic.rs"]
mod synthetic;

use synthetic::SynthNode;
use synthetic::SyntheticFs;
use synthetic::mount_synthetic;

/// Lazily unmounts `path` on drop, so a test that panics midway never leaves a mount behind.
struct PathUnmountGuard(PathBuf);

impl Drop for PathUnmountGuard {
    fn drop(&mut self) {
        let _ = unmount_path(&self.0, true);
    }
}

/// Restores allocation counting to disabled on drop, including on test panic/unwind, so a failed
/// assertion mid-measurement can never leave counting on for a later test in the same process.
struct DisableCountingOnDrop;
impl Drop for DisableCountingOnDrop {
    fn drop(&mut self) {
        lore_base::allocator::set_allocation_counting(false);
    }
}

fn session_config() -> SessionConfig {
    SessionConfig {
        workers: 1,
        init: InitConfig::default(),
    }
}

fn statfs(path: &Path) {
    let c_path = CString::new(path.as_os_str().as_bytes()).expect("path must not contain NUL");
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is a valid, NUL-terminated C string; `buf` is a valid out-pointer sized
    // for `statvfs`'s own struct.
    let ret = unsafe { libc::statvfs(c_path.as_ptr(), &mut buf) };
    assert_eq!(ret, 0, "statvfs must succeed against a real mount");
}

/// One lookup+open+read+getattr+release+readdir(plus)+statfs+forget-trigger cycle against `root`,
/// entirely on the calling thread. Callers must run this on a plain `std::thread`, never the
/// tokio runtime serving the session (a syscall on a serving thread can deadlock the session's
/// own workers). `suffix` keeps warmup and measured names distinct.
fn drive_all_opcodes(root: &Path, notifier: &Notifier, suffix: &str) {
    // OPEN, READ, GETATTR (fh-based, no path walk), RELEASE.
    let mut file = std::fs::File::open(root.join(format!("file-{suffix}")))
        .unwrap_or_else(|e| panic!("open must succeed: {e}"));
    let mut buf = [0u8; 4096];
    file.read_exact(&mut buf)
        .unwrap_or_else(|e| panic!("read must succeed: {e}"));
    file.metadata()
        .unwrap_or_else(|e| panic!("fstat must succeed: {e}"));
    drop(file);

    // READDIR/READDIRPLUS: a first pass that stats every entry trains the kernel's
    // READDIRPLUS_AUTO heuristic (the same "ls -l" pattern task 15's own mount test relies on),
    // so the second pass is more likely to actually receive READDIRPLUS instead of plain READDIR
    // (confirmed empirically: see the report).
    for entry in std::fs::read_dir(root).unwrap_or_else(|e| panic!("readdir must succeed: {e}")) {
        let entry = entry.unwrap_or_else(|e| panic!("dirent must decode: {e}"));
        let _ = entry.metadata();
    }
    for entry in std::fs::read_dir(root).unwrap_or_else(|e| panic!("readdir must succeed: {e}")) {
        let _ = entry.unwrap_or_else(|e| panic!("dirent must decode: {e}"));
    }

    statfs(root);

    // LOOKUP, on a name never looked up before in this pass, with no open fd held afterward --
    // so the explicit `inval_entry` below can reach zero references and evict down to a real
    // FORGET (spec: "Notifications"; the same mechanism `fuse_lock_inode`/kernel LRU eviction
    // would otherwise trigger only under memory pressure, which an unprivileged test cannot
    // force -- see the report for why this replaces `/proc/sys/vm/drop_caches`).
    let forget_name = format!("forget-me-{suffix}");
    std::fs::symlink_metadata(root.join(&forget_name))
        .unwrap_or_else(|e| panic!("lookup must succeed: {e}"));
    notifier
        .inval_entry(ROOT_ID, forget_name.as_bytes(), false)
        .expect("inval_entry must send");
}

/// Runs [`drive_all_opcodes`] via `lore_spawn_blocking!`, never a direct `.join()`. That macro's
/// single-expression form dispatches onto `lore_base::runtime::core_runtime()` -- a genuinely
/// separate `Runtime` from this test's own. Under `current_thread` flavor, `lore_spawn!` (used by
/// `Session`'s worker) schedules onto whatever runtime is already ambient, i.e. this very test
/// runtime, which has exactly one thread; a synchronous `.join()` on that thread would block the
/// only thread able to poll the worker task, deadlocking it. Awaiting `core_runtime()`'s blocking
/// task instead only polls for completion, so the executor stays free to keep running the worker.
async fn run_pass(root: &Path, notifier: &Notifier, suffix: &'static str) {
    let root = root.to_path_buf();
    let notifier = notifier.clone();
    lore_base::lore_spawn_blocking!(move || drive_all_opcodes(&root, &notifier, suffix))
        .await
        .expect("blocking task must not panic");
}

fn bench_tree(file_len: u64) -> Vec<(String, SynthNode)> {
    let file = |len: u64| SynthNode::File {
        content: vec![0u8; len as usize],
        executable: false,
    };
    vec![
        ("file-warmup".to_string(), file(file_len)),
        ("file-measured".to_string(), file(file_len)),
        ("forget-me-warmup".to_string(), file(0)),
        ("forget-me-measured".to_string(), file(0)),
    ]
}

fn forget_family_count(session: &Session<SyntheticFs, DeviceTransport>) -> u64 {
    let metrics = session.metrics();
    metrics.snapshot(Opcode::Forget).count + metrics.snapshot(Opcode::BatchForget).count
}

/// Decision 21's zero-allocation assertion: `Session` with a single worker on a `current_thread`
/// runtime, so the session's own worker executes every request on one stable OS thread -- the
/// same thread `dispatch()`'s thread-local allocation counter (spec: "Allocation discipline")
/// then measures. All syscalls against the mount are driven from a plain `std::thread`, never
/// this test's own runtime thread.
#[tokio::test(flavor = "current_thread")]
async fn allocations_per_opcode_are_zero_after_warmup() {
    skip_without_fuse!();

    let dir = TempDir::new("lore-fuse-protocol-bench-alloc-test-");
    let fs = SyntheticFs::from_tree(bench_tree(4096));
    let (_mount, session) = mount_synthetic(dir.path(), fs, session_config())
        .await
        .expect("mount must negotiate against a real kernel");
    let guard = PathUnmountGuard(dir.path().to_path_buf());
    let notifier = session.notifier().expect("notifier must be available");
    let root = dir.path().to_path_buf();

    // Warmup pass, no allocation counting: absorbs one-time costs (lazy static data, first-touch
    // page faults, buffer growth) that would otherwise look like a per-opcode allocation.
    run_pass(&root, &notifier, "warmup").await;
    let warmup_deadline = Instant::now() + Duration::from_secs(2);
    while forget_family_count(&session) == 0 {
        assert!(
            Instant::now() < warmup_deadline,
            "the warmup pass's explicit inval_entry must produce a real FORGET/BATCH_FORGET"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let before_forget = forget_family_count(&session);
    lore_base::allocator::set_allocation_counting(true);
    let _restore = DisableCountingOnDrop;
    run_pass(&root, &notifier, "measured").await;

    let measured_deadline = Instant::now() + Duration::from_secs(2);
    while forget_family_count(&session) == before_forget {
        assert!(
            Instant::now() < measured_deadline,
            "the measured pass's explicit inval_entry must produce a real FORGET/BATCH_FORGET"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    for &op in &[
        Opcode::Lookup,
        Opcode::Getattr,
        Opcode::Open,
        Opcode::Read,
        Opcode::Release,
        Opcode::Readdirplus,
        Opcode::Statfs,
    ] {
        let snapshot = session.metrics().snapshot(op);
        assert!(
            snapshot.count > 0,
            "{op:?} must have been exercised at least once, got {snapshot:?}"
        );
        assert_eq!(
            snapshot.max_allocations, 0,
            "{op:?} must not allocate: {snapshot:?}"
        );
    }
    for &op in &[Opcode::Forget, Opcode::BatchForget] {
        let snapshot = session.metrics().snapshot(op);
        if snapshot.count > 0 {
            assert_eq!(
                snapshot.max_allocations, 0,
                "{op:?} must not allocate: {snapshot:?}"
            );
        }
    }

    drop(_restore);
    tokio::time::timeout(Duration::from_secs(5), session.shutdown())
        .await
        .expect("shutdown must complete promptly");
    drop(guard);
}
