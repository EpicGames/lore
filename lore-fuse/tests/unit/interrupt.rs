// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::mem::size_of;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
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
fn force_abort_connection(id: u32) {
    std::fs::write(format!("/sys/fs/fuse/connections/{id}/abort"), b"1")
        .expect("writing to the fuse connection's abort file must succeed");
}

#[tokio::test(flavor = "multi_thread")]
async fn sigint_to_dd_blocked_on_read_exits_by_signal_and_next_stat_completes_within_1s() {
    skip_without_fuse!();

    let dir = TempDir::new("lore-fuse-interrupt-read-test-");
    let release = Arc::new(Notify::new());
    let fs = SyntheticFs::from_tree(vec![
        (
            "blocked".to_string(),
            SynthNode::BlockingRead {
                release: Arc::clone(&release),
            },
        ),
        (
            "control".to_string(),
            SynthNode::File {
                content: b"still-alive".to_vec(),
                executable: false,
            },
        ),
    ]);
    let (_mount, session) = mount_synthetic(dir.path(), fs, session_config())
        .await
        .expect("mount must negotiate against a real kernel");
    let guard = PathUnmountGuard(dir.path().to_path_buf());

    let blocked_path = dir.path().join("blocked");
    let mut dd = Command::new("dd")
        .arg(format!("if={}", blocked_path.display()))
        .arg("of=/dev/null")
        .arg("bs=4096")
        .spawn()
        .expect("dd must spawn");

    // Waits for our own `read` handler to confirm it is holding the request open, rather than
    // sleeping and hoping `dd` has reached its `read(2)` by then.
    release.notified().await;

    let pid = dd.id() as libc::pid_t;
    // SAFETY: `pid` is `dd`'s own pid, still alive (we have not reaped it yet).
    let ret = unsafe { libc::kill(pid, libc::SIGINT) };
    assert_eq!(ret, 0, "kill(2) must succeed against a live child");

    let signal = wait_for_exit_signal(&mut dd, Duration::from_secs(5)).await;
    assert_eq!(
        signal,
        Some(libc::SIGINT),
        "dd must be terminated by the SIGINT we sent, not exit some other way"
    );

    // The worker that was serving `dd`'s blocked read must be free to serve new requests: a
    // completely unrelated stat must complete promptly, proving the interrupt did not wedge it.
    let control_path = dir.path().join("control");
    tokio::time::timeout(
        Duration::from_secs(1),
        lore_base::lore_spawn_blocking!(move || std::fs::metadata(&control_path)),
    )
    .await
    .expect("stat of an unrelated file must complete within 1s of the interrupt")
    .expect("spawn_blocking must not panic")
    .expect("stat must succeed");

    // Bounded rather than a bare `.await`: `shutdown()` awaits every worker's `JoinHandle`, and a
    // regression that lets a handler stay stuck past its interrupt would otherwise hang the test
    // suite instead of failing it.
    tokio::time::timeout(Duration::from_secs(5), session.shutdown())
        .await
        .expect("shutdown must complete promptly once the interrupted read is no longer blocked");
    drop(guard);
}

/// DEVIATION (see report): this test is `#[ignore]`d. Plan task 16 assumes a plain `SIGINT` to a
/// caller blocked on a real kernel's LOOKUP is enough to make the kernel reject the daemon's
/// eventual `EINTR` reply with `ENOENT` (`SendOutcome::Aborted`), driving `Filesystem::
/// reply_failed`. Against this machine's kernel (7.0.0-30-generic) that premise does not hold, and
/// four different real-mount experiments (kept here, in order of what was tried) all confirm it:
///
/// 1. Plain `SIGINT`, no abort: `fs/fuse/dev.c`'s `request_wait_answer` only uses
///    `FUSE_INTERRUPT` as a hint for the daemon to hurry up. Once a request has been read by the
///    daemon (`FR_SENT`), the calling task's own wait becomes uninterruptible (by any signal,
///    including `SIGKILL`) until the daemon actually answers. Debug tracing confirmed a real
///    `FUSE_INTERRUPT` reaches our worker (`slot_hit=true`) and the daemon's resulting `EINTR`
///    reply write returns `Ok(())` (`Sent`), never `Aborted`. `stat` only exits once that reply
///    lets its kernel-side wait finish; the already-pending `SIGINT` is then delivered on the way
///    back to userspace -- so `stat` dying "by SIGINT" is not evidence the kernel ever gave up on
///    the request.
/// 2. `SIGINT` immediately followed by forcing the connection down via
///    `/sys/fs/fuse/connections/<id>/abort` (the knob `fusermount -uz` uses to recover a hung
///    daemon): still never observed `Aborted`. Plausible reason: the daemon's own
///    `guard.interrupted()` only resolves once dispatch has actually processed a real
///    `FUSE_INTERRUPT` request; aborting too early can tear the connection down before that
///    request is even delivered, leaving the handler parked on `std::future::pending()` forever
///    with no reply ever attempted.
/// 3. Same, but gated on first observing `session.metrics().snapshot(Opcode::Interrupt).count >=
///    1` (confirming dispatch really did call `slots.interrupt()`) before aborting: still never
///    observed `Aborted`. This suggests `fc->connected = 0` (what the sysfs `abort` knob sets)
///    does not by itself invalidate a reply for a request already sitting in the kernel's
///    `processing` queue -- the reply is still honored regardless of the `connected` flag, and
///    `ENOENT` on the write is reserved for stricter cases (e.g. the request no longer being
///    tracked at all, such as after full connection teardown via `fuse_dev_release`/superblock put
///    following a real, non-lazy unmount, or a genuine double-answer).
///
/// The dispatch/interrupt code this test exercises is independently known-correct (see
/// `dispatch::aborted_send_calls_reply_failed_with_entry_outcome`, which drives the same
/// `reply_failed` path with `MockTransport::abort_next_send`). What is missing is a *real*, kernel
/// -level trigger for `Aborted` reachable from an ordinary `SIGINT` scenario; this needs either a
/// different reproduction (e.g. racing a real non-lazy `umount` against the in-flight reply, which
/// risks the exact hang this task's binding constraints forbid) or plan revision. Left in place
/// (attempt 3's version, the closest approximation) rather than deleted, so it can be re-enabled
/// if a reliable trigger is found.
#[ignore = "SendOutcome::Aborted is not reproducible via SIGINT on this kernel; see doc comment"]
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

    let blocked_path = dir.path().join("blocked");
    let mut stat = Command::new("stat")
        .arg(&blocked_path)
        .spawn()
        .expect("stat must spawn");

    // Confirms our `lookup` handler is the one blocking (not, say, `stat` still resolving
    // earlier path components), before sending the signal.
    release.notified().await;

    let conn_id = fuse_connection_id(dir.path())
        .expect("the mount's sysfs connection directory must exist once INIT has negotiated");

    let pid = stat.id() as libc::pid_t;
    // SAFETY: `pid` is `stat`'s own pid, still alive (we have not reaped it yet).
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
    force_abort_connection(conn_id);

    let signal = wait_for_exit_signal(&mut stat, Duration::from_secs(5)).await;
    assert_eq!(
        signal,
        Some(libc::SIGINT),
        "stat must be terminated by the SIGINT we sent, not exit some other way"
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

    tokio::time::timeout(Duration::from_secs(5), session.shutdown())
        .await
        .expect("shutdown must complete promptly even after the connection was force-aborted");
    drop(guard);
}
