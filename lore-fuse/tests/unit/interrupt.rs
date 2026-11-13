// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::io::Read;
use std::mem::size_of;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_base::test_util::TempDir;
use lore_fuse::abi::InHeader;
use lore_fuse::abi::Opcode;
use lore_fuse::abi::OutHeader;
use lore_fuse::codec::request::decode;
use lore_fuse::dispatch::DeferredInterrupts;
use lore_fuse::dispatch::dispatch;
use lore_fuse::init::InitConfig;
use lore_fuse::interrupt::*;
use lore_fuse::metrics::Metrics;
use lore_fuse::mount::unmount_path;
use lore_fuse::reply::Errno;
use lore_fuse::reply::ReadFault;
use lore_fuse::session::SessionConfig;
use lore_fuse::skip_without_fuse;
use lore_fuse::transport::Transport;
use lore_fuse::transport::mock::MockTransport;
use tokio::sync::Notify;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

use crate::synthetic::SynthNode;
use crate::synthetic::SyntheticFs;
use crate::synthetic::mount_synthetic;
use crate::test_stub::StubFilesystem;

#[test]
fn slot_begin_end_never_allocates() {
    let slots = InFlightSlots::new(4);
    // Warm up: the first begin/end pair may initialize static data lazily.
    {
        let _guard = slots.begin(0, 1);
    }

    lore_base::allocator::set_allocation_counting(true);
    let before = lore_base::allocator::thread_allocation_count();
    {
        let _guard = slots.begin(0, 42);
    }
    let after = lore_base::allocator::thread_allocation_count();
    lore_base::allocator::set_allocation_counting(false);

    assert_eq!(after, before, "InFlightSlots::begin/drop must not allocate");
}

/// An INTERRUPT that lands after the slot is claimed but before the request's `select!` first
/// polls `interrupted` still interrupts it.
#[tokio::test(flavor = "current_thread")]
async fn an_interrupt_before_the_first_poll_is_not_lost() {
    let slots = InFlightSlots::new(1);
    let guard = slots.begin(0, 7);
    assert!(slots.interrupt(7), "the claimed slot holds the unique");

    tokio::time::timeout(Duration::from_secs(1), guard.interrupted())
        .await
        .expect("the interrupt that came before the first poll was lost");
}

/// An INTERRUPT of a request that has finished does not interrupt the next request the worker
/// takes.
#[tokio::test(flavor = "current_thread")]
async fn an_interrupt_of_an_earlier_request_does_not_reach_the_next() {
    let slots = InFlightSlots::new(1);
    let first = slots.begin(0, 7);
    assert!(slots.interrupt(7));
    drop(first);
    assert!(!slots.interrupt(7), "the finished request's unique is gone");

    let second = slots.begin(0, 8);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), second.interrupted())
            .await
            .is_err(),
        "the earlier request's interrupt reached the next one"
    );
}

fn header_and_body(opcode: u32, unique: u64, body: &[u8]) -> Vec<u8> {
    let header = InHeader {
        len: (size_of::<InHeader>() + body.len()) as u32,
        opcode,
        unique,
        nodeid: 1,
        uid: 0,
        gid: 0,
        pid: 0,
        total_extlen: 0,
        padding: 0,
    };
    let mut buffer = header.as_bytes().to_vec();
    buffer.extend_from_slice(body);
    buffer
}

/// Decodes just enough of a reply to read its errno, mirroring `dispatch.rs`'s own test
/// helper (private to that module, so duplicated here rather than shared).
fn reply_errno(bytes: &[u8]) -> i32 {
    let (out_header, _) = OutHeader::ref_from_prefix(bytes).expect("reply has a header");
    out_header.error
}

#[tokio::test(flavor = "current_thread")]
async fn interrupt_of_unknown_unique_replies_eagain() {
    let fs = StubFilesystem::new();
    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());

    // Nothing has ever registered unique 999 in either `slots` or `deferred`.
    let buffer = header_and_body(Opcode::Interrupt as u32, 1, &999u64.to_le_bytes());
    let decoded = decode(&buffer).expect("valid request");
    let mut scratch = [0u8; 4096];
    let worker_index = worker.index();
    dispatch(
        &fs,
        &mut worker,
        worker_index,
        decoded.header,
        decoded.request,
        &metrics,
        &slots,
        &deferred,
        &mut scratch,
    )
    .await
    .expect("dispatch must not hit a transport error against MockTransport");

    let replies = transport.replies.lock();
    assert_eq!(replies.len(), 1, "an unknown target must still get a reply");
    assert_eq!(reply_errno(&replies[0].1), -Errno::EAGAIN.0);
}

#[tokio::test]
async fn interrupt_of_deferred_read_replies_eintr() {
    let fs = StubFilesystem::new();
    fs.read_results.lock().push_back(Err(ReadFault::WouldFetch));
    // Keeps the deferred task's `fetch: true` retry hanging, so it can only resolve via the
    // interrupt below, not by racing ahead and completing on its own.
    fs.hang_fetch_read.store(true, Ordering::SeqCst);

    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());

    let read_buffer = header_and_body(Opcode::Read as u32, 9, &[0u8; 40]);
    let decoded = decode(&read_buffer).expect("valid request");
    let mut scratch = [0u8; 4096];
    let worker_index = worker.index();
    dispatch(
        &fs,
        &mut worker,
        worker_index,
        decoded.header,
        decoded.request,
        &metrics,
        &slots,
        &deferred,
        &mut scratch,
    )
    .await
    .expect("dispatch must not hit a transport error against MockTransport");

    // The read has been deferred (`WouldFetch`): no reply yet, but `deferred.register` runs
    // synchronously inside `defer_read`, before the retry is even spawned, so the unique is
    // already there by the time `dispatch` above returns -- no polling/retry needed here.
    assert_eq!(transport.replies.lock().len(), 0);
    assert!(
        deferred.interrupt(9),
        "the deferred read must already be registered"
    );

    for _ in 0..50 {
        if transport.replies.lock().len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let replies = transport.replies.lock();
    assert_eq!(
        replies.len(),
        1,
        "the interrupted deferred read must still reply"
    );
    assert_eq!(reply_errno(&replies[0].1), -Errno::EINTR.0);
}

#[tokio::test]
async fn interrupt_of_deferred_lookup_replies_eintr_and_forgets_the_unique() {
    let fs = StubFilesystem::new();
    fs.defer_listing.store(true, Ordering::SeqCst);
    // The full lookup never completes on its own: only the interrupt can end it.
    fs.hang_listing.store(true, Ordering::SeqCst);

    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());

    let lookup = header_and_body(Opcode::Lookup as u32, 9, b"cold\0");
    let decoded = decode(&lookup).expect("valid request");
    let mut scratch = [0u8; 4096];
    let worker_index = worker.index();
    let dispatched = tokio::time::timeout(
        Duration::from_secs(2),
        dispatch(
            &fs,
            &mut worker,
            worker_index,
            decoded.header,
            decoded.request,
            &metrics,
            &slots,
            &deferred,
            &mut scratch,
        ),
    )
    .await;
    assert!(dispatched.is_ok(), "the LOOKUP must be deferred");

    assert_eq!(transport.replies.lock().len(), 0);
    assert!(deferred.interrupt(9), "the deferred LOOKUP is registered");

    for _ in 0..200 {
        if transport.replies.lock().len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(transport.replies.lock().len(), 1);
    assert_eq!(reply_errno(&transport.replies.lock()[0].1), -Errno::EINTR.0);
    for _ in 0..200 {
        if !deferred.interrupt(9) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!deferred.interrupt(9), "an answered unique is forgotten");
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupt_of_in_flight_unique_cancels_future_and_replies_eintr() {
    let fs = StubFilesystem::new();
    // The inline (non-fetch) read hangs on `local_gate`, which this test never notifies, so
    // the only way `dispatch` below can ever complete is via the interrupt.
    fs.hang_local_read.store(true, Ordering::SeqCst);

    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());

    let read_buffer = header_and_body(Opcode::Read as u32, 5, &[0u8; 40]);
    let decoded = decode(&read_buffer).expect("valid request");
    let mut scratch = [0u8; 4096];
    let worker_index = worker.index();

    let dispatch_fut = dispatch(
        &fs,
        &mut worker,
        worker_index,
        decoded.header,
        decoded.request,
        &metrics,
        &slots,
        &deferred,
        &mut scratch,
    );
    tokio::pin!(dispatch_fut);

    // `select!` polls `dispatch_fut` at least once before it can possibly reach the sleep
    // branch below; that first poll runs `dispatch` synchronously up through
    // `slots.begin(..)` and into `fs.read`'s pend on `local_gate`. So by the time the sleep
    // fires, the slot is already registered -- `slots.interrupt` below cannot lose the race
    // against a not-yet-registered slot (`InFlightSlots::interrupt`'s `notify_waiters` only
    // wakes waiters already registered at the moment of the call).
    tokio::select! {
        result = &mut dispatch_fut => {
            result.expect("dispatch must not hit a transport error against MockTransport");
        }
        _ = tokio::time::sleep(Duration::from_millis(20)) => {
            assert!(
                slots.interrupt(5),
                "the read must still be in flight when interrupted"
            );
            dispatch_fut
                .await
                .expect("dispatch must not hit a transport error against MockTransport");
        }
    }

    let replies = transport.replies.lock();
    assert_eq!(
        replies.len(),
        1,
        "the interrupted in-flight read must still reply"
    );
    assert_eq!(reply_errno(&replies[0].1), -Errno::EINTR.0);
}

// End-to-end interrupt tests: a real child process blocked on a real mount, killed with a real
// `SIGINT`, driving the same in-flight-slot cancellation and reply-failure accounting the tests
// above cover with `MockTransport`, but against a real kernel.

/// Lazily unmounts `path` on drop, so a test that panics midway never leaves a mount behind.
struct PathUnmountGuard(PathBuf);

impl Drop for PathUnmountGuard {
    fn drop(&mut self) {
        let _ = unmount_path(&self.0, true);
    }
}

/// Two workers, not one: a real kernel delivers `INTERRUPT` as a separate request on a free
/// worker's fd while the blocked request's own worker is still parked inside `dispatch()`. With
/// a single worker there is no free reader to receive `INTERRUPT` at all -- the worker loop is a
/// strict `receive -> dispatch -> receive` sequence, so it can only see the next request after
/// the current one finishes, and the blocked one only finishes once interrupted. That is a
/// deadlock task 15/17's single-worker helpers never hit, since they never block a request.
fn session_config() -> SessionConfig {
    SessionConfig {
        workers: 2,
        init: InitConfig::default(),
    }
}

/// Polls `check` every 5ms (well under the 20ms sleep ceiling) until it reports done or
/// `timeout` elapses. Never blocks on the mount itself -- `check` must not either.
async fn poll_until(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if check() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Waits (by polling `try_wait`, never a blocking `wait`) for `child` to exit, and returns the
/// signal that terminated it, if any.
async fn wait_for_exit_signal(child: &mut Child, timeout: Duration) -> Option<i32> {
    let mut status = None;
    poll_until(timeout, || {
        if let Ok(Some(exit_status)) = child.try_wait() {
            status = Some(exit_status);
            true
        } else {
            false
        }
    })
    .await;
    status.and_then(|s| s.signal())
}

/// Finds the connection id `fusermount3` assigned to the mount at `path`, by scanning
/// `/proc/self/mountinfo` for the line whose mount point matches and whose filesystem type is
/// `fuse.lore`. The minor device number in that line's `major:minor` field is also the directory
/// name under `/sys/fs/fuse/connections/`.
fn fuse_connection_id(path: &Path) -> Option<u32> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let target = path.to_str()?;
    for line in mountinfo.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let Some(dash) = tokens.iter().position(|&t| t == "-") else {
            continue;
        };
        if tokens.get(4).copied() != Some(target) {
            continue;
        }
        if tokens.get(dash + 1).copied() != Some("fuse.lore") {
            continue;
        }
        return tokens.get(2)?.split(':').nth(1)?.parse().ok();
    }
    None
}

/// Forces the kernel to tear down a FUSE connection via `/sys/fs/fuse/connections/<id>/abort` --
/// the same knob `fusermount -uz` uses to recover a daemon that stopped answering. The kernel
/// completes every `processing` request for its caller immediately with `ECONNABORTED` and marks
/// the connection disconnected, so a reply the daemon writes afterwards for one of those requests
/// is rejected (`ENOENT`) instead of accepted. This is the real trigger for `SendOutcome::Aborted`
/// -- a plain `SIGINT` to the blocked caller is not: see the doc comment on
/// `reply_failed_fires_for_lookup_aborted_by_sigint` for why.
fn force_abort_connection(id: u32) -> std::io::Result<()> {
    std::fs::write(format!("/sys/fs/fuse/connections/{id}/abort"), b"1")
}

/// How long a test waits for a step that takes milliseconds when nothing is wrong. Only a
/// regression reaches it, so it sits far past any scheduling delay on a loaded machine.
const STEP_BOUND: Duration = Duration::from_secs(60);

/// How long a dropped [`Reader`] waits for its killed reader to exit.
const REAP_BOUND: Duration = Duration::from_secs(5);

/// Names the path a reader process works on.
const READER_PATH_VAR: &str = "LORE_FUSE_INTERRUPT_READER_PATH";

/// Names the [`ReaderOperation`] a reader process makes.
const READER_OPERATION_VAR: &str = "LORE_FUSE_INTERRUPT_READER_OPERATION";

/// The test a reader process runs.
const READER_TEST: &str = "interrupt::reader";

/// The request a reader blocks in on its path.
#[derive(Clone, Copy)]
enum ReaderOperation {
    /// An `open` and a `read(2)` of the file. The read sends the READ.
    Read,
    /// A stat of the file, which sends a LOOKUP.
    Lookup,
}

impl ReaderOperation {
    /// The name [`READER_OPERATION_VAR`] carries.
    fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Lookup => "lookup",
        }
    }
}

/// A reader process on a mount, killed and reaped on drop unless it has exited.
///
/// The kernel ends a request the session has taken only when the session answers it, so a reader
/// killed in one can stay in the kernel. Aborting the connection ends the request. Where the
/// connection cannot be aborted, as in a container without the FUSE control filesystem, drop
/// waits [`REAP_BOUND`] and leaves the reader to end with this process, which holds every open
/// copy of the connection's device.
struct Reader {
    child: Child,
    connection: u32,
}

impl Drop for Reader {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        let _ = self.child.kill();
        let aborted = force_abort_connection(self.connection);
        let deadline = std::time::Instant::now() + REAP_BOUND;
        while std::time::Instant::now() < deadline {
            if !matches!(self.child.try_wait(), Ok(None)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        eprintln!(
            "reader {} has not exited {REAP_BOUND:?} after SIGKILL (aborting connection {}: \
             {aborted:?}); it ends once this process exits",
            self.child.id(),
            self.connection
        );
    }
}

/// Starts this test binary again as a reader that makes `operation` on `path`, on the mount whose
/// connection is `connection`. A system tool such as `dd` would make the test depend on how that
/// tool handles `SIGINT`.
fn spawn_reader(operation: ReaderOperation, path: &Path, connection: u32) -> Reader {
    let mut command = Command::new(std::env::current_exe().expect("this test binary"));
    command
        .args(["--exact", READER_TEST, "--ignored", "--nocapture"])
        .env(READER_PATH_VAR, path)
        .env(READER_OPERATION_VAR, operation.name())
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    // SAFETY: `prepare_reader` makes only async-signal-safe calls, as `pre_exec` requires.
    unsafe { command.pre_exec(prepare_reader) };
    Reader {
        child: command.spawn().expect("the reader must spawn"),
        connection,
    }
}

/// Runs in the reader between `fork` and `exec`, and gives `SIGINT` its default action and
/// unblocks it, whatever the reader inherited: a shell without job control starts a background
/// command with `SIGINT` ignored, and a child starts with the signal mask of the thread that
/// spawned it.
fn prepare_reader() -> std::io::Result<()> {
    // SAFETY: `sigemptyset`, `sigaddset`, `signal` and `sigprocmask` are async-signal-safe, and
    // `sigemptyset` initializes `sigint` before `sigaddset` and `sigprocmask` read it.
    unsafe {
        let mut sigint: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut sigint);
        libc::sigaddset(&mut sigint, libc::SIGINT);
        if libc::signal(libc::SIGINT, libc::SIG_DFL) == libc::SIG_ERR
            || libc::sigprocmask(libc::SIG_UNBLOCK, &sigint, std::ptr::null_mut()) != 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The reader process [`spawn_reader`] starts: a stat of the file [`READER_PATH_VAR`] names, or an
/// `open` of it and a `read(2)`. The stat or the read blocks until a signal ends it. Run without
/// that variable, it does nothing.
#[test]
#[ignore = "the reader process spawn_reader starts, not a test of its own"]
fn reader() {
    let Some(path) = std::env::var_os(READER_PATH_VAR) else {
        return;
    };
    let operation = std::env::var(READER_OPERATION_VAR).unwrap_or_default();
    let outcome = if operation == ReaderOperation::Lookup.name() {
        std::fs::metadata(&path).map(|_| 0)
    } else {
        std::fs::File::open(&path).and_then(|mut file| file.read(&mut [0u8; 4096]))
    };
    panic!("the reader's {operation} returned {outcome:?} rather than being ended by a signal");
}

/// Waits until `release` reports that the reader's request has reached the session, and fails at
/// once with the reader's exit status if the reader exits first.
async fn wait_for_request(release: &Notify, reader: &mut Reader) {
    let deadline = tokio::time::Instant::now() + STEP_BOUND;
    let notified = release.notified();
    tokio::pin!(notified);
    loop {
        tokio::select! {
            () = &mut notified => return,
            () = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
        if let Ok(Some(status)) = reader.child.try_wait() {
            panic!("the reader exited with {status} before its request reached the session");
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the reader's request must reach the session within {STEP_BOUND:?}"
        );
    }
}

/// A reader blocked in a `READ` the session holds open is ended by `SIGINT`. The kernel sends
/// `FUSE_INTERRUPT` to a free worker, and the session cancels the handler and answers `EINTR`.
/// Only that answer lets the kernel finish the request and deliver the signal. The bounded
/// shutdown then shows that the worker which served the read came free.
#[tokio::test(flavor = "multi_thread")]
async fn sigint_ends_a_reader_blocked_on_read_and_frees_its_worker() {
    skip_without_fuse!();

    let dir = TempDir::new("lore-fuse-interrupt-read-test-");
    let release = Arc::new(Notify::new());
    let fs = SyntheticFs::from_tree(vec![(
        "blocked".to_string(),
        SynthNode::BlockingRead {
            release: Arc::clone(&release),
        },
    )]);
    let (_mount, session) = mount_synthetic(dir.path(), fs, session_config())
        .await
        .expect("mount must negotiate against a real kernel");
    let guard = PathUnmountGuard(dir.path().to_path_buf());
    let connection = fuse_connection_id(dir.path())
        .expect("the mount's sysfs connection directory must exist once INIT has negotiated");
    let mut reader = spawn_reader(
        ReaderOperation::Read,
        &dir.path().join("blocked"),
        connection,
    );

    // Waits for our own `read` handler to confirm it is holding the request open, rather than
    // sleeping and hoping the reader has reached its `read(2)` by then.
    wait_for_request(&release, &mut reader).await;

    let pid = reader.child.id() as libc::pid_t;
    // SAFETY: `pid` is the reader's own pid, still alive (we have not reaped it yet).
    let ret = unsafe { libc::kill(pid, libc::SIGINT) };
    assert_eq!(ret, 0, "kill(2) must succeed against a live child");

    let signal = wait_for_exit_signal(&mut reader.child, STEP_BOUND).await;
    assert_eq!(
        signal,
        Some(libc::SIGINT),
        "the reader must be terminated by the SIGINT we sent, not exit some other way"
    );

    // Bounded rather than a bare `.await`: `shutdown()` awaits every worker's `JoinHandle`, and a
    // regression that lets a handler stay stuck past its interrupt would otherwise hang the test
    // suite instead of failing it.
    tokio::time::timeout(STEP_BOUND, session.shutdown())
        .await
        .expect("shutdown must complete promptly once the interrupted read is no longer blocked");
    // A worker counts the INTERRUPT once it has woken the request's slot, which can be after the
    // reader has exited, so the count is read once shutdown has joined every worker.
    assert!(
        session.metrics().snapshot(Opcode::Interrupt).count >= 1,
        "the session must have received the kernel's FUSE_INTERRUPT"
    );
    drop(guard);
}

/// DEVIATION (see report): this test is `#[ignore]`d. Plan task 16 assumes a plain `SIGINT` to a
/// caller blocked on a real kernel's LOOKUP is enough to make the kernel reject the daemon's
/// eventual `EINTR` reply with `ENOENT` (`SendOutcome::Aborted`), driving `Filesystem::
/// reply_failed`. Against this machine's kernel (7.0.0-30-generic) that premise does not hold, as
/// three real-mount experiments (kept here, in order of what was tried) show:
///
/// 1. Plain `SIGINT`, no abort: `fs/fuse/dev.c`'s `request_wait_answer` only uses
///    `FUSE_INTERRUPT` as a hint for the daemon to hurry up. Once a request has been read by the
///    daemon (`FR_SENT`), the calling task's own wait becomes uninterruptible (by any signal,
///    including `SIGKILL`) until the daemon actually answers. Debug tracing confirmed a real
///    `FUSE_INTERRUPT` reaches our worker (`slot_hit=true`) and the daemon's resulting `EINTR`
///    reply write returns `Ok(())` (`Sent`), never `Aborted`. The caller only exits once that
///    reply lets its kernel-side wait finish; the already-pending `SIGINT` is then delivered on the
///    way back to userspace -- so the caller dying "by SIGINT" is not evidence the kernel ever gave
///    up on the request.
/// 2. `SIGINT` immediately followed by forcing the connection down via
///    `/sys/fs/fuse/connections/<id>/abort` (the knob `fusermount -uz` uses to recover a hung
///    daemon): `Aborted` was not observed. The daemon's own `guard.interrupted()` only resolves
///    once dispatch has processed a real `FUSE_INTERRUPT` request; aborting earlier can tear the
///    connection down before that request is delivered, leaving the handler parked on
///    `std::future::pending()` forever with no reply attempted.
/// 3. Same, but gated on first observing `session.metrics().snapshot(Opcode::Interrupt).count >=
///    1` (confirming dispatch really did call `slots.interrupt()`) before aborting. `Aborted`
///    needs the abort to land between the session waking on the `FUSE_INTERRUPT` and its `EINTR`
///    reply: a reply written first is accepted as `Sent`. The test cannot force that order, so it
///    fails in most runs.
///
/// The dispatch/interrupt code this test exercises is independently known-correct (see
/// `dispatch::aborted_send_calls_reply_failed_with_entry_outcome`, which drives the same
/// `reply_failed` path with `MockTransport::abort_next_send`). What is missing is a *real*, kernel
/// -level trigger for `Aborted` that an ordinary `SIGINT` scenario reaches every time; this needs
/// either a different reproduction (e.g. racing a real non-lazy `umount` against the in-flight
/// reply, which risks the exact hang this task's binding constraints forbid) or plan revision.
/// Left in place (attempt 3's version, the closest approximation) rather than deleted, so it can
/// be re-enabled if a reliable trigger is found.
#[ignore = "SendOutcome::Aborted needs the abort to land between the INTERRUPT and the EINTR \
            reply, an order the test cannot force; see doc comment"]
#[tokio::test(flavor = "multi_thread")]
async fn reply_failed_fires_for_lookup_aborted_by_sigint() {
    skip_without_fuse!();

    let dir = TempDir::new("lore-fuse-interrupt-lookup-test-");
    let release = Arc::new(Notify::new());
    let fs = SyntheticFs::from_tree(vec![(
        "blocked".to_string(),
        SynthNode::BlockingLookup {
            release: Arc::clone(&release),
        },
    )]);
    let fs_handle = Arc::clone(&fs);
    let (_mount, session) = mount_synthetic(dir.path(), fs, session_config())
        .await
        .expect("mount must negotiate against a real kernel");
    let guard = PathUnmountGuard(dir.path().to_path_buf());
    let conn_id = fuse_connection_id(dir.path())
        .expect("the mount's sysfs connection directory must exist once INIT has negotiated");
    let mut reader = spawn_reader(
        ReaderOperation::Lookup,
        &dir.path().join("blocked"),
        conn_id,
    );

    // Confirms our `lookup` handler is the one blocking (not, say, the reader still resolving
    // earlier path components), before sending the signal.
    wait_for_request(&release, &mut reader).await;

    let pid = reader.child.id() as libc::pid_t;
    // SAFETY: `pid` is the reader's own pid, still alive (we have not reaped it yet).
    let ret = unsafe { libc::kill(pid, libc::SIGINT) };
    assert_eq!(ret, 0, "kill(2) must succeed against a live child");

    // Races the connection abort against our own daemon's `EINTR` reply write: the abort must
    // land after the daemon has woken up from `guard.interrupted()` (else it never attempts a
    // reply at all -- the request just stays parked forever, since nothing else ever resolves
    // it) but before that reply's `write(2)` reaches the kernel (else it is accepted as `Sent`,
    // per the doc comment above). A tight busy-spin (not `poll_until`'s 5ms ticks, which would
    // lose the race outright) on the `Interrupt` opcode's metrics count is the earliest
    // observable signal that `slots.interrupt()` has fired. The session's own workers run on a
    // separate runtime (`lore_spawn!`'s core runtime), so spinning here never delays them.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while session
        .metrics()
        .snapshot(lore_fuse::abi::Opcode::Interrupt)
        .count
        == 0
    {
        assert!(
            std::time::Instant::now() < deadline,
            "a real FUSE_INTERRUPT must be processed within 2s of the signal"
        );
    }
    force_abort_connection(conn_id)
        .expect("writing to the fuse connection's abort file must succeed");

    let signal = wait_for_exit_signal(&mut reader.child, STEP_BOUND).await;
    assert_eq!(
        signal,
        Some(libc::SIGINT),
        "the reader must be terminated by the SIGINT we sent, not exit some other way"
    );

    let fired = poll_until(Duration::from_secs(2), || {
        fs_handle.reply_failed_count.load(Ordering::SeqCst) >= 1
    })
    .await;
    assert!(
        fired,
        "reply_failed must fire once the kernel aborts the LOOKUP reply for a request whose \
         connection was torn down"
    );

    tokio::time::timeout(STEP_BOUND, session.shutdown())
        .await
        .expect("shutdown must complete promptly even after the connection was force-aborted");
    drop(guard);
}
