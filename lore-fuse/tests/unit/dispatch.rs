// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::mem::size_of;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_fuse::abi::AttrOut;
use lore_fuse::abi::CreateIn;
use lore_fuse::abi::CreateOut;
use lore_fuse::abi::EntryOut;
use lore_fuse::abi::FallocateIn;
use lore_fuse::abi::FlushIn;
use lore_fuse::abi::FsyncIn;
use lore_fuse::abi::InHeader;
use lore_fuse::abi::LinkIn;
use lore_fuse::abi::MkdirIn;
use lore_fuse::abi::MknodIn;
use lore_fuse::abi::Opcode;
use lore_fuse::abi::Rename2In;
use lore_fuse::abi::RenameIn;
use lore_fuse::abi::SetattrIn;
use lore_fuse::abi::WriteIn;
use lore_fuse::abi::WriteOut;
use lore_fuse::abi::entry::setattr_valid;
use lore_fuse::abi::file::fsync_flags;
use lore_fuse::codec::request::decode;
use lore_fuse::dispatch::*;
use lore_fuse::filesystem::EntryRequest;
use lore_fuse::filesystem::Filesystem;
use lore_fuse::filesystem::default_errno;
use lore_fuse::interrupt::InFlightSlots;
use lore_fuse::metrics::Metrics;
use lore_fuse::reply::AttrReply;
use lore_fuse::reply::EntryReply;
use lore_fuse::reply::Errno;
use lore_fuse::reply::ReadFault;
use lore_fuse::reply::ReplyOutcome;
use lore_fuse::reply::Timeouts;
use lore_fuse::transport::Transport;
use lore_fuse::transport::mock::MockTransport;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

use crate::synthetic::SyntheticFs;
use crate::test_stub::CREATED_FH;
use crate::test_stub::CREATED_INO;
use crate::test_stub::Call;
use crate::test_stub::Fetched;
use crate::test_stub::LINK_TARGET;
use crate::test_stub::StubFilesystem;

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

async fn run<F: Filesystem>(
    fs: &Arc<F>,
    worker: &mut lore_fuse::transport::mock::MockWorker,
    metrics: &Metrics,
    slots: &InFlightSlots,
    deferred: &Arc<DeferredInterrupts>,
    buffer: &[u8],
) {
    let mut scratch = [0u8; 4096];
    run_in(fs, worker, metrics, slots, deferred, buffer, &mut scratch).await;
}

/// [`run`] with `scratch` as the worker's scratch, which a worker keeps across requests.
async fn run_in<F: Filesystem>(
    fs: &Arc<F>,
    worker: &mut lore_fuse::transport::mock::MockWorker,
    metrics: &Metrics,
    slots: &InFlightSlots,
    deferred: &Arc<DeferredInterrupts>,
    buffer: &[u8],
    scratch: &mut [u8],
) {
    let decoded = decode(buffer).expect("valid request");
    let worker_index = worker.index();
    dispatch(
        fs,
        worker,
        worker_index,
        decoded.header,
        decoded.request,
        metrics,
        slots,
        deferred,
        scratch,
    )
    .await
    .expect("dispatch must not hit a transport error against MockTransport");
}

#[tokio::test(flavor = "current_thread")]
async fn unsupported_opcode_gets_default_errno_without_calling_fs() {
    let fs = StubFilesystem::new();
    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());

    let buffer = header_and_body(Opcode::Ioctl as u32, 1, &[]);
    run(&fs, &mut worker, &metrics, &slots, &deferred, &buffer).await;

    assert_eq!(fs.calls.load(Ordering::SeqCst), 0);
    let replies = transport.replies.lock();
    assert_eq!(replies.len(), 1);
    let reply = &replies[0].1;
    let (out_header, _) =
        lore_fuse::abi::OutHeader::ref_from_prefix(reply).expect("reply has a header");
    assert_eq!(out_header.error, -Errno::ENOTTY.0);
}

fn name_body<T: IntoBytes + zerocopy::Immutable>(head: &T, names: &[&[u8]]) -> Vec<u8> {
    let mut body = head.as_bytes().to_vec();
    for name in names {
        body.extend_from_slice(name);
        body.push(0);
    }
    body
}

/// One request per write-family opcode on node 1, with the call its trait method must see.
fn write_family_requests() -> Vec<(Opcode, Vec<u8>, Call)> {
    let setattr = SetattrIn {
        valid: setattr_valid::SIZE | setattr_valid::FH,
        fh: 3,
        size: 1000,
        mode: 0o600,
        ..SetattrIn::default()
    };
    let mut write = WriteIn {
        fh: 4,
        offset: 8192,
        size: 5,
        write_flags: 2,
        ..WriteIn::default()
    }
    .as_bytes()
    .to_vec();
    write.extend_from_slice(b"hello");
    let fsync = FsyncIn {
        fh: 6,
        fsync_flags: fsync_flags::FDATASYNC,
        padding: 0,
    };
    let one = |opcode: Opcode, body: Vec<u8>, call: Call| (opcode, body, call);
    vec![
        one(
            Opcode::Setattr,
            setattr.as_bytes().to_vec(),
            Call::Setattr {
                ino: 1,
                valid: setattr.valid,
                fh: 3,
                size: 1000,
                mode: 0o600,
            },
        ),
        one(
            Opcode::Readlink,
            Vec::new(),
            Call::Readlink {
                ino: 1,
                capacity: libc::PATH_MAX as usize - 1,
            },
        ),
        one(
            Opcode::Symlink,
            name_body(&[0u8; 0], &[b"link", b"../target"]),
            Call::Symlink {
                parent: 1,
                name: b"link".to_vec(),
                target: b"../target".to_vec(),
            },
        ),
        one(
            Opcode::Mknod,
            name_body(
                &MknodIn {
                    mode: libc::S_IFIFO | 0o600,
                    rdev: 9,
                    umask: 0o022,
                    padding: 0,
                },
                &[b"fifo"],
            ),
            Call::Mknod {
                parent: 1,
                name: b"fifo".to_vec(),
                mode: libc::S_IFIFO | 0o600,
                rdev: 9,
                umask: 0o022,
            },
        ),
        one(
            Opcode::Mkdir,
            name_body(
                &MkdirIn {
                    mode: 0o755,
                    umask: 0o022,
                },
                &[b"dir"],
            ),
            Call::Mkdir {
                parent: 1,
                name: b"dir".to_vec(),
                mode: 0o755,
                umask: 0o022,
            },
        ),
        one(
            Opcode::Unlink,
            name_body(&[0u8; 0], &[b"gone"]),
            Call::Unlink {
                parent: 1,
                name: b"gone".to_vec(),
            },
        ),
        one(
            Opcode::Rmdir,
            name_body(&[0u8; 0], &[b"empty"]),
            Call::Rmdir {
                parent: 1,
                name: b"empty".to_vec(),
            },
        ),
        one(
            Opcode::Rename,
            name_body(&RenameIn { newdir: 5 }, &[b"old", b"new"]),
            Call::Rename {
                parent: 1,
                name: b"old".to_vec(),
                newparent: 5,
                newname: b"new".to_vec(),
                flags: 0,
            },
        ),
        one(
            Opcode::Rename2,
            name_body(
                &Rename2In {
                    newdir: 6,
                    flags: libc::RENAME_NOREPLACE,
                    padding: 0,
                },
                &[b"from", b"to"],
            ),
            Call::Rename {
                parent: 1,
                name: b"from".to_vec(),
                newparent: 6,
                newname: b"to".to_vec(),
                flags: libc::RENAME_NOREPLACE,
            },
        ),
        one(
            Opcode::Link,
            name_body(&LinkIn { oldnodeid: 7 }, &[b"hard"]),
            Call::Link {
                ino: 7,
                newparent: 1,
                newname: b"hard".to_vec(),
            },
        ),
        one(
            Opcode::Create,
            name_body(
                &CreateIn {
                    flags: (libc::O_CREAT | libc::O_RDWR) as u32,
                    mode: libc::S_IFREG | 0o644,
                    umask: 0o022,
                    open_flags: 0,
                },
                &[b"new.txt"],
            ),
            Call::Create {
                parent: 1,
                name: b"new.txt".to_vec(),
                mode: libc::S_IFREG | 0o644,
                umask: 0o022,
                flags: (libc::O_CREAT | libc::O_RDWR) as u32,
            },
        ),
        one(
            Opcode::Write,
            write,
            Call::Write {
                ino: 1,
                fh: 4,
                offset: 8192,
                data: b"hello".to_vec(),
                write_flags: 2,
            },
        ),
        one(
            Opcode::Flush,
            FlushIn {
                fh: 5,
                lock_owner: 11,
                ..FlushIn::default()
            }
            .as_bytes()
            .to_vec(),
            Call::Flush {
                ino: 1,
                fh: 5,
                lock_owner: 11,
            },
        ),
        one(
            Opcode::Fsync,
            fsync.as_bytes().to_vec(),
            Call::Fsync {
                ino: 1,
                fh: 6,
                datasync: true,
            },
        ),
        one(
            Opcode::Fsyncdir,
            FsyncIn {
                fh: 8,
                ..FsyncIn::default()
            }
            .as_bytes()
            .to_vec(),
            Call::Fsyncdir {
                ino: 1,
                fh: 8,
                datasync: false,
            },
        ),
        one(
            Opcode::Fallocate,
            FallocateIn {
                fh: 9,
                offset: 10,
                length: 11,
                mode: 1,
                padding: 0,
            }
            .as_bytes()
            .to_vec(),
            Call::Fallocate {
                ino: 1,
                fh: 9,
                offset: 10,
                length: 11,
                mode: 1,
            },
        ),
    ]
}

/// Dispatches one request on a fresh mock transport and returns its reply header and body.
async fn dispatch_one<F: Filesystem>(fs: &Arc<F>, opcode: Opcode, body: &[u8]) -> Vec<u8> {
    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    let metrics = Box::new(Metrics::new());
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());
    let buffer = header_and_body(opcode as u32, 1, body);
    run(fs, &mut worker, &metrics, &slots, &deferred, &buffer).await;
    let replies = transport.replies.lock();
    assert_eq!(replies.len(), 1, "{opcode:?} must get exactly one reply");
    replies[0].1.clone()
}

fn split_reply(reply: &[u8]) -> (lore_fuse::abi::OutHeader, &[u8]) {
    let (header, body) =
        lore_fuse::abi::OutHeader::read_from_prefix(reply).expect("reply has a header");
    assert_eq!(header.len as usize, reply.len());
    (header, body)
}

#[tokio::test(flavor = "current_thread")]
async fn write_family_requests_reach_their_trait_methods_with_decoded_arguments() {
    let fs = StubFilesystem::new();
    let table = write_family_requests();

    for (opcode, body, _) in &table {
        let reply = dispatch_one(&fs, *opcode, body).await;
        let (header, _) = split_reply(&reply);
        assert_eq!(header.error, 0, "{opcode:?} must succeed");
    }

    let expected: Vec<Call> = table.into_iter().map(|(_, _, call)| call).collect();
    assert_eq!(*fs.write_family_calls.lock(), expected);
}

#[tokio::test(flavor = "current_thread")]
async fn create_reply_carries_the_new_entry_and_its_handle() {
    let fs = StubFilesystem::new();
    let (_, body, _) = write_family_requests()
        .into_iter()
        .find(|(opcode, ..)| *opcode == Opcode::Create)
        .expect("create request");

    let reply = dispatch_one(&fs, Opcode::Create, &body).await;
    let (_, rest) = split_reply(&reply);
    let (created, tail) = CreateOut::read_from_prefix(rest).expect("create body");
    assert!(tail.is_empty());
    assert_eq!(created.entry.nodeid, CREATED_INO);
    assert_eq!(created.entry.attr.ino, CREATED_INO);
    assert_eq!(created.open.fh, CREATED_FH);
    assert_eq!(
        created.open.open_flags,
        lore_fuse::abi::file::open_flags::KEEP_CACHE
    );
}

#[tokio::test(flavor = "current_thread")]
async fn write_reply_reports_the_bytes_written() {
    let fs = StubFilesystem::new();
    let (_, body, _) = write_family_requests()
        .into_iter()
        .find(|(opcode, ..)| *opcode == Opcode::Write)
        .expect("write request");

    let reply = dispatch_one(&fs, Opcode::Write, &body).await;
    let (_, rest) = split_reply(&reply);
    let (written, tail) = WriteOut::read_from_prefix(rest).expect("write body");
    assert!(tail.is_empty());
    assert_eq!(written.size, 5);
}

#[tokio::test(flavor = "current_thread")]
async fn setattr_reply_carries_the_new_attributes() {
    let fs = StubFilesystem::new();
    let (_, body, _) = write_family_requests()
        .into_iter()
        .find(|(opcode, ..)| *opcode == Opcode::Setattr)
        .expect("setattr request");

    let reply = dispatch_one(&fs, Opcode::Setattr, &body).await;
    let (_, rest) = split_reply(&reply);
    let (attr, tail) = AttrOut::read_from_prefix(rest).expect("attr body");
    assert!(tail.is_empty());
    assert_eq!(attr.attr_valid, 1);
    assert_eq!(attr.attr.size, 1000);
    assert_eq!(attr.attr.mode, 0o600);
}

#[tokio::test(flavor = "current_thread")]
async fn readlink_reply_is_the_target_without_a_nul() {
    let fs = StubFilesystem::new();

    let reply = dispatch_one(&fs, Opcode::Readlink, &[]).await;
    let (header, rest) = split_reply(&reply);
    assert_eq!(header.error, 0);
    assert_eq!(rest, LINK_TARGET);
}

#[tokio::test(flavor = "current_thread")]
async fn mknod_mkdir_symlink_and_link_reply_with_the_new_entry() {
    let fs = StubFilesystem::new();
    for (opcode, body, _) in write_family_requests() {
        if !matches!(
            opcode,
            Opcode::Mknod | Opcode::Mkdir | Opcode::Symlink | Opcode::Link
        ) {
            continue;
        }
        let reply = dispatch_one(&fs, opcode, &body).await;
        let (_, rest) = split_reply(&reply);
        let (entry, tail) = EntryOut::read_from_prefix(rest).expect("entry body");
        assert!(tail.is_empty(), "{opcode:?}");
        assert_eq!(entry.nodeid, CREATED_INO, "{opcode:?}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn namespace_and_sync_opcodes_reply_with_an_empty_success() {
    let fs = StubFilesystem::new();
    for (opcode, body, _) in write_family_requests() {
        if !matches!(
            opcode,
            Opcode::Unlink
                | Opcode::Rmdir
                | Opcode::Rename
                | Opcode::Rename2
                | Opcode::Flush
                | Opcode::Fsync
                | Opcode::Fsyncdir
                | Opcode::Fallocate
        ) {
            continue;
        }
        let reply = dispatch_one(&fs, opcode, &body).await;
        let (header, rest) = split_reply(&reply);
        assert_eq!(header.error, 0, "{opcode:?}");
        assert!(rest.is_empty(), "{opcode:?}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn write_family_errors_reach_the_kernel_unchanged() {
    let fs = StubFilesystem::new();
    *fs.write_family_error.lock() = Some(Errno::EEXIST);

    for (opcode, body, _) in write_family_requests() {
        let reply = dispatch_one(&fs, opcode, &body).await;
        let (header, rest) = split_reply(&reply);
        assert_eq!(header.error, -Errno::EEXIST.0, "{opcode:?}");
        assert!(rest.is_empty(), "{opcode:?}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn aborted_entry_creating_replies_report_the_new_entry() {
    let fs = StubFilesystem::new();
    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());

    let mut aborted = 0;
    for (opcode, body, _) in write_family_requests() {
        if !matches!(
            opcode,
            Opcode::Create | Opcode::Mknod | Opcode::Mkdir | Opcode::Symlink | Opcode::Link
        ) {
            continue;
        }
        transport.abort_next_send.store(true, Ordering::SeqCst);
        let buffer = header_and_body(opcode as u32, 1, &body);
        run(&fs, &mut worker, &metrics, &slots, &deferred, &buffer).await;
        aborted += 1;
    }

    assert_eq!(
        *fs.reply_failed_calls.lock(),
        vec![ReplyOutcome::Entry { ino: CREATED_INO }; aborted]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn default_bodies_return_the_default_errnos_on_the_synthetic_filesystem() {
    let fs = SyntheticFs::from_tree(Vec::new());
    for (opcode, body, _) in write_family_requests() {
        let expected = match opcode {
            Opcode::Readlink => Errno::EINVAL,
            Opcode::Flush | Opcode::Fsync | Opcode::Fsyncdir => Errno::ENOSYS,
            _ => Errno::EROFS,
        };
        assert_eq!(default_errno(opcode), expected, "{opcode:?}");

        let reply = dispatch_one(&fs, opcode, &body).await;
        let (header, rest) = split_reply(&reply);
        assert_eq!(header.error, -expected.0, "{opcode:?}");
        assert!(rest.is_empty(), "{opcode:?}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn metrics_record_one_sample_per_request() {
    let fs = StubFilesystem::new();
    fs.getattr_results.lock().push_back(Ok(AttrReply {
        attr: Default::default(),
        timeout: Duration::ZERO,
    }));
    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());

    let buffer = header_and_body(Opcode::Getattr as u32, 1, &[0u8; 16]);
    run(&fs, &mut worker, &metrics, &slots, &deferred, &buffer).await;

    assert_eq!(metrics.snapshot(Opcode::Getattr).count, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn forget_gets_no_reply() {
    let fs = StubFilesystem::new();
    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());

    let buffer = header_and_body(Opcode::Forget as u32, 1, &[7u8, 0, 0, 0, 0, 0, 0, 0]);
    run(&fs, &mut worker, &metrics, &slots, &deferred, &buffer).await;

    assert_eq!(transport.replies.lock().len(), 0);
    assert_eq!(fs.forget_calls.lock().as_slice(), &[(1, 7)]);
}

#[tokio::test(flavor = "current_thread")]
async fn aborted_send_calls_reply_failed_with_entry_outcome() {
    let fs = StubFilesystem::new();
    fs.lookup_results.lock().push_back(Ok(EntryReply {
        ino: 42,
        generation: 0,
        attr: Default::default(),
        timeouts: Timeouts::ZERO,
    }));
    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    transport.abort_next_send.store(true, Ordering::SeqCst);
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());

    let buffer = header_and_body(Opcode::Lookup as u32, 1, b"foo\0");
    run(&fs, &mut worker, &metrics, &slots, &deferred, &buffer).await;

    assert_eq!(transport.replies.lock().len(), 0);
    assert_eq!(
        fs.reply_failed_calls.lock().as_slice(),
        &[ReplyOutcome::Entry { ino: 42 }]
    );
}

/// An aborted READDIRPLUS reply gives back one lookup for each record the kernel would have
/// taken a reference for: every record whose node id is not 0, `.` and `..` aside.
#[tokio::test(flavor = "current_thread")]
async fn an_aborted_readdirplus_reply_reports_each_record_with_a_node_id() {
    let fs = StubFilesystem::new();
    *fs.readdirplus_nodeids.lock() = vec![0, 5, 0, 6];
    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    transport.abort_next_send.store(true, Ordering::SeqCst);
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());

    let buffer = header_and_body(Opcode::Readdirplus as u32, 1, &readdir_body(7, 4096));
    run(&fs, &mut worker, &metrics, &slots, &deferred, &buffer).await;

    assert_eq!(transport.replies.lock().len(), 0);
    assert_eq!(
        fs.reply_failed_calls.lock().as_slice(),
        &[
            ReplyOutcome::Entry { ino: 5 },
            ReplyOutcome::Entry { ino: 6 }
        ]
    );
}

/// An aborted reply reports only the records it holds: not one the page had no room for, nor
/// one an earlier reply left in the worker's scratch past this reply's end.
#[tokio::test(flavor = "current_thread")]
async fn an_aborted_readdirplus_reply_reports_only_the_records_it_holds() {
    let fs = StubFilesystem::new();
    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());
    let mut scratch = [0u8; 4096];
    let record = lore_fuse::abi::dir::direntplus_size(crate::test_stub::STUB_DIRENT.len()) as u32;

    *fs.readdirplus_nodeids.lock() = vec![5, 6, 7];
    let sent = header_and_body(Opcode::Readdirplus as u32, 1, &readdir_body(7, 4096));
    run_in(
        &fs,
        &mut worker,
        &metrics,
        &slots,
        &deferred,
        &sent,
        &mut scratch,
    )
    .await;
    assert!(reply_for(&transport, 1).is_some(), "the first page is sent");

    // Room for two records and most of a third, which the stub's third does not fit.
    *fs.readdirplus_nodeids.lock() = vec![8, 9, 10];
    transport.abort_next_send.store(true, Ordering::SeqCst);
    let size = 3 * record - 8;
    let aborted = header_and_body(Opcode::Readdirplus as u32, 2, &readdir_body(7, size));
    run_in(
        &fs,
        &mut worker,
        &metrics,
        &slots,
        &deferred,
        &aborted,
        &mut scratch,
    )
    .await;

    assert_eq!(
        fs.reply_failed_calls.lock().as_slice(),
        &[
            ReplyOutcome::Entry { ino: 8 },
            ReplyOutcome::Entry { ino: 9 }
        ]
    );
}

/// A READDIRPLUS interrupted once it has written records gives back one lookup for each
/// record with a node id: the kernel takes none on an `EINTR` reply.
#[tokio::test(flavor = "current_thread")]
async fn an_interrupted_readdirplus_gives_back_each_record_with_a_node_id() {
    let fs = StubFilesystem::new();
    *fs.readdirplus_nodeids.lock() = vec![0, 5, 0, 6];
    fs.park_readdirplus.store(true, Ordering::SeqCst);
    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());

    let plus = header_and_body(Opcode::Readdirplus as u32, 1, &readdir_body(7, 4096));
    let interrupt = async {
        await_condition(|| fs.readdirplus_parked.load(Ordering::SeqCst)).await;
        slots.interrupt(1)
    };
    let ((), interrupted) = tokio::join!(
        run(&fs, &mut worker, &metrics, &slots, &deferred, &plus),
        interrupt
    );

    assert!(
        interrupted,
        "the INTERRUPT found the page parked after its records"
    );
    let (_, reply) = reply_for(&transport, 1).expect("the page replies");
    assert_eq!(split_reply(&reply).0.error, -Errno::EINTR.0);
    assert_eq!(
        fs.reply_failed_calls.lock().as_slice(),
        &[
            ReplyOutcome::Entry { ino: 5 },
            ReplyOutcome::Entry { ino: 6 }
        ]
    );
}

/// A READDIRPLUS answered with an error once it has written records gives back one lookup
/// for each record with a node id: the kernel takes none on an error reply.
#[tokio::test(flavor = "current_thread")]
async fn a_readdirplus_answering_an_error_gives_back_each_record_with_a_node_id() {
    let fs = StubFilesystem::new();
    *fs.readdirplus_nodeids.lock() = vec![0, 5, 0, 6];
    *fs.readdirplus_error.lock() = Some(Errno::EIO);
    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());

    let plus = header_and_body(Opcode::Readdirplus as u32, 1, &readdir_body(7, 4096));
    run(&fs, &mut worker, &metrics, &slots, &deferred, &plus).await;

    let (_, reply) = reply_for(&transport, 1).expect("the page replies");
    assert_eq!(split_reply(&reply).0.error, -Errno::EIO.0);
    assert_eq!(
        fs.reply_failed_calls.lock().as_slice(),
        &[
            ReplyOutcome::Entry { ino: 5 },
            ReplyOutcome::Entry { ino: 6 }
        ]
    );
}

#[tokio::test]
async fn readdir_and_readdirplus_receive_the_opendir_handle() {
    let fs = StubFilesystem::new();
    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());

    let mut body = [0u8; 40];
    body[..8].copy_from_slice(&42u64.to_le_bytes());
    let plus = header_and_body(Opcode::Readdirplus as u32, 1, &body);
    run(&fs, &mut worker, &metrics, &slots, &deferred, &plus).await;
    body[..8].copy_from_slice(&43u64.to_le_bytes());
    let plain = header_and_body(Opcode::Readdir as u32, 2, &body);
    run(&fs, &mut worker, &metrics, &slots, &deferred, &plain).await;

    assert_eq!(fs.readdir_handles.lock().as_slice(), &[42, 43]);
}

fn entry(ino: u64) -> EntryReply {
    EntryReply {
        ino,
        generation: 0,
        attr: Default::default(),
        timeouts: Timeouts::ZERO,
    }
}

/// Dispatches `buffer`, failing the test if the worker is still busy with it after two
/// seconds.
async fn run_promptly<F: Filesystem>(
    fs: &Arc<F>,
    worker: &mut lore_fuse::transport::mock::MockWorker,
    metrics: &Metrics,
    slots: &InFlightSlots,
    deferred: &Arc<DeferredInterrupts>,
    buffer: &[u8],
) {
    let dispatched = tokio::time::timeout(
        Duration::from_secs(2),
        run(fs, worker, metrics, slots, deferred, buffer),
    )
    .await;
    assert!(
        dispatched.is_ok(),
        "the worker must be free while the request waits on a fetch"
    );
}

/// Polls until `condition` holds, for up to two seconds.
async fn await_condition(mut condition: impl FnMut() -> bool) {
    for _ in 0..200 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The reply the transport recorded for `unique`, with the index of the worker it went to.
fn reply_for(transport: &MockTransport, unique: u64) -> Option<(usize, Vec<u8>)> {
    transport
        .replies
        .lock()
        .iter()
        .find(|(_, bytes)| split_reply(bytes).0.unique == unique)
        .cloned()
}

/// A READDIR or READDIRPLUS body for stream `fh` at offset 0, asking for `size` bytes.
fn readdir_body(fh: u64, size: u32) -> [u8; 40] {
    let mut body = [0u8; 40];
    body[..8].copy_from_slice(&fh.to_le_bytes());
    body[16..20].copy_from_slice(&size.to_le_bytes());
    body
}

#[tokio::test]
async fn would_fetch_lookup_is_deferred_and_worker_serves_next_request_before_it_completes() {
    let fs = StubFilesystem::new();
    fs.defer_listing.store(true, Ordering::SeqCst);
    fs.hang_listing.store(true, Ordering::SeqCst);
    fs.lookup_results.lock().push_back(Ok(entry(42)));
    fs.getattr_results.lock().push_back(Ok(AttrReply {
        attr: Default::default(),
        timeout: Duration::ZERO,
    }));
    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());

    let lookup = header_and_body(Opcode::Lookup as u32, 1, b"cold\0");
    run_promptly(&fs, &mut worker, &metrics, &slots, &deferred, &lookup).await;
    assert_eq!(transport.replies.lock().len(), 0);

    let getattr = header_and_body(Opcode::Getattr as u32, 2, &[0u8; 16]);
    run_promptly(&fs, &mut worker, &metrics, &slots, &deferred, &getattr).await;
    assert!(reply_for(&transport, 2).is_some());

    fs.listing_gate.notify_one();
    await_condition(|| reply_for(&transport, 1).is_some()).await;
    let (_, reply) = reply_for(&transport, 1).expect("the deferred LOOKUP replies");
    let (header, body) = split_reply(&reply);
    assert_eq!(header.error, 0);
    let (entry, _) = EntryOut::read_from_prefix(body).expect("entry body");
    assert_eq!(entry.nodeid, 42);
}

#[tokio::test]
async fn deferred_lookup_reply_goes_to_the_originating_worker_fd() {
    let fs = StubFilesystem::new();
    fs.defer_listing.store(true, Ordering::SeqCst);
    fs.hang_listing.store(true, Ordering::SeqCst);
    fs.lookup_results.lock().push_back(Ok(entry(42)));
    let transport = MockTransport::new();
    let mut worker_a = transport.worker().expect("worker a");
    let worker_b = transport.worker().expect("worker b");
    assert_ne!(worker_a.index(), worker_b.index());
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(2);
    let deferred = Arc::new(DeferredInterrupts::new());

    let lookup = header_and_body(Opcode::Lookup as u32, 1, b"cold\0");
    run_promptly(&fs, &mut worker_a, &metrics, &slots, &deferred, &lookup).await;
    fs.listing_gate.notify_one();

    await_condition(|| reply_for(&transport, 1).is_some()).await;
    let (index, _) = reply_for(&transport, 1).expect("the deferred LOOKUP replies");
    assert_eq!(index, worker_a.index());
}

#[tokio::test]
async fn aborted_deferred_lookup_reply_reports_the_entry() {
    let fs = StubFilesystem::new();
    fs.defer_listing.store(true, Ordering::SeqCst);
    fs.hang_listing.store(true, Ordering::SeqCst);
    fs.lookup_results.lock().push_back(Ok(entry(42)));
    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());

    let lookup = header_and_body(Opcode::Lookup as u32, 1, b"cold\0");
    run_promptly(&fs, &mut worker, &metrics, &slots, &deferred, &lookup).await;
    transport.abort_next_send.store(true, Ordering::SeqCst);
    fs.listing_gate.notify_one();

    await_condition(|| !fs.reply_failed_calls.lock().is_empty()).await;
    assert_eq!(
        fs.reply_failed_calls.lock().as_slice(),
        &[ReplyOutcome::Entry { ino: 42 }]
    );
}

#[tokio::test]
async fn deferred_readdir_replies_with_its_entries_within_the_requested_size() {
    let fs = StubFilesystem::new();
    fs.defer_listing.store(true, Ordering::SeqCst);
    fs.hang_listing.store(true, Ordering::SeqCst);
    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());
    let size = 64;

    let readdir = header_and_body(Opcode::Readdir as u32, 1, &readdir_body(5, size));
    run_promptly(&fs, &mut worker, &metrics, &slots, &deferred, &readdir).await;
    fs.listing_gate.notify_one();

    await_condition(|| reply_for(&transport, 1).is_some()).await;
    let (_, reply) = reply_for(&transport, 1).expect("the deferred READDIR replies");
    let (header, body) = split_reply(&reply);
    assert_eq!(header.error, 0);
    assert!(body.len() <= size as usize, "{} bytes", body.len());
    assert!(
        body.windows(crate::test_stub::STUB_DIRENT.len())
            .any(|name| name == crate::test_stub::STUB_DIRENT)
    );
    assert_eq!(fs.readdir_handles.lock().as_slice(), &[5]);
}

#[tokio::test]
async fn an_aborted_deferred_readdirplus_reply_reports_each_record_with_a_node_id() {
    let fs = StubFilesystem::new();
    fs.defer_listing.store(true, Ordering::SeqCst);
    fs.hang_listing.store(true, Ordering::SeqCst);
    *fs.readdirplus_nodeids.lock() = vec![0, 0, 3, 4];
    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());

    let plus = header_and_body(Opcode::Readdirplus as u32, 1, &readdir_body(7, 4096));
    run_promptly(&fs, &mut worker, &metrics, &slots, &deferred, &plus).await;
    transport.abort_next_send.store(true, Ordering::SeqCst);
    fs.listing_gate.notify_one();

    // One synchronous loop makes the reports, so once the first is seen all are.
    await_condition(|| !fs.reply_failed_calls.lock().is_empty()).await;
    assert_eq!(
        fs.reply_failed_calls.lock().as_slice(),
        &[
            ReplyOutcome::Entry { ino: 3 },
            ReplyOutcome::Entry { ino: 4 }
        ]
    );
}

#[tokio::test]
async fn a_deferred_readdirplus_answering_an_error_gives_back_each_record_with_a_node_id() {
    let fs = StubFilesystem::new();
    fs.defer_listing.store(true, Ordering::SeqCst);
    *fs.readdirplus_nodeids.lock() = vec![0, 0, 3, 4];
    *fs.readdirplus_error.lock() = Some(Errno::EIO);
    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());

    let plus = header_and_body(Opcode::Readdirplus as u32, 1, &readdir_body(7, 4096));
    run_promptly(&fs, &mut worker, &metrics, &slots, &deferred, &plus).await;

    await_condition(|| reply_for(&transport, 1).is_some()).await;
    let (_, reply) = reply_for(&transport, 1).expect("the deferred page replies");
    assert_eq!(split_reply(&reply).0.error, -Errno::EIO.0);
    assert_eq!(
        fs.reply_failed_calls.lock().as_slice(),
        &[
            ReplyOutcome::Entry { ino: 3 },
            ReplyOutcome::Entry { ino: 4 }
        ]
    );
}

#[tokio::test]
async fn an_interrupted_deferred_readdirplus_gives_back_each_record_with_a_node_id() {
    let fs = StubFilesystem::new();
    fs.defer_listing.store(true, Ordering::SeqCst);
    fs.park_readdirplus.store(true, Ordering::SeqCst);
    *fs.readdirplus_nodeids.lock() = vec![0, 0, 3, 4];
    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());

    let plus = header_and_body(Opcode::Readdirplus as u32, 1, &readdir_body(7, 4096));
    run_promptly(&fs, &mut worker, &metrics, &slots, &deferred, &plus).await;
    await_condition(|| fs.readdirplus_parked.load(Ordering::SeqCst)).await;
    run_promptly(
        &fs,
        &mut worker,
        &metrics,
        &slots,
        &deferred,
        &interrupt_of(2, 1),
    )
    .await;

    await_condition(|| reply_for(&transport, 1).is_some()).await;
    let (_, reply) = reply_for(&transport, 1).expect("the interrupted page replies");
    assert_eq!(split_reply(&reply).0.error, -Errno::EINTR.0);
    assert_eq!(
        fs.reply_failed_calls.lock().as_slice(),
        &[
            ReplyOutcome::Entry { ino: 3 },
            ReplyOutcome::Entry { ino: 4 }
        ]
    );
}

#[tokio::test]
async fn would_fetch_read_is_deferred_and_worker_serves_next_request_before_it_completes() {
    let fs = StubFilesystem::new();
    fs.read_results.lock().push_back(Err(ReadFault::WouldFetch));
    fs.hang_fetch_read.store(true, Ordering::SeqCst);
    fs.getattr_results.lock().push_back(Ok(AttrReply {
        attr: Default::default(),
        timeout: Duration::ZERO,
    }));

    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());

    let read_buffer = header_and_body(Opcode::Read as u32, 1, &[0u8; 40]);
    run(&fs, &mut worker, &metrics, &slots, &deferred, &read_buffer).await;

    // The deferred read is still hanging on `fetch_gate`: no reply for it yet, but the
    // worker itself is free to answer a later request.
    assert_eq!(transport.replies.lock().len(), 0);

    let getattr_buffer = header_and_body(Opcode::Getattr as u32, 2, &[0u8; 16]);
    run(
        &fs,
        &mut worker,
        &metrics,
        &slots,
        &deferred,
        &getattr_buffer,
    )
    .await;
    assert_eq!(transport.replies.lock().len(), 1);

    fs.read_results.lock().push_back(Ok(5));
    fs.hang_fetch_read.store(false, Ordering::SeqCst);
    fs.fetch_gate.notify_one();

    for _ in 0..50 {
        if transport.replies.lock().len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let replies = transport.replies.lock();
    assert_eq!(replies.len(), 2, "deferred read must eventually reply");
}

#[tokio::test]
async fn deferred_reply_goes_to_the_originating_worker_fd() {
    let fs = StubFilesystem::new();
    fs.read_results.lock().push_back(Err(ReadFault::WouldFetch));
    fs.read_results.lock().push_back(Ok(3));

    let transport = MockTransport::new();
    let mut worker_a = transport.worker().expect("worker a");
    let mut worker_b = transport.worker().expect("worker b");
    assert_ne!(worker_a.index(), worker_b.index());

    let metrics = Metrics::new();
    let slots = InFlightSlots::new(2);
    let deferred = Arc::new(DeferredInterrupts::new());

    let read_buffer = header_and_body(Opcode::Read as u32, 1, &[0u8; 40]);
    run(
        &fs,
        &mut worker_a,
        &metrics,
        &slots,
        &deferred,
        &read_buffer,
    )
    .await;

    // A request on worker b, so the reply log has an entry from each worker index once the
    // deferred read (worker a) lands.
    let getattr_buffer = header_and_body(Opcode::Getattr as u32, 2, &[0u8; 16]);
    fs.getattr_results.lock().push_back(Ok(AttrReply {
        attr: Default::default(),
        timeout: Duration::ZERO,
    }));
    run(
        &fs,
        &mut worker_b,
        &metrics,
        &slots,
        &deferred,
        &getattr_buffer,
    )
    .await;

    for _ in 0..50 {
        if transport.replies.lock().len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let replies = transport.replies.lock();
    assert_eq!(replies.len(), 2);
    assert!(
        replies.iter().any(|(index, _)| *index == worker_a.index()),
        "the deferred read's reply must be tagged with its originating worker's index"
    );
}

/// The body of the CREATE in [`write_family_requests`].
fn create_body() -> Vec<u8> {
    write_family_requests()
        .into_iter()
        .find(|(opcode, ..)| *opcode == Opcode::Create)
        .map(|(_, body, _)| body)
        .expect("create request")
}

fn interrupt_of(unique: u64, target: u64) -> Vec<u8> {
    header_and_body(Opcode::Interrupt as u32, unique, &target.to_le_bytes())
}

/// A deferred CREATE whose fetch an INTERRUPT overtakes answers `EINTR` and never runs its
/// completion, so nothing is created.
#[tokio::test]
async fn an_interrupt_during_a_deferred_fetch_gets_eintr() {
    let fs = StubFilesystem::new();
    fs.defer_writes.store(true, Ordering::SeqCst);
    fs.hang_fetch_phase.store(true, Ordering::SeqCst);
    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());

    let create = header_and_body(Opcode::Create as u32, 1, &create_body());
    run_promptly(&fs, &mut worker, &metrics, &slots, &deferred, &create).await;
    assert!(
        reply_for(&transport, 1).is_none(),
        "the CREATE waits on its fetch off the worker"
    );
    await_condition(|| !fs.fetches.lock().is_empty()).await;
    assert_eq!(
        *fs.fetches.lock(),
        vec![Fetched::Entry {
            parent: 1,
            name: b"new.txt".to_vec(),
            request: EntryRequest::Create,
        }]
    );

    run_promptly(
        &fs,
        &mut worker,
        &metrics,
        &slots,
        &deferred,
        &interrupt_of(2, 1),
    )
    .await;
    await_condition(|| reply_for(&transport, 1).is_some()).await;
    let (_, reply) = reply_for(&transport, 1).expect("the interrupted CREATE replies");
    let (header, body) = split_reply(&reply);
    assert_eq!(header.error, -Errno::EINTR.0);
    assert!(body.is_empty());
    assert!(
        reply_for(&transport, 2).is_none(),
        "an INTERRUPT that finds its target gets no reply"
    );
    assert_eq!(fs.completions_entered.load(Ordering::SeqCst), 0);
    assert!(
        fs.write_family_calls.lock().is_empty(),
        "nothing was created"
    );
}

/// An INTERRUPT arriving once a deferred CREATE's fetch is done and its completion runs gets
/// the completion's result, never `EINTR`, so no change is half made.
#[tokio::test]
async fn an_interrupt_during_a_deferred_completion_gets_its_result() {
    let fs = StubFilesystem::new();
    fs.defer_writes.store(true, Ordering::SeqCst);
    fs.hang_completion.store(true, Ordering::SeqCst);
    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());

    let create = header_and_body(Opcode::Create as u32, 1, &create_body());
    run_promptly(&fs, &mut worker, &metrics, &slots, &deferred, &create).await;
    await_condition(|| fs.completions_entered.load(Ordering::SeqCst) == 1).await;
    assert_eq!(fs.completions_entered.load(Ordering::SeqCst), 1);
    assert_eq!(fs.fetches.lock().len(), 1, "the fetch ran first");
    assert!(reply_for(&transport, 1).is_none());

    run_promptly(
        &fs,
        &mut worker,
        &metrics,
        &slots,
        &deferred,
        &interrupt_of(2, 1),
    )
    .await;
    fs.completion_gate.notify_one();
    await_condition(|| reply_for(&transport, 1).is_some()).await;
    let (_, reply) = reply_for(&transport, 1).expect("the completed CREATE replies");
    let (header, body) = split_reply(&reply);
    assert_eq!(header.error, 0, "the completion's result, not EINTR");
    let (created, _) = CreateOut::read_from_prefix(body).expect("create body");
    assert_eq!(created.entry.nodeid, CREATED_INO);
    assert_eq!(created.open.fh, CREATED_FH);
    assert_eq!(fs.write_family_calls.lock().len(), 1);
}

/// A SETATTR of a size whose copy-up waits on its fetch leaves the worker free; it replies
/// with the new attributes once the fetch is done.
#[tokio::test]
async fn a_setattr_whose_copy_up_is_held_leaves_the_worker_free() {
    let fs = StubFilesystem::new();
    fs.defer_writes.store(true, Ordering::SeqCst);
    fs.hang_fetch_phase.store(true, Ordering::SeqCst);
    fs.getattr_results.lock().push_back(Ok(AttrReply {
        attr: Default::default(),
        timeout: Duration::ZERO,
    }));
    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());

    let setattr = SetattrIn {
        valid: setattr_valid::SIZE,
        size: 1000,
        ..SetattrIn::default()
    };
    let request = header_and_body(Opcode::Setattr as u32, 1, setattr.as_bytes());
    run_promptly(&fs, &mut worker, &metrics, &slots, &deferred, &request).await;
    assert!(
        reply_for(&transport, 1).is_none(),
        "the SETATTR waits on its copy-up off the worker"
    );
    let getattr = header_and_body(Opcode::Getattr as u32, 2, &[0u8; 16]);
    run_promptly(&fs, &mut worker, &metrics, &slots, &deferred, &getattr).await;
    assert!(
        reply_for(&transport, 2).is_some(),
        "the worker answers meanwhile"
    );
    await_condition(|| !fs.fetches.lock().is_empty()).await;
    assert_eq!(
        *fs.fetches.lock(),
        vec![Fetched::Content {
            ino: 1,
            len: Some(1000),
        }]
    );

    fs.fetch_phase_gate.notify_one();
    await_condition(|| reply_for(&transport, 1).is_some()).await;
    let (_, reply) = reply_for(&transport, 1).expect("the SETATTR replies after its fetch");
    let (header, body) = split_reply(&reply);
    assert_eq!(header.error, 0);
    let (attr, _) = AttrOut::read_from_prefix(body).expect("attr body");
    assert_eq!(attr.attr.size, 1000);
}

/// SYMLINK, MKNOD, MKDIR, UNLINK, RMDIR, RENAME and RENAME2 whose local form answers
/// `WouldFetch` each wait on their fetch off the worker, which answers another request
/// meanwhile; once the fetch is done, the full form runs once with the decoded arguments and
/// the request gets its own reply: the new entry, or an empty success.
#[tokio::test]
async fn deferred_namespace_requests_fetch_then_complete_with_their_own_reply() {
    let deferred_ops = [
        Opcode::Symlink,
        Opcode::Mknod,
        Opcode::Mkdir,
        Opcode::Unlink,
        Opcode::Rmdir,
        Opcode::Rename,
        Opcode::Rename2,
    ];
    let table: Vec<_> = write_family_requests()
        .into_iter()
        .filter(|(opcode, ..)| deferred_ops.contains(opcode))
        .collect();
    assert_eq!(table.len(), deferred_ops.len(), "one request per opcode");
    for (opcode, body, call) in table {
        let fs = StubFilesystem::new();
        fs.defer_writes.store(true, Ordering::SeqCst);
        fs.hang_fetch_phase.store(true, Ordering::SeqCst);
        fs.getattr_results.lock().push_back(Ok(AttrReply {
            attr: Default::default(),
            timeout: Duration::ZERO,
        }));
        let transport = MockTransport::new();
        let mut worker = transport.worker().expect("worker");
        let metrics = Metrics::new();
        let slots = InFlightSlots::new(1);
        let deferred = Arc::new(DeferredInterrupts::new());

        let request = header_and_body(opcode as u32, 1, &body);
        run_promptly(&fs, &mut worker, &metrics, &slots, &deferred, &request).await;
        assert!(
            reply_for(&transport, 1).is_none(),
            "{opcode:?} waits on its fetch off the worker"
        );
        let getattr = header_and_body(Opcode::Getattr as u32, 2, &[0u8; 16]);
        run_promptly(&fs, &mut worker, &metrics, &slots, &deferred, &getattr).await;
        assert!(
            reply_for(&transport, 2).is_some(),
            "{opcode:?}: the worker answers meanwhile"
        );
        await_condition(|| !fs.fetches.lock().is_empty()).await;
        let fetched = fs.fetches.lock().clone();
        let expected_fetch = match &call {
            Call::Symlink { parent, name, .. } => Fetched::Entry {
                parent: *parent,
                name: name.clone(),
                request: EntryRequest::Symlink,
            },
            Call::Mknod { parent, name, .. } => Fetched::Entry {
                parent: *parent,
                name: name.clone(),
                request: EntryRequest::Mknod,
            },
            Call::Mkdir { parent, name, .. } => Fetched::Entry {
                parent: *parent,
                name: name.clone(),
                request: EntryRequest::Mkdir,
            },
            Call::Unlink { parent, name } => Fetched::Entry {
                parent: *parent,
                name: name.clone(),
                request: EntryRequest::Unlink,
            },
            Call::Rmdir { parent, name } => Fetched::Entry {
                parent: *parent,
                name: name.clone(),
                request: EntryRequest::Rmdir,
            },
            Call::Rename {
                parent,
                name,
                newparent,
                newname,
                flags,
            } => Fetched::Rename {
                parent: *parent,
                name: name.clone(),
                newparent: *newparent,
                newname: newname.clone(),
                flags: *flags,
            },
            other => panic!("{opcode:?}: no deferred form for {other:?}"),
        };
        assert_eq!(fetched, vec![expected_fetch], "{opcode:?}: the fetch");
        assert_eq!(
            fs.completions_entered.load(Ordering::SeqCst),
            0,
            "{opcode:?}: nothing completes before its fetch"
        );

        fs.fetch_phase_gate.notify_one();
        await_condition(|| reply_for(&transport, 1).is_some()).await;
        let (_, reply) = reply_for(&transport, 1).expect("the request replies");
        let (header, rest) = split_reply(&reply);
        assert_eq!(header.error, 0, "{opcode:?}");
        if matches!(opcode, Opcode::Symlink | Opcode::Mknod | Opcode::Mkdir) {
            let (entry, tail) = EntryOut::read_from_prefix(rest).expect("entry body");
            assert!(tail.is_empty(), "{opcode:?}");
            assert_eq!(entry.nodeid, CREATED_INO, "{opcode:?}");
        } else {
            assert!(rest.is_empty(), "{opcode:?}: an empty success");
        }
        assert_eq!(
            fs.completions_entered.load(Ordering::SeqCst),
            1,
            "{opcode:?}"
        );
        assert_eq!(*fs.write_family_calls.lock(), vec![call], "{opcode:?}");
    }
}

/// FSYNC's flush runs off the worker, which answers another request meanwhile; the FSYNC
/// replies once the flush is done, with no second flush.
#[tokio::test]
async fn fsync_flushes_off_the_worker() {
    let fs = StubFilesystem::new();
    fs.defer_writes.store(true, Ordering::SeqCst);
    fs.hang_fetch_phase.store(true, Ordering::SeqCst);
    fs.getattr_results.lock().push_back(Ok(AttrReply {
        attr: Default::default(),
        timeout: Duration::ZERO,
    }));
    let transport = MockTransport::new();
    let mut worker = transport.worker().expect("worker");
    let metrics = Metrics::new();
    let slots = InFlightSlots::new(1);
    let deferred = Arc::new(DeferredInterrupts::new());

    let fsync = FsyncIn {
        fh: 6,
        fsync_flags: fsync_flags::FDATASYNC,
        padding: 0,
    };
    let request = header_and_body(Opcode::Fsync as u32, 1, fsync.as_bytes());
    run_promptly(&fs, &mut worker, &metrics, &slots, &deferred, &request).await;
    assert!(
        reply_for(&transport, 1).is_none(),
        "the FSYNC waits on its flush off the worker"
    );
    let getattr = header_and_body(Opcode::Getattr as u32, 2, &[0u8; 16]);
    run_promptly(&fs, &mut worker, &metrics, &slots, &deferred, &getattr).await;
    assert!(
        reply_for(&transport, 2).is_some(),
        "the worker answers meanwhile"
    );
    await_condition(|| !fs.fetches.lock().is_empty()).await;
    assert_eq!(
        *fs.fetches.lock(),
        vec![Fetched::Flush {
            ino: 1,
            datasync: true,
        }]
    );

    fs.fetch_phase_gate.notify_one();
    await_condition(|| reply_for(&transport, 1).is_some()).await;
    let (_, reply) = reply_for(&transport, 1).expect("the FSYNC replies after its flush");
    let (header, body) = split_reply(&reply);
    assert_eq!(header.error, 0);
    assert!(body.is_empty());
    assert!(
        fs.write_family_calls.lock().is_empty(),
        "the flush is the fetch; nothing flushes again"
    );
}
