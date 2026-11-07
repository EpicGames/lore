// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::mem::size_of;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_base::test_util::TempDir;
use lore_fuse::abi::Attr;
use lore_fuse::abi::GetattrIn;
use lore_fuse::abi::InHeader;
use lore_fuse::abi::InitIn;
use lore_fuse::abi::KERNEL_MAJOR;
use lore_fuse::abi::Kstatfs;
use lore_fuse::abi::Opcode;
use lore_fuse::abi::OutHeader;
use lore_fuse::abi::REQUIRED_MINOR;
use lore_fuse::abi::ROOT_ID;
use lore_fuse::abi::WriteIn;
use lore_fuse::abi::WriteOut;
use lore_fuse::abi::init_flags;
use lore_fuse::codec::dirent::DirentWriter;
use lore_fuse::filesystem::Filesystem;
use lore_fuse::filesystem::RequestInfo;
use lore_fuse::init::InitConfig;
use lore_fuse::init::REQUIRED_FLAGS;
use lore_fuse::mount::MountOptions;
use lore_fuse::mount::mount;
use lore_fuse::mount::unmount_path;
use lore_fuse::reply::AttrReply;
use lore_fuse::reply::EntryReply;
use lore_fuse::reply::Errno;
use lore_fuse::reply::OpenReply;
use lore_fuse::reply::ReadFault;
use lore_fuse::reply::ReplyOutcome;
use lore_fuse::session::Session;
use lore_fuse::session::SessionConfig;
use lore_fuse::skip_without_fuse;
use lore_fuse::transport::device::DeviceTransport;
use lore_fuse::transport::mock::MockTransport;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

use crate::test_stub::StubFilesystem;

/// Builds one raw request buffer: an `InHeader` followed by `body`'s raw bytes.
fn header_and_body(opcode: Opcode, unique: u64, body: &[u8]) -> Vec<u8> {
    let total = size_of::<InHeader>() + body.len();
    let header = InHeader {
        len: total as u32,
        opcode: opcode as u32,
        unique,
        nodeid: 0,
        uid: 0,
        gid: 0,
        pid: 0,
        total_extlen: 0,
        padding: 0,
    };
    let mut bytes = header.as_bytes().to_vec();
    bytes.extend_from_slice(body);
    bytes
}

fn init_in_body() -> Vec<u8> {
    let (flags, flags2) = init_flags::split(REQUIRED_FLAGS);
    let body = InitIn {
        major: KERNEL_MAJOR,
        minor: REQUIRED_MINOR,
        max_readahead: 0,
        flags,
        flags2,
        unused: [0; 11],
    };
    body.as_bytes().to_vec()
}

/// A fresh `Arc<Arc<MockTransport>>`: `Transport` is implemented for `Arc<MockTransport>`
/// (see `transport/mock.rs`), so `Session<F, Arc<MockTransport>>` needs one more `Arc` layer
/// around it, matching `Session::start`'s `transport: Arc<T>` parameter. Test-only, and
/// structurally forced by `MockTransport`'s own (pre-existing, out of this task's scope)
/// choice to implement `Transport` for `Arc<Self>` rather than `Self`; clippy's usual
/// "redundant allocation" reasoning does not apply since there is no single-`Arc` type that
/// would satisfy `Session`'s `T: Transport` bound here.
#[allow(clippy::redundant_allocation)]
fn mock_transport() -> Arc<Arc<MockTransport>> {
    Arc::new(MockTransport::new())
}

#[tokio::test(flavor = "multi_thread")]
async fn start_negotiates_init_on_worker_zero_and_replies() {
    let transport = mock_transport();
    transport.push_request(header_and_body(Opcode::Init, 1, &init_in_body()));

    let fs = Arc::new(StubFilesystem::default());
    let session = Session::start(
        fs,
        Arc::clone(&transport),
        SessionConfig {
            workers: 1,
            init: InitConfig::default(),
        },
    )
    .await
    .expect("INIT must negotiate against a well-formed request");

    assert_eq!(session.negotiated().minor, REQUIRED_MINOR);

    session.shutdown().await;

    let replies = transport.replies.lock();
    assert_eq!(replies.len(), 1, "INIT must get exactly one reply");
    assert_eq!(replies[0].0, 0, "INIT's reply must go through worker 0");
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_loop_dispatches_requests_after_init() {
    let transport = mock_transport();
    transport.push_request(header_and_body(Opcode::Init, 1, &init_in_body()));

    let fs = Arc::new(StubFilesystem::default());
    let calls_before = fs.calls.load(Ordering::Relaxed);

    let session = Session::start(
        Arc::clone(&fs),
        Arc::clone(&transport),
        SessionConfig {
            workers: 1,
            init: InitConfig::default(),
        },
    )
    .await
    .expect("INIT must negotiate");

    transport.push_request(header_and_body(
        Opcode::Getattr,
        2,
        GetattrIn::default().as_bytes(),
    ));

    // Give the worker loop a chance to pick up and dispatch the queued GETATTR.
    for _ in 0..200 {
        if transport.replies.lock().len() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    session.shutdown().await;

    assert!(
        fs.calls.load(Ordering::Relaxed) > calls_before,
        "the dispatched GETATTR must have reached the stub filesystem"
    );
    assert_eq!(
        transport.replies.lock().len(),
        2,
        "INIT plus one GETATTR reply"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_loop_serves_a_write_larger_than_the_default_request_buffer() {
    let transport = mock_transport();
    transport.push_request(header_and_body(Opcode::Init, 1, &init_in_body()));
    let fs = Arc::new(StubFilesystem::default());
    let session = Session::start(
        Arc::clone(&fs),
        Arc::clone(&transport),
        SessionConfig {
            workers: 1,
            init: InitConfig::read_write(),
        },
    )
    .await
    .expect("INIT must negotiate");
    assert_eq!(session.negotiated().max_write, 1 << 20);

    let payload = vec![7u8; 256 * 1024];
    let mut body = WriteIn {
        size: payload.len() as u32,
        ..WriteIn::default()
    }
    .as_bytes()
    .to_vec();
    body.extend_from_slice(&payload);
    transport.push_request(header_and_body(Opcode::Write, 2, &body));

    for _ in 0..200 {
        if transport.replies.lock().len() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    session.shutdown().await;

    let replies = transport.replies.lock();
    assert_eq!(replies.len(), 2, "INIT plus one WRITE reply");
    let (_, rest) = OutHeader::read_from_prefix(&replies[1].1).expect("reply header");
    let (written, _) = WriteOut::read_from_prefix(rest).expect("write reply body");
    assert_eq!(written.size as usize, payload.len());
}

/// A request whose header parses but whose body does not is answered, so the calling
/// process is not left waiting: an overlong name with `ENAMETOOLONG`, any other body `EIO`.
#[tokio::test(flavor = "multi_thread")]
async fn undecodable_body_is_answered_with_an_errno() {
    let transport = mock_transport();
    transport.push_request(header_and_body(Opcode::Init, 1, &init_in_body()));
    let fs = Arc::new(StubFilesystem::default());
    let session = Session::start(
        Arc::clone(&fs),
        Arc::clone(&transport),
        SessionConfig {
            workers: 1,
            init: InitConfig::default(),
        },
    )
    .await
    .expect("INIT must negotiate");

    let mut long_name = vec![b'a'; 300];
    long_name.push(0);
    transport.push_request(header_and_body(Opcode::Lookup, 2, &long_name));
    transport.push_request(header_and_body(Opcode::Getattr, 3, &[0u8; 3]));

    for _ in 0..200 {
        if transport.replies.lock().len() >= 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    session.shutdown().await;

    let replies = transport.replies.lock();
    assert_eq!(
        replies.len(),
        3,
        "INIT plus one reply per undecodable request"
    );
    let errors: Vec<(u64, i32)> = replies[1..]
        .iter()
        .map(|(_, bytes)| {
            let (header, _) = OutHeader::read_from_prefix(bytes).expect("reply header");
            (header.unique, header.error)
        })
        .collect();
    assert_eq!(
        errors,
        vec![(2, -Errno::ENAMETOOLONG.0), (3, -Errno::EIO.0)]
    );
}

/// A LOOKUP that needs a fetch waits off the worker, so a session with one worker answers
/// the requests behind it first.
#[tokio::test(flavor = "multi_thread")]
async fn a_deferred_lookup_leaves_the_only_worker_free() {
    let transport = mock_transport();
    transport.push_request(header_and_body(Opcode::Init, 1, &init_in_body()));
    let fs = Arc::new(StubFilesystem::default());
    fs.defer_listing.store(true, Ordering::SeqCst);
    fs.hang_listing.store(true, Ordering::SeqCst);
    let session = Session::start(
        Arc::clone(&fs),
        Arc::clone(&transport),
        SessionConfig {
            workers: 1,
            init: InitConfig::default(),
        },
    )
    .await
    .expect("INIT must negotiate");

    transport.push_request(header_and_body(Opcode::Lookup, 2, b"cold\0"));
    transport.push_request(header_and_body(
        Opcode::Getattr,
        3,
        GetattrIn::default().as_bytes(),
    ));
    let answered = |unique: u64| {
        transport.replies.lock().iter().any(|(_, bytes)| {
            OutHeader::read_from_prefix(bytes).is_ok_and(|(header, _)| header.unique == unique)
        })
    };
    for _ in 0..200 {
        if answered(3) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let getattr_first = answered(3) && !answered(2);

    fs.listing_gate.notify_one();
    for _ in 0..200 {
        if answered(2) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let lookup_answered = answered(2);
    session.shutdown().await;

    assert!(
        getattr_first,
        "GETATTR must be answered while the LOOKUP waits"
    );
    assert!(lookup_answered, "the deferred LOOKUP must be answered");
}

/// A worker that fails to open after others started leaves none of them running.
#[tokio::test(flavor = "multi_thread")]
async fn failed_worker_spawn_stops_the_spawned_workers() {
    let transport = mock_transport();
    transport.fail_worker.store(2 + 1, Ordering::SeqCst);
    transport.push_request(header_and_body(Opcode::Init, 1, &init_in_body()));

    let result = Session::start(
        Arc::new(StubFilesystem::default()),
        Arc::clone(&transport),
        SessionConfig {
            workers: 4,
            init: InitConfig::default(),
        },
    )
    .await;

    assert!(
        result.is_err(),
        "the third worker's failure must fail start"
    );
    assert_eq!(
        Arc::strong_count(&transport),
        1,
        "no worker may still hold the session"
    );
    assert_eq!(
        Arc::strong_count(&*transport),
        1,
        "no worker may still hold its transport worker"
    );
}

#[test]
fn default_session_config_caps_workers_at_eight() {
    let config = SessionConfig::default();
    assert!(config.workers >= 1);
    assert!(config.workers <= 8);
}

// `Session` driving the real `/dev/fuse` device transport end to end, against a trivial root-only
// `Filesystem`.

/// Lazily unmounts `path` on drop, so a test that panics midway never leaves a mount behind.
struct PathUnmountGuard(PathBuf);

impl Drop for PathUnmountGuard {
    fn drop(&mut self) {
        let _ = unmount_path(&self.0, true);
    }
}

/// Unmounts `path` lazily, retrying briefly on failure. A `GETATTR`/`stat` against the mount
/// just before unmounting can leave the kernel's vfsmount reference count settling
/// asynchronously, which a non-lazy unmount surfaces as `EBUSY` ("Device or resource busy").
/// `fusermount3`'s non-lazy fallback path (raw `umount2(2)`) also requires `CAP_SYS_ADMIN`, so
/// once `fusermount3` itself reports busy, retrying a non-lazy unmount never converges for an
/// unprivileged process -- it just trades one permanent error (`EBUSY`) for another (`EPERM`).
/// Lazy unmount (`fusermount3 -u -z` / `MNT_DETACH`) sidesteps the busy check entirely: it
/// detaches the mount from the namespace immediately and lets the kernel finish tearing it down
/// once the last reference drops, which is also why `PathUnmountGuard` already uses `lazy: true`.
/// Calls the free `unmount_path` function directly rather than `Mount::unmount`, since the
/// latter's one-shot `mounted` flag would turn every retry after the first into a silent no-op.
async fn unmount_with_retry(path: &std::path::Path) {
    const ATTEMPTS: u32 = 10;
    for attempt in 0..ATTEMPTS {
        match unmount_path(path, true) {
            Ok(()) => return,
            Err(_) if attempt + 1 < ATTEMPTS => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => panic!("unmount: {error}"),
        }
    }
}

/// Answers `GETATTR` for the root inode as a directory and nothing else; every other method
/// this test never exercises still needs a total, non-panicking body per the trait.
#[derive(Default)]
struct RootOnlyFilesystem;

impl Filesystem for RootOnlyFilesystem {
    async fn lookup(
        &self,
        _req: &RequestInfo,
        _parent: u64,
        _name: &[u8],
    ) -> Result<EntryReply, Errno> {
        Err(Errno::ENOENT)
    }

    fn forget(&self, _ino: u64, _nlookup: u64) {}

    async fn getattr(&self, _req: &RequestInfo, ino: u64) -> Result<AttrReply, Errno> {
        if ino == ROOT_ID {
            Ok(AttrReply {
                attr: Attr {
                    ino: ROOT_ID,
                    mode: libc::S_IFDIR | 0o755,
                    nlink: 2,
                    ..Default::default()
                },
                timeout: Duration::ZERO,
            })
        } else {
            Err(Errno::ENOENT)
        }
    }

    fn statfs(&self, _req: &RequestInfo) -> Result<Kstatfs, Errno> {
        Ok(Kstatfs::default())
    }

    fn opendir(&self, _req: &RequestInfo, _ino: u64) -> Result<OpenReply, Errno> {
        Ok(OpenReply { fh: 0, flags: 0 })
    }

    async fn readdir(
        &self,
        _req: &RequestInfo,
        _ino: u64,
        _fh: u64,
        _offset: u64,
        _out: &mut DirentWriter<'_>,
    ) -> Result<(), Errno> {
        Ok(())
    }

    async fn readdirplus(
        &self,
        _req: &RequestInfo,
        _ino: u64,
        _fh: u64,
        _offset: u64,
        _out: &mut DirentWriter<'_>,
    ) -> Result<usize, Errno> {
        Ok(0)
    }

    fn releasedir(&self, _ino: u64, _fh: u64) {}

    fn open(&self, _req: &RequestInfo, _ino: u64, _flags: u32) -> Result<OpenReply, Errno> {
        Ok(OpenReply { fh: 0, flags: 0 })
    }

    async fn read(
        &self,
        _req: &RequestInfo,
        _ino: u64,
        _fh: u64,
        _offset: u64,
        _out: &mut [u8],
        _fetch: bool,
    ) -> Result<usize, ReadFault> {
        Ok(0)
    }

    fn release(&self, _ino: u64, _fh: u64) {}

    fn reply_failed(&self, _req: &RequestInfo, _outcome: &ReplyOutcome) {}
}

fn session_config() -> SessionConfig {
    SessionConfig {
        workers: 1,
        init: InitConfig::default(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stat_of_root_after_init_is_dir() {
    skip_without_fuse!();

    let dir = TempDir::new("lore-fuse-session-stat-test-");
    let options = MountOptions {
        fsname: "lore:session-stat".to_string(),
        max_read: 1 << 20,
        fusermount: None,
    };

    let mount_handle = Arc::new(mount(dir.path(), &options).expect("mount"));
    let guard = PathUnmountGuard(dir.path().to_path_buf());

    let transport = Arc::new(DeviceTransport::new(Arc::clone(&mount_handle)));
    let fs = Arc::new(RootOnlyFilesystem);

    let session = Session::start(fs, transport, session_config())
        .await
        .expect("INIT must negotiate against a real kernel");

    let path = dir.path().to_path_buf();
    let metadata = lore_base::lore_spawn_blocking!(move || std::fs::metadata(&path))
        .await
        .expect("spawn_blocking must not panic")
        .expect("stat must succeed against the mounted root");

    assert!(
        metadata.is_dir(),
        "the mount root must report as a directory"
    );

    session.shutdown().await;
    unmount_with_retry(dir.path()).await;
    drop(guard);
}

#[tokio::test(flavor = "multi_thread")]
async fn session_ends_after_unmount() {
    skip_without_fuse!();

    let dir = TempDir::new("lore-fuse-session-unmount-test-");
    let options = MountOptions {
        fsname: "lore:session-unmount".to_string(),
        max_read: 1 << 20,
        fusermount: None,
    };

    let mount_handle = Arc::new(mount(dir.path(), &options).expect("mount"));
    let guard = PathUnmountGuard(dir.path().to_path_buf());

    let transport = Arc::new(DeviceTransport::new(Arc::clone(&mount_handle)));
    let fs = Arc::new(RootOnlyFilesystem);

    let session = Session::start(fs, transport, session_config())
        .await
        .expect("INIT must negotiate against a real kernel");

    unmount_with_retry(dir.path()).await;

    tokio::time::timeout(Duration::from_secs(5), session.wait())
        .await
        .expect("the worker loop must end once the device disconnects");

    drop(guard);
}
