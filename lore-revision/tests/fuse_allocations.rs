// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Per-request allocation counts of the FUSE provider on a real mount (decision 21): one
//! worker on a `current_thread` runtime, so the dispatcher's thread-local count covers the
//! whole request, with every syscall made from the blocking pool.
//!
//! A process of its own: each test reads the most allocations one request made since its session
//! started, warm-up included, and allocation counting is switched process-wide, so another test
//! switching it on during a warm-up would count that warm-up.
#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::io::Read as _;
use std::os::fd::AsRawFd as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_fuse::abi::Opcode;
use lore_fuse::abi::ROOT_ID;
use lore_fuse::init::InitConfig;
use lore_fuse::mount::MountOptions;
use lore_fuse::mount::unmount_path;
use lore_fuse::session::Session;
use lore_fuse::session::SessionConfig;
use lore_fuse::skip_without_fuse;
use lore_fuse::transport::Notifier;
use lore_fuse::transport::device::DeviceTransport;
use lore_revision::fs::fuse::attr::AttrContext;
use lore_revision::fs::fuse::filesystem::FuseFilesystem;

#[path = "support/fuse_fixture.rs"]
mod fuse_fixture;
#[path = "support/fuse_test_util.rs"]
mod fuse_test_util;
#[path = "helper.rs"]
mod helper;

use fuse_test_util::PAGE;

/// Allocations one READ of a local fragment may make, all of them inside `lore-storage`'s read
/// path; the provider and protocol layers add none.
const READ_ALLOCATION_BUDGET: u64 = 8;

/// Allocations one CREATE may make, measured. The namespace budgets are each the most any one
/// request made in `create_unlink_and_rename_stay_within_their_budget`.
const CREATE_ALLOCATION_BUDGET: u64 = 5;
/// Allocations one UNLINK of a tracked file the store serves may make, its directory's listing
/// current, measured: its path, and the deletion record's.
const TRACKED_UNLINK_ALLOCATION_BUDGET: u64 = 4;
/// Allocations one UNLINK of a local file the revision does not hold may make, measured: its
/// path and its set-aside, 5, and 1 more where pinning its descriptor grows the descriptor cache.
const LOCAL_UNLINK_ALLOCATION_BUDGET: u64 = 6;
/// Allocations one UNLINK of a tracked file a write copied up may make, measured. It is both
/// kinds at once, so it pays a local file's set-aside and a tracked file's deletion record, 8,
/// and 1 more where pinning its descriptor grows the descriptor cache.
const COPIED_UP_UNLINK_ALLOCATION_BUDGET: u64 = 9;
/// Allocations one RENAME may make, onto a free name or over a tracked file, measured.
const RENAME_ALLOCATION_BUDGET: u64 = 2;

const FILE_LEN: usize = 4096;
const MAX_READ: u32 = 1 << 17;
/// Incompressible, so its leaves are stored as the content, spanning several of them.
const FRAGMENTED_LEN: usize = 3 << 20;
/// Seeds the fragmented file's content.
const FRAGMENTED_SEED: u64 = 0x2545_f491_4f6c_dd1d;

type MountedSession = Session<FuseFilesystem, DeviceTransport>;

/// How long a test waits for the kernel to forget an inode whose name it invalidated. Whatever
/// still holds the name's dentry, as another process on a loaded machine may, delays the FORGET
/// until it lets go.
const FORGET_WAIT: Duration = Duration::from_secs(60);

/// Counting is switched process-wide, so one test's measured pass would count the other's
/// warm-up.
static COUNTING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Unmounts on drop, lazily if a plain unmount is refused, so a failing test leaves no mount.
struct UnmountGuard(PathBuf);

impl Drop for UnmountGuard {
    fn drop(&mut self) {
        if unmount_path(&self.0, false).is_err() {
            let _ = unmount_path(&self.0, true);
        }
    }
}

struct DisableCountingOnDrop;

impl Drop for DisableCountingOnDrop {
    fn drop(&mut self) {
        lore_base::allocator::set_allocation_counting(false);
    }
}

fn contents(seed: u8) -> Vec<u8> {
    (0..FILE_LEN)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

struct Mounted {
    // Field order is drop order: the session stops before the unmount.
    session: Arc<MountedSession>,
    fs: Arc<FuseFilesystem>,
    notifier: Notifier,
    _guard: UnmountGuard,
    root: PathBuf,
    _dir: TempDir,
    _fixture: fuse_fixture::FuseFixture,
}

async fn mount() -> Mounted {
    let warm = contents(1);
    let measured = contents(2);
    let fragmented = fuse_test_util::incompressible(FRAGMENTED_LEN, FRAGMENTED_SEED);
    let fixture = fuse_fixture::committed(&[
        ("warm.bin", &warm),
        ("measured.bin", &measured),
        ("fragmented.bin", &fragmented),
        ("dir/held.txt", b"held"),
        ("forget/warmup.txt", b"w"),
        ("forget/measured.txt", b"m"),
    ])
    .await;
    let fs = LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            FuseFilesystem::new(
                fixture.repository.clone(),
                fixture.state.clone(),
                AttrContext::for_mounting_user(MAX_READ, 1_700_000_000_000),
            ),
        )
        .await
        .expect("provider construction");

    let dir = TempDir::new("lore-fuse-allocations-");
    let options = MountOptions {
        fsname: "lore:allocations".to_string(),
        max_read: MAX_READ,
        fusermount: None,
    };
    let mount = Arc::new(lore_fuse::mount::mount(dir.path(), &options).expect("mount"));
    let guard = UnmountGuard(dir.path().to_path_buf());
    let transport = Arc::new(DeviceTransport::new(mount));
    let session = Session::start(
        fs.clone(),
        transport,
        SessionConfig {
            workers: 1,
            init: InitConfig {
                zero_message_open: true,
                ..InitConfig::default()
            },
        },
    )
    .await
    .expect("session must negotiate against a real kernel");
    let notifier = session.notifier().expect("notifier");
    fs.set_notifier(notifier.clone())
        .expect("the notification thread");
    Mounted {
        session,
        fs,
        notifier,
        _guard: guard,
        root: dir.path().to_path_buf(),
        _dir: dir,
        _fixture: fixture,
    }
}

impl Mounted {
    fn forgets(&self) -> u64 {
        let metrics = self.session.metrics();
        metrics.snapshot(Opcode::Forget).count + metrics.snapshot(Opcode::BatchForget).count
    }

    /// Waits until the provider has dropped inode `ino`, which only the kernel's FORGET of its
    /// last lookup does.
    async fn await_forgotten(&self, ino: u64) {
        let deadline = Instant::now() + FORGET_WAIT;
        while self.fs.inodes().get(ino).is_some() {
            assert!(
                Instant::now() < deadline,
                "inode {ino} was not forgotten in {FORGET_WAIT:?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn shutdown(self) {
        tokio::time::timeout(Duration::from_secs(5), self.session.shutdown())
            .await
            .expect("shutdown must complete promptly");
    }
}

/// Runs `work` on the blocking pool; a syscall on this runtime's only thread would wait on a
/// request that same thread has to serve.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    lore_base::lore_spawn_blocking!(work)
        .await
        .expect("syscall task must not panic")
}

fn statvfs(path: &Path) {
    let c_path = CString::new(path.as_os_str().as_bytes()).expect("path without NUL");
    // SAFETY: `statvfs` is plain old data, valid when zeroed.
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is NUL-terminated and `buf` is a valid out-pointer.
    let ret = unsafe { libc::statvfs(c_path.as_ptr(), &mut buf) };
    assert_eq!(ret, 0, "statvfs must succeed");
}

/// GETATTR on an open file, forced past the kernel's attribute cache.
fn forced_getattr(file: &std::fs::File) {
    let empty = CString::default();
    // SAFETY: `statx` is plain old data, valid when zeroed.
    let mut buf: libc::statx = unsafe { std::mem::zeroed() };
    // SAFETY: the fd is open for the call, the path is an empty NUL-terminated string, and
    // `buf` is a valid out-pointer.
    let ret = unsafe {
        libc::statx(
            file.as_raw_fd(),
            empty.as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_STATX_FORCE_SYNC,
            libc::STATX_BASIC_STATS,
            &mut buf,
        )
    };
    assert_eq!(ret, 0, "statx must succeed");
}

fn list_with_attributes(dir: &Path) {
    for entry in std::fs::read_dir(dir).expect("readdir") {
        let _ = entry.expect("dirent").metadata();
    }
}

fn read_file(path: &Path) {
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .expect("open")
        .read_to_end(&mut buf)
        .expect("read");
    assert_eq!(buf.len(), FILE_LEN);
}

/// `O_DIRECT` READs of `len` bytes at each of `offsets`, each reaching the provider and each
/// answering the bytes of `expected` it covers.
fn read_direct(path: &Path, len: usize, offsets: &[u64], expected: &[u8]) {
    let file = fuse_test_util::open_direct(path);
    let buf = fuse_test_util::Aligned::new(len);
    for &offset in offsets {
        assert_eq!(fuse_test_util::pread(&file, &buf, len, offset), len);
        let at = offset as usize;
        assert!(
            buf.as_slice()[..len] == expected[at..at + len],
            "the READ at {offset} answers the file's bytes"
        );
    }
}

fn ino_of(path: &Path) -> u64 {
    std::fs::symlink_metadata(path).expect("stat").ino()
}

#[tokio::test(flavor = "current_thread")]
async fn warm_lookup_getattr_readdirplus_open_release_forget_statfs_allocate_zero() {
    skip_without_fuse!();
    let _counting = COUNTING.lock().await;
    let m = Box::pin(mount()).await;
    let root = m.root.clone();

    // Warm-up, uncounted: builds listings, creates every inode the measured pass touches, and
    // answers the one OPEN a zero-message-open connection ever sends.
    let (held, dir_ino, forget_ino, [warmup_ino, measured_ino]) = blocking({
        let root = root.clone();
        move || {
            list_with_attributes(&root);
            list_with_attributes(&root);
            read_file(&root.join("warm.bin"));
            let held = std::fs::File::open(root.join("dir/held.txt")).expect("open held");
            forced_getattr(&held);
            statvfs(&root);
            (
                held,
                ino_of(&root.join("dir")),
                ino_of(&root.join("forget")),
                ["forget/warmup.txt", "forget/measured.txt"].map(|name| ino_of(&root.join(name))),
            )
        }
    })
    .await;
    m.notifier
        .inval_entry(forget_ino, b"warmup.txt", false)
        .expect("inval_entry");
    m.await_forgotten(warmup_ino).await;

    // Make the kernel ask again: drop the held file's dentry (its open fd keeps the inode, so
    // the provider still has it) and the root's cached directory pages.
    m.notifier
        .inval_entry(dir_ino, b"held.txt", false)
        .expect("inval_entry");
    m.notifier.inval_inode(ROOT_ID, 0, 0).expect("inval_inode");

    const MEASURED: [Opcode; 4] = [
        Opcode::Lookup,
        Opcode::Getattr,
        Opcode::Readdirplus,
        Opcode::Statfs,
    ];
    let counts_before = MEASURED.map(|op| m.session.metrics().snapshot(op).count);
    let before_forget = m.forgets();
    lore_base::allocator::set_allocation_counting(true);
    let restore = DisableCountingOnDrop;
    let held = blocking({
        let root = root.clone();
        move || {
            std::fs::symlink_metadata(root.join("dir/held.txt")).expect("lookup");
            forced_getattr(&held);
            list_with_attributes(&root);
            statvfs(&root);
            held
        }
    })
    .await;
    m.notifier
        .inval_entry(forget_ino, b"measured.txt", false)
        .expect("inval_entry");
    m.await_forgotten(measured_ino).await;
    assert!(m.forgets() > before_forget, "a FORGET ran while counting");
    drop(restore);
    drop(held);

    let metrics = m.session.metrics();
    for (op, before) in MEASURED.iter().zip(counts_before) {
        assert!(
            metrics.snapshot(*op).count > before,
            "{op:?} must run in the measured pass"
        );
    }
    for op in [
        Opcode::Lookup,
        Opcode::Getattr,
        Opcode::Readdirplus,
        Opcode::Open,
        Opcode::Statfs,
    ] {
        let snapshot = metrics.snapshot(op);
        assert!(snapshot.count > 0, "{op:?} must have run: {snapshot:?}");
        assert_eq!(
            snapshot.max_allocations, 0,
            "{op:?} must not allocate: {snapshot:?}"
        );
    }
    assert_eq!(
        metrics.snapshot(Opcode::Open).count,
        1,
        "zero-message open: the kernel sends one OPEN per connection"
    );
    for op in [Opcode::Release, Opcode::Forget, Opcode::BatchForget] {
        let snapshot = metrics.snapshot(op);
        assert_eq!(
            snapshot.max_allocations, 0,
            "{op:?} must not allocate: {snapshot:?}"
        );
    }
    assert!(m.fs.inodes().len() > 1);
    m.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn read_of_local_fragment_reports_allocations_within_budget() {
    skip_without_fuse!();
    let _counting = COUNTING.lock().await;
    let m = Box::pin(mount()).await;
    let root = m.root.clone();

    blocking({
        let root = root.clone();
        move || {
            list_with_attributes(&root);
            read_file(&root.join("warm.bin"));
        }
    })
    .await;
    let reads_before = m.session.metrics().snapshot(Opcode::Read).count;

    lore_base::allocator::set_allocation_counting(true);
    let restore = DisableCountingOnDrop;
    blocking({
        let root = root.clone();
        move || read_file(&root.join("measured.bin"))
    })
    .await;
    drop(restore);

    let read = m.session.metrics().snapshot(Opcode::Read);
    assert!(
        read.count > reads_before,
        "the measured read must reach the provider"
    );
    println!(
        "READ of a local fragment: max_allocations = {} (budget {READ_ALLOCATION_BUDGET})",
        read.max_allocations
    );
    assert!(
        read.max_allocations <= READ_ALLOCATION_BUDGET,
        "READ allocated {} times, over the budget of {READ_ALLOCATION_BUDGET}",
        read.max_allocations
    );
    m.shutdown().await;
}

/// A hot READ of an uncompressed fragmented file under the default strategy, served from the leaves
/// it keeps, allocates nothing.
#[tokio::test(flavor = "current_thread")]
async fn hot_read_of_an_uncompressed_fragmented_file_allocates_zero() {
    skip_without_fuse!();
    let _counting = COUNTING.lock().await;
    let m = Box::pin(mount()).await;
    let path = m.root.join("fragmented.bin");
    // Eight pages span at most two leaves, each at least 32 KiB, which the default keeps.
    let offsets: Arc<Vec<u64>> = Arc::new((0..8).map(|i| (1 << 20) + i * PAGE as u64).collect());
    let expected = Arc::new(fuse_test_util::incompressible(
        FRAGMENTED_LEN,
        FRAGMENTED_SEED,
    ));
    blocking({
        let (path, offsets, expected) = (path.clone(), offsets.clone(), expected.clone());
        move || read_direct(&path, PAGE, &offsets, &expected)
    })
    .await;
    let reads_before = m.session.metrics().snapshot(Opcode::Read).count;

    lore_base::allocator::set_allocation_counting(true);
    let restore = DisableCountingOnDrop;
    blocking(move || read_direct(&path, PAGE, &offsets, &expected)).await;
    drop(restore);

    let read = m.session.metrics().snapshot(Opcode::Read);
    assert!(
        read.count >= reads_before + 8,
        "every measured read reaches the provider"
    );
    assert_eq!(
        read.max_allocations, 0,
        "a hot READ of an uncompressed fragmented file must not allocate: {read:?}"
    );
    m.shutdown().await;
}

/// A writable provider on a read-write mount with one worker, its local changes in `external`.
async fn mount_writable(external: &TempDir) -> Mounted {
    mount_writable_over(external, &[("keep.txt", b"k")]).await
}

/// [`mount_writable`] over a revision of `files`.
async fn mount_writable_over(external: &TempDir, files: &[(&str, &[u8])]) -> Mounted {
    Box::pin(mount_writable_over_revisions(external, &[files])).await
}

/// [`mount_writable`] over the last of `revisions`, one commit each.
async fn mount_writable_over_revisions(
    external: &TempDir,
    revisions: &[&[(&str, &[u8])]],
) -> Mounted {
    let fixture = fuse_fixture::committed_revisions(revisions).await;
    let fs = LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            FuseFilesystem::new_writable(
                fixture.repository.clone(),
                fixture.state.clone(),
                AttrContext::for_mounting_user(MAX_READ, 1_700_000_000_000),
                external.path(),
            ),
        )
        .await
        .expect("writable provider construction");
    let dir = TempDir::new("lore-fuse-allocations-write-");
    let options = MountOptions {
        fsname: "lore:allocations-write".to_string(),
        max_read: MAX_READ,
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
            workers: 1,
            init: InitConfig {
                zero_message_open: true,
                ..InitConfig::read_write()
            },
        },
    )
    .await
    .expect("session must negotiate against a real kernel");
    let notifier = session.notifier().expect("notifier");
    fs.set_notifier(notifier.clone())
        .expect("the notification thread");
    Mounted {
        session,
        fs,
        notifier,
        _guard: guard,
        root: dir.path().to_path_buf(),
        _dir: dir,
        _fixture: fixture,
    }
}

/// `O_DIRECT` WRITEs of `data` at each of `offsets`, each reaching the provider.
fn write_direct(file: &std::fs::File, data: &fuse_test_util::Aligned, offsets: &[u64]) {
    let bytes = data.as_slice();
    for &offset in offsets {
        // SAFETY: the fd is open and `bytes` holds `bytes.len()` readable bytes.
        let n = unsafe {
            libc::pwrite(
                file.as_raw_fd(),
                bytes.as_ptr().cast(),
                bytes.len(),
                offset as libc::off_t,
            )
        };
        assert_eq!(
            n,
            bytes.len() as isize,
            "pwrite: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// Warm READ and WRITE of an open local file allocate nothing, the WRITEs through the write
/// generation's bracket around their host operation.
#[tokio::test(flavor = "current_thread")]
async fn read_and_write_of_an_open_local_file_allocate_zero() {
    skip_without_fuse!();
    let _counting = COUNTING.lock().await;
    let external = TempDir::new("lore-fuse-allocations-local-file-");
    let m = Box::pin(mount_writable(&external)).await;
    let path = m.root.join("output.o");
    let len = 64 << 10;
    let offsets: Vec<u64> = (0..8u64).map(|i| i * len as u64).collect();

    // Warm-up, uncounted: CREATE caches the host file, and the kernel's first WRITE and READ
    // of it land.
    let (file, reader) = blocking({
        let path = path.clone();
        let offsets = offsets.clone();
        move || {
            use std::os::unix::fs::OpenOptionsExt as _;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .custom_flags(libc::O_DIRECT)
                .open(&path)
                .expect("create through the mount");
            let data = fuse_test_util::Aligned::new(len);
            write_direct(&file, &data, &offsets);
            let reader = fuse_test_util::open_direct(&path);
            let buf = fuse_test_util::Aligned::new(len);
            assert_eq!(fuse_test_util::pread(&reader, &buf, len, 0), len);
            (file, reader)
        }
    })
    .await;
    let metrics = m.session.metrics();
    let (writes, reads) = (
        metrics.snapshot(Opcode::Write).count,
        metrics.snapshot(Opcode::Read).count,
    );
    let ino = blocking({
        let path = path.clone();
        move || ino_of(&path)
    })
    .await;
    let generation = m.fs.write_generation(ino).expect("the output's inode");

    lore_base::allocator::set_allocation_counting(true);
    let restore = DisableCountingOnDrop;
    blocking(move || {
        let data = fuse_test_util::Aligned::new(len);
        write_direct(&file, &data, &offsets);
        let buf = fuse_test_util::Aligned::new(len);
        for &offset in &offsets {
            assert_eq!(fuse_test_util::pread(&reader, &buf, len, offset), len);
        }
        // Closing sends FLUSH, which only this runtime's thread can answer.
        drop((file, reader));
    })
    .await;
    drop(restore);

    let write = metrics.snapshot(Opcode::Write);
    let read = metrics.snapshot(Opcode::Read);
    assert!(
        write.count >= writes + 8,
        "every measured WRITE reaches the provider"
    );
    assert!(
        read.count >= reads + 8,
        "every measured READ reaches the provider"
    );
    assert_eq!(
        write.max_allocations, 0,
        "WRITE must not allocate: {write:?}"
    );
    assert_eq!(read.max_allocations, 0, "READ must not allocate: {read:?}");
    assert_eq!(
        m.fs.write_generation(ino),
        Some(generation + 2 * (write.count - writes)),
        "each WRITE moved the write generation around its host operation"
    );
    m.shutdown().await;
}

/// Writes `files` under `external`'s `write/`, as earlier writes through a mount left them.
// The test plants local content outside any repository write, as a mount does.
#[allow(clippy::disallowed_methods)]
fn plant(external: &TempDir, files: &[(&str, &[u8])]) {
    for (path, bytes) in files {
        let at = external.path().join("write").join(path);
        std::fs::create_dir_all(at.parent().expect("a parent")).expect("parent directories");
        std::fs::write(&at, bytes).expect("a local file");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn warm_lookup_getattr_readdirplus_and_read_of_local_entries_allocate_zero() {
    skip_without_fuse!();
    let _counting = COUNTING.lock().await;
    let external = TempDir::new("lore-fuse-allocations-local-");
    let local = contents(3);
    plant(
        &external,
        &[
            ("local.bin", &local),
            ("dir/local.txt", b"local"),
            ("dir/held.txt", b"edited"),
            ("made/inside.txt", b"inside"),
        ],
    );
    let m = Box::pin(mount_writable_over(
        &external,
        &[("keep.txt", b"k"), ("dir/held.txt", b"held")],
    ))
    .await;
    let root = m.root.clone();
    let expected = Arc::new(local);

    // Warm-up, uncounted: builds the listings, creates every inode the measured pass touches
    // and opens their host files.
    let (held, dir_ino) = blocking({
        let root = root.clone();
        let expected = expected.clone();
        move || {
            list_with_attributes(&root);
            list_with_attributes(&root.join("dir"));
            list_with_attributes(&root);
            read_direct(&root.join("local.bin"), FILE_LEN, &[0], &expected);
            let held = std::fs::File::open(root.join("dir/held.txt")).expect("open held");
            forced_getattr(&held);
            (held, ino_of(&root.join("dir")))
        }
    })
    .await;
    // Make the kernel ask again: drop the held file's dentry (its open fd keeps the inode, so
    // the provider still has it) and the root's cached directory pages.
    m.notifier
        .inval_entry(dir_ino, b"held.txt", false)
        .expect("inval_entry");
    m.notifier.inval_inode(ROOT_ID, 0, 0).expect("inval_inode");

    const MEASURED: [Opcode; 4] = [
        Opcode::Lookup,
        Opcode::Getattr,
        Opcode::Readdirplus,
        Opcode::Read,
    ];
    let counts_before = MEASURED.map(|op| m.session.metrics().snapshot(op).count);
    lore_base::allocator::set_allocation_counting(true);
    let restore = DisableCountingOnDrop;
    let held = blocking({
        let root = root.clone();
        let expected = expected.clone();
        move || {
            std::fs::symlink_metadata(root.join("dir/held.txt")).expect("lookup");
            forced_getattr(&held);
            list_with_attributes(&root);
            read_direct(&root.join("local.bin"), FILE_LEN, &[0], &expected);
            held
        }
    })
    .await;
    drop(restore);
    drop(held);

    let metrics = m.session.metrics();
    for (op, before) in MEASURED.iter().zip(counts_before) {
        let snapshot = metrics.snapshot(*op);
        assert!(
            snapshot.count > before,
            "{op:?} must run in the measured pass: {snapshot:?}"
        );
        assert_eq!(
            snapshot.max_allocations, 0,
            "{op:?} of a local entry must not allocate: {snapshot:?}"
        );
    }
    m.shutdown().await;
}

/// Warm CREATEs in a tree that records file metadata, two commits in, allocate as many times as
/// in a tree that records none, within CREATE's budget: new nodes take fresh blocks, so neither
/// reads file metadata.
#[tokio::test(flavor = "current_thread")]
async fn create_over_a_tree_recording_file_metadata_stays_within_its_budget() {
    skip_without_fuse!();
    let _counting = COUNTING.lock().await;
    let first: [(&str, &[u8]); 2] = [("tracked.o", b"one"), ("other.o", b"o")];
    let second: [(&str, &[u8]); 1] = [("tracked.o", b"two")];
    let plain = Box::pin(create_max_allocations(&[&first], false)).await;
    let recorded = Box::pin(create_max_allocations(&[&first, &second], true)).await;
    println!(
        "CREATE max_allocations: {recorded} over a tree recording file metadata, {plain} over one \
         recording none (budget {CREATE_ALLOCATION_BUDGET})"
    );
    assert_eq!(
        recorded, plain,
        "file metadata adds no allocation to a CREATE"
    );
    assert!(
        recorded <= CREATE_ALLOCATION_BUDGET,
        "CREATE allocated {recorded} times, over the budget of {CREATE_ALLOCATION_BUDGET}"
    );
}

/// The most allocations one of a run of warm CREATEs makes on a mount of the last of
/// `revisions`, whose tree records file metadata as `records_file_metadata` says.
// The CREATEs go through the mount, as a user's do.
#[allow(clippy::disallowed_methods)]
async fn create_max_allocations(
    revisions: &[&[(&str, &[u8])]],
    records_file_metadata: bool,
) -> u64 {
    const ROUNDS: usize = 8;
    let external = TempDir::new("lore-fuse-allocations-metadata-");
    let m = Box::pin(mount_writable_over_revisions(&external, revisions)).await;
    let served = m.fs.served();
    let tree = LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            served.state.tree(served.repository.clone()),
        )
        .await
        .expect("the tree");
    assert_eq!(
        !tree.hash_file_metadata.is_zero(),
        records_file_metadata,
        "the tree records file metadata"
    );
    let root = m.root.clone();

    // Warm-up, uncounted: the root's listing is built, and a CREATE takes the first fresh block.
    blocking({
        let root = root.clone();
        move || drop(std::fs::File::create_new(root.join("warm.o")).expect("create"))
    })
    .await;
    let metrics = m.session.metrics();
    let before = metrics.snapshot(Opcode::Create).count;

    lore_base::allocator::set_allocation_counting(true);
    let restore = DisableCountingOnDrop;
    blocking({
        let root = root.clone();
        move || {
            for i in 0..ROUNDS {
                drop(std::fs::File::create_new(root.join(format!("new-{i}.o"))).expect("create"));
            }
        }
    })
    .await;
    drop(restore);

    let create = metrics.snapshot(Opcode::Create);
    assert!(
        create.count >= before + ROUNDS as u64,
        "every CREATE is counted"
    );
    m.shutdown().await;
    create.max_allocations
}

/// CREATE, UNLINK and RENAME, of local files and over and of tracked ones, each allocate within
/// their measured budget, and so does UNLINK of a tracked file a write copied up. The most one
/// request made only rises, so UNLINK is measured in passes of increasing budget: tracked
/// files, then local ones, then copied-up ones.
#[tokio::test(flavor = "current_thread")]
// The test changes names through the mount, as a user does.
#[allow(clippy::disallowed_methods)]
async fn create_unlink_and_rename_stay_within_their_budget() {
    skip_without_fuse!();
    const ROUNDS: usize = 8;
    let _counting = COUNTING.lock().await;
    let external = TempDir::new("lore-fuse-allocations-namespace-");
    let tracked: Vec<(String, Vec<u8>)> = (0..3 * ROUNDS + 1)
        .map(|i| (format!("tracked-{i}.o"), contents(i as u8)))
        .collect();
    let files: Vec<(&str, &[u8])> = tracked
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect();
    let m = Box::pin(mount_writable_over(&external, &files)).await;
    let root = m.root.clone();

    // Warm-up, uncounted: the root's listing is built, and one of each request lands.
    blocking({
        let root = root.clone();
        move || {
            std::fs::write(root.join("warm.tmp"), b"w").expect("create");
            std::fs::rename(root.join("warm.tmp"), root.join("tracked-0.o")).expect("rename over");
            std::fs::remove_file(root.join("tracked-0.o")).expect("unlink");
        }
    })
    .await;
    // Copied up uncounted, so only their UNLINKs are measured.
    blocking({
        let root = root.clone();
        let write = external.path().join("write");
        move || {
            use std::io::Write as _;
            for i in 1..=ROUNDS {
                let name = format!("tracked-{}.o", 2 * ROUNDS + i);
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(root.join(&name))
                    .expect("open for writing")
                    .write_all(b"more")
                    .expect("write");
                assert!(write.join(&name).exists(), "{name} is copied up");
            }
        }
    })
    .await;
    let metrics = m.session.metrics();
    let before = [
        Opcode::Create,
        Opcode::Unlink,
        Opcode::Rename,
        Opcode::Rename2,
    ]
    .map(|op| metrics.snapshot(op).count);

    lore_base::allocator::set_allocation_counting(true);
    let restore = DisableCountingOnDrop;
    blocking({
        let root = root.clone();
        move || {
            for i in 1..=ROUNDS {
                std::fs::remove_file(root.join(format!("tracked-{}.o", ROUNDS + i)))
                    .expect("unlink tracked");
            }
        }
    })
    .await;
    let tracked_unlink = metrics.snapshot(Opcode::Unlink);
    blocking({
        let root = root.clone();
        move || {
            for i in 1..=ROUNDS {
                let tmp = root.join(format!("save-{i}.tmp"));
                drop(
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&tmp)
                        .expect("create"),
                );
                std::fs::rename(&tmp, root.join(format!("tracked-{i}.o"))).expect("rename over");
                let free = root.join(format!("free-{i}.tmp"));
                drop(std::fs::File::create_new(&free).expect("create"));
                std::fs::rename(&free, root.join(format!("renamed-{i}.o"))).expect("rename");
                // A stat of a name the kernel holds no dentry for first, so a rebuild of the
                // listing an overflow of the root's log left stale lands in LOOKUP.
                assert!(std::fs::symlink_metadata(root.join(format!("absent-{i}"))).is_err());
                std::fs::remove_file(root.join(format!("renamed-{i}.o"))).expect("unlink local");
            }
        }
    })
    .await;
    let local_unlink = metrics.snapshot(Opcode::Unlink);
    blocking({
        let root = root.clone();
        move || {
            for i in 1..=ROUNDS {
                std::fs::remove_file(root.join(format!("tracked-{}.o", 2 * ROUNDS + i)))
                    .expect("unlink copied up");
            }
        }
    })
    .await;
    drop(restore);

    let [creates, unlinks, renames, renames2] = before;
    let create = metrics.snapshot(Opcode::Create);
    let unlink = metrics.snapshot(Opcode::Unlink);
    let rename = metrics.snapshot(Opcode::Rename);
    let rename2 = metrics.snapshot(Opcode::Rename2);
    assert!(
        create.count >= creates + 2 * ROUNDS as u64,
        "every CREATE is counted"
    );
    assert!(
        unlink.count >= unlinks + 3 * ROUNDS as u64,
        "every UNLINK is counted"
    );
    assert!(
        rename.count + rename2.count >= renames + renames2 + 2 * ROUNDS as u64,
        "every RENAME is counted"
    );
    let rename_max = rename.max_allocations.max(rename2.max_allocations);
    println!(
        "max_allocations: CREATE {} (budget {CREATE_ALLOCATION_BUDGET}), UNLINK of a tracked \
         file {} (budget {TRACKED_UNLINK_ALLOCATION_BUDGET}), of a local one {} (budget \
         {LOCAL_UNLINK_ALLOCATION_BUDGET}), of a copied-up one {} (budget \
         {COPIED_UP_UNLINK_ALLOCATION_BUDGET}), RENAME {rename_max} (budget \
         {RENAME_ALLOCATION_BUDGET})",
        create.max_allocations,
        tracked_unlink.max_allocations,
        local_unlink.max_allocations,
        unlink.max_allocations
    );
    assert!(
        create.max_allocations <= CREATE_ALLOCATION_BUDGET
            && tracked_unlink.max_allocations <= TRACKED_UNLINK_ALLOCATION_BUDGET
            && local_unlink.max_allocations <= LOCAL_UNLINK_ALLOCATION_BUDGET
            && unlink.max_allocations <= COPIED_UP_UNLINK_ALLOCATION_BUDGET
            && rename_max <= RENAME_ALLOCATION_BUDGET,
        "CREATE {create:?}, UNLINK of tracked {tracked_unlink:?}, of local {local_unlink:?}, \
         of copied-up {unlink:?}, RENAME {rename:?}, RENAME2 {rename2:?}"
    );
    m.shutdown().await;
}

/// Warm LOOKUP and GETATTR of a file in a directory MKDIR made, answered through the empty
/// listing MKDIR cached and the nodes attached to it since, then warm READDIRPLUS once the
/// listing is rebuilt, allocate nothing.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::disallowed_methods)]
async fn warm_lookup_getattr_and_readdirplus_in_a_directory_mkdir_made_allocate_zero() {
    skip_without_fuse!();
    let _counting = COUNTING.lock().await;
    let external = TempDir::new("lore-fuse-allocations-mkdir-");
    let m = Box::pin(mount_writable(&external)).await;
    let made_path = m.root.join("made");
    let file_path = made_path.join("new.bin");
    // The root's listing is built before the count is taken.
    blocking({
        let root = m.root.clone();
        move || {
            std::fs::symlink_metadata(root.join("keep.txt")).expect("keep.txt");
        }
    })
    .await;
    let built = m.fs.listings_built();
    let (held, made_ino) = blocking({
        let (made_path, file_path) = (made_path.clone(), file_path.clone());
        move || {
            std::fs::create_dir(&made_path).expect("mkdir");
            std::fs::write(&file_path, contents(5)).expect("create and write");
            let held = std::fs::File::open(&file_path).expect("open");
            forced_getattr(&held);
            (held, ino_of(&made_path))
        }
    })
    .await;
    // Warm-up, uncounted: one LOOKUP of the name once the kernel holds no dentry for it.
    m.notifier
        .inval_entry(made_ino, b"new.bin", false)
        .expect("inval_entry");
    blocking({
        let file_path = file_path.clone();
        move || {
            std::fs::symlink_metadata(&file_path).expect("lookup");
        }
    })
    .await;
    m.notifier
        .inval_entry(made_ino, b"new.bin", false)
        .expect("inval_entry");

    let metrics = m.session.metrics();
    let before = [Opcode::Lookup, Opcode::Getattr].map(|op| metrics.snapshot(op).count);
    lore_base::allocator::set_allocation_counting(true);
    let restore = DisableCountingOnDrop;
    let held = blocking({
        let file_path = file_path.clone();
        move || {
            std::fs::symlink_metadata(&file_path).expect("lookup");
            forced_getattr(&held);
            held
        }
    })
    .await;
    drop(restore);
    assert_eq!(
        m.fs.listings_built(),
        built,
        "a listing of the directory MKDIR made was built"
    );
    for (op, before) in [Opcode::Lookup, Opcode::Getattr].into_iter().zip(before) {
        let snapshot = metrics.snapshot(op);
        assert!(
            snapshot.count > before,
            "{op:?} must have run: {snapshot:?}"
        );
        assert_eq!(
            snapshot.max_allocations, 0,
            "{op:?} in a directory MKDIR made must not allocate: {snapshot:?}"
        );
    }

    // Warm-up, uncounted: the READDIRPLUS that rebuilds the listing the CREATE left behind.
    blocking({
        let made_path = made_path.clone();
        move || list_with_attributes(&made_path)
    })
    .await;
    // The kernel answers the next listing from its cache unless the directory is invalidated.
    m.notifier
        .inval_entry(made_ino, b"new.bin", false)
        .expect("inval_entry");
    m.notifier.inval_inode(made_ino, 0, 0).expect("inval_inode");
    let before = metrics.snapshot(Opcode::Readdirplus).count;
    lore_base::allocator::set_allocation_counting(true);
    let restore = DisableCountingOnDrop;
    blocking({
        let made_path = made_path.clone();
        move || list_with_attributes(&made_path)
    })
    .await;
    drop(restore);
    drop(held);
    let readdirplus = metrics.snapshot(Opcode::Readdirplus);
    assert!(
        readdirplus.count > before,
        "READDIRPLUS must have run: {readdirplus:?}"
    );
    assert_eq!(
        readdirplus.max_allocations, 0,
        "READDIRPLUS of a directory MKDIR made must not allocate: {readdirplus:?}"
    );
    m.shutdown().await;
}
