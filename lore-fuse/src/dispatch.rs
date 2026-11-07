// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Turns one decoded request into exactly one metrics sample and (with the sole exception of
//! `FORGET`/`BATCH_FORGET`, and an `INTERRUPT` that finds its target) exactly one reply (spec:
//! "Request loop", "Reply accounting").
//!
//! `LOOKUP`, `GETATTR`, `READDIR`, `READDIRPLUS`, and the local half of `READ` are cancellable:
//! each claims the calling worker's [`InFlightSlots`] slot for its `unique` and races the
//! `Filesystem` call against [`SlotGuard::interrupted`], replying `EINTR` if `INTERRUPT` wins
//! (spec: "Interrupts"). `READ`, `LOOKUP`, `READDIR` and `READDIRPLUS` first run without a remote
//! fetch; one that answers `ReadFault::WouldFetch` is answered later instead: the dispatcher
//! registers the request's unique in [`DeferredInterrupts`] and `lore_spawn!`s a task that owns
//! what the answer needs (decision 3), runs the fetching form of the request, and replies
//! through a [`crate::transport::ReplySender`] bound to this same worker.
//!
//! CREATE, MKNOD, MKDIR, SYMLINK, UNLINK, RMDIR, RENAME, WRITE, SETATTR, FALLOCATE and FSYNC run
//! their local forms on the worker, not raced, since a change is never left half made. One that
//! answers `WouldFetch` is deferred the same way, in two phases: [`Filesystem::fetch`] raced
//! against INTERRUPT, then the full form as the completion, not raced, so an INTERRUPT arriving
//! during the completion gets the completion's result.
//!
//! [`SlotGuard::interrupted`]: crate::interrupt::SlotGuard::interrupted

use std::io::IoSlice;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;

use dashmap::DashMap;
use tokio::sync::Notify;

use crate::abi::AttrOut;
use crate::abi::CreateOut;
use crate::abi::EntryOut;
use crate::abi::FsyncIn;
use crate::abi::InHeader;
use crate::abi::Opcode;
use crate::abi::OpenOut;
use crate::abi::SetattrIn;
use crate::abi::StatfsOut;
use crate::abi::WriteOut;
use crate::abi::entry::setattr_valid;
use crate::abi::file::fsync_flags;
use crate::codec::dirent::DirentWriter;
use crate::codec::dirent::referenced_nodeids;
use crate::codec::reply::ReplyHeaderBuf;
use crate::codec::reply::encode_data;
use crate::codec::reply::encode_empty;
use crate::codec::reply::encode_error;
use crate::codec::reply::encode_struct;
use crate::codec::request::Request;
use crate::filesystem::EntryRequest;
use crate::filesystem::Fetch;
use crate::filesystem::Filesystem;
use crate::filesystem::RequestInfo;
use crate::filesystem::default_errno;
use crate::interrupt::InFlightSlots;
use crate::metrics::Metrics;
use crate::reply::AttrReply;
use crate::reply::CreateReply;
use crate::reply::EntryReply;
use crate::reply::Errno;
use crate::reply::OpenReply;
use crate::reply::ReadFault;
use crate::reply::ReplyOutcome;
use crate::transport::ReplySender;
use crate::transport::SendOutcome;
use crate::transport::TransportError;
use crate::transport::TransportWorker;

/// Tracks unique ids of requests deferred onto a detached task (decision 3), so `INTERRUPT` can
/// find and wake one after it no longer holds an [`InFlightSlots`] slot. Consulted only after
/// `InFlightSlots` comes up empty; a `DashMap` (cold path only) rather than a fixed table, since
/// the number of concurrently deferred requests is unbounded.
#[derive(Default)]
pub struct DeferredInterrupts {
    map: DashMap<u64, Arc<Notify>>,
    /// Requests deferred since the session started.
    registered: AtomicU64,
}

impl DeferredInterrupts {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many requests were deferred since the session started.
    pub fn deferred_total(&self) -> u64 {
        self.registered.load(Ordering::Relaxed)
    }

    fn register(&self, unique: u64) -> Arc<Notify> {
        self.registered.fetch_add(1, Ordering::Relaxed);
        let notify = Arc::new(Notify::new());
        self.map.insert(unique, Arc::clone(&notify));
        notify
    }

    fn remove(&self, unique: u64) {
        self.map.remove(&unique);
    }

    /// Wakes the deferred task registered for `unique`, if any. A dedicated `Notify` per
    /// unique (never reused for another request) makes `notify_one` safe to call at any time
    /// relative to the task's own `notified().await`: unlike `InFlightSlot`'s per-worker,
    /// reused `Notify`, there is no later, unrelated waiter that could consume a stale permit.
    pub fn interrupt(&self, unique: u64) -> bool {
        match self.map.get(&unique) {
            Some(notify) => {
                notify.notify_one();
                true
            }
            None => false,
        }
    }
}

/// Where a reply goes: the worker that read the request, or, for a deferred request, a handle
/// bound to that worker's fd.
trait ReplyTo {
    fn reply(&mut self, iov: &[IoSlice<'_>]) -> Result<SendOutcome, TransportError>;
}

impl<W: TransportWorker> ReplyTo for W {
    fn reply(&mut self, iov: &[IoSlice<'_>]) -> Result<SendOutcome, TransportError> {
        self.send(iov)
    }
}

impl ReplyTo for ReplySender {
    fn reply(&mut self, iov: &[IoSlice<'_>]) -> Result<SendOutcome, TransportError> {
        self.send(iov)
    }
}

/// Sends one reply and, only when the kernel had already abandoned the request
/// (`SendOutcome::Aborted`), reports `outcome` back to the provider (spec: "Reply accounting").
/// A real transport error (not an abort) propagates so the caller's worker loop can stop.
fn finish_reply<F: Filesystem, R: ReplyTo>(
    fs: &Arc<F>,
    info: &RequestInfo,
    to: &mut R,
    iov: &[IoSlice<'_>],
    outcome: ReplyOutcome,
) -> Result<(), TransportError> {
    if to.reply(iov)? == SendOutcome::Aborted {
        fs.reply_failed(info, &outcome);
    }
    Ok(())
}

/// [`finish_reply`] for a READDIRPLUS reply of the records in `listed`: an aborted one gives
/// back the lookups its records carry ([`give_back_records`]).
fn finish_directory_plus<F: Filesystem, R: ReplyTo>(
    fs: &Arc<F>,
    info: &RequestInfo,
    to: &mut R,
    iov: &[IoSlice<'_>],
    listed: &[u8],
) -> Result<(), TransportError> {
    if to.reply(iov)? == SendOutcome::Aborted {
        give_back_records(fs, info, listed);
    }
    Ok(())
}

/// Reports to the provider, one [`ReplyOutcome::Entry`] each, the lookups the READDIRPLUS
/// records in `listed` carry, which the kernel never takes: those of a reply it aborted, of a
/// page answered with an error, or of a page an INTERRUPT cut short. They are read from the
/// records themselves, so a change since the page was built cannot redirect them.
fn give_back_records<F: Filesystem>(fs: &Arc<F>, info: &RequestInfo, listed: &[u8]) {
    for ino in referenced_nodeids(listed) {
        fs.reply_failed(info, &ReplyOutcome::Entry { ino });
    }
}

/// Dispatches one decoded request: calls the matching `Filesystem` method (if any), encodes and
/// sends exactly one reply (`FORGET`/`BATCH_FORGET`, and an `INTERRUPT` that finds its target,
/// excepted), and records exactly one metrics sample for `header.opcode`. `scratch` backs
/// `READDIR`/`READDIRPLUS` listings, an inline `READ`'s data and a `READLINK` target, since at
/// most one is in use per request.
#[lore_macro::test_pub]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch<F, W>(
    fs: &Arc<F>,
    worker: &mut W,
    worker_index: usize,
    header: &InHeader,
    request: Request<'_>,
    metrics: &Metrics,
    slots: &InFlightSlots,
    deferred: &Arc<DeferredInterrupts>,
    scratch: &mut [u8],
) -> Result<(), TransportError>
where
    F: Filesystem,
    W: TransportWorker,
{
    let start = Instant::now();
    let before = lore_base::allocator::thread_allocation_count();
    let opcode = Opcode::try_from(header.opcode).ok();

    let info = RequestInfo {
        unique: header.unique,
        nodeid: header.nodeid,
        uid: header.uid,
        gid: header.gid,
        pid: header.pid,
    };
    let mut reply_buf = ReplyHeaderBuf::new();

    let result = dispatch_request(
        fs,
        worker,
        worker_index,
        header,
        request,
        &info,
        slots,
        deferred,
        scratch,
        &mut reply_buf,
    )
    .await;

    if let Some(opcode) = opcode {
        let elapsed = start.elapsed();
        let after = lore_base::allocator::thread_allocation_count();
        metrics.record(opcode, elapsed, after.saturating_sub(before));
    }

    result
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_request<F, W>(
    fs: &Arc<F>,
    worker: &mut W,
    worker_index: usize,
    header: &InHeader,
    request: Request<'_>,
    info: &RequestInfo,
    slots: &InFlightSlots,
    deferred: &Arc<DeferredInterrupts>,
    scratch: &mut [u8],
    reply_buf: &mut ReplyHeaderBuf,
) -> Result<(), TransportError>
where
    F: Filesystem,
    W: TransportWorker,
{
    match request {
        // Session::start serves INIT directly, before any worker reaches this dispatcher; a
        // second one reaching here is unexpected. Still reply (never leave the kernel waiting).
        Request::Init(_) => reply_error(fs, info, worker, reply_buf, Errno::EIO),

        Request::Destroy => {
            fs.destroy();
            reply_empty(fs, info, worker, reply_buf, Ok(()))
        }

        Request::Lookup { name } => {
            let guard = slots.begin(worker_index, info.unique);
            let outcome = tokio::select! {
                result = fs.lookup_local(info, header.nodeid, name) => Some(result),
                _ = guard.interrupted() => None,
            };
            drop(guard);

            match outcome {
                Some(Ok(entry)) => reply_entry(fs, info, worker, reply_buf, Ok(entry)),
                Some(Err(ReadFault::Errno(errno))) => {
                    reply_error(fs, info, worker, reply_buf, errno)
                }
                Some(Err(ReadFault::WouldFetch)) => {
                    let request = Deferred::Lookup {
                        parent: header.nodeid,
                        name: name.into(),
                    };
                    defer(fs, worker.reply_sender(), deferred, *info, request);
                    Ok(())
                }
                None => reply_error(fs, info, worker, reply_buf, Errno::EINTR),
            }
        }

        Request::Forget(body) => {
            fs.forget(header.nodeid, body.nlookup);
            Ok(())
        }

        Request::BatchForget(entries) => {
            for entry in entries {
                fs.forget(entry.nodeid, entry.nlookup);
            }
            Ok(())
        }

        Request::Getattr(_body) => {
            let guard = slots.begin(worker_index, info.unique);
            let outcome = tokio::select! {
                result = fs.getattr(info, header.nodeid) => Some(result),
                _ = guard.interrupted() => None,
            };
            drop(guard);

            match outcome {
                Some(result) => reply_attr(fs, info, worker, reply_buf, result),
                None => reply_error(fs, info, worker, reply_buf, Errno::EINTR),
            }
        }

        Request::Statfs => match fs.statfs(info) {
            Ok(st) => {
                let body = StatfsOut { st };
                let iov = encode_struct(info.unique, &body, reply_buf);
                finish_reply(fs, info, worker, &iov, ReplyOutcome::None)
            }
            Err(errno) => reply_error(fs, info, worker, reply_buf, errno),
        },

        Request::Opendir(_body) => {
            let result = fs.opendir(info, header.nodeid);
            reply_open(fs, info, worker, reply_buf, result)
        }

        Request::Open(body) => {
            let result = fs.open(info, header.nodeid, body.flags);
            reply_open(fs, info, worker, reply_buf, result)
        }

        Request::Readdir(body) => {
            let guard = slots.begin(worker_index, info.unique);
            // Bound to the kernel's requested buffer size: `scratch` also backs the (much
            // larger) reply buffer, and a `DirentWriter` filled past what the kernel asked for
            // produces a reply the kernel rejects outright (observed as `EIO` from a real mount
            // under a directory large enough to need more than one READDIR round trip).
            let cap = (body.size as usize).min(scratch.len());
            let mut writer = DirentWriter::new(&mut scratch[..cap]);
            let outcome = tokio::select! {
                result = fs.readdir_local(info, header.nodeid, body.fh, body.offset, &mut writer) => Some(result),
                _ = guard.interrupted() => None,
            };
            drop(guard);

            match outcome {
                Some(Ok(())) => {
                    let used = writer.finish();
                    let iov = encode_data(info.unique, reply_buf, &scratch[..used]);
                    finish_reply(fs, info, worker, &iov, ReplyOutcome::None)
                }
                Some(Err(ReadFault::Errno(errno))) => {
                    reply_error(fs, info, worker, reply_buf, errno)
                }
                Some(Err(ReadFault::WouldFetch)) => {
                    let request = Deferred::Readdir(DeferredListing {
                        ino: header.nodeid,
                        fh: body.fh,
                        offset: body.offset,
                        size: cap,
                    });
                    defer(fs, worker.reply_sender(), deferred, *info, request);
                    Ok(())
                }
                None => reply_error(fs, info, worker, reply_buf, Errno::EINTR),
            }
        }

        Request::Readdirplus(body) => {
            let guard = slots.begin(worker_index, info.unique);
            // See the identical bound in the `Readdir` arm above.
            let cap = (body.size as usize).min(scratch.len());
            let mut writer = DirentWriter::new(&mut scratch[..cap]);
            let outcome = tokio::select! {
                result = fs.readdirplus_local(info, header.nodeid, body.fh, body.offset, &mut writer) => Some(result),
                _ = guard.interrupted() => None,
            };
            drop(guard);

            match outcome {
                Some(Ok(_)) => {
                    let used = writer.finish();
                    let listed = &scratch[..used];
                    let iov = encode_data(info.unique, reply_buf, listed);
                    finish_directory_plus(fs, info, worker, &iov, listed)
                }
                Some(Err(ReadFault::Errno(errno))) => {
                    let used = writer.finish();
                    give_back_records(fs, info, &scratch[..used]);
                    reply_error(fs, info, worker, reply_buf, errno)
                }
                Some(Err(ReadFault::WouldFetch)) => {
                    let request = Deferred::Readdirplus(DeferredListing {
                        ino: header.nodeid,
                        fh: body.fh,
                        offset: body.offset,
                        size: cap,
                    });
                    defer(fs, worker.reply_sender(), deferred, *info, request);
                    Ok(())
                }
                // A page can be cut short after counting rows, while a later row waits.
                None => {
                    let used = writer.finish();
                    give_back_records(fs, info, &scratch[..used]);
                    reply_error(fs, info, worker, reply_buf, Errno::EINTR)
                }
            }
        }

        Request::Releasedir(body) => {
            fs.releasedir(header.nodeid, body.fh);
            reply_empty(fs, info, worker, reply_buf, Ok(()))
        }

        Request::Release(body) => {
            fs.release(header.nodeid, body.fh);
            reply_empty(fs, info, worker, reply_buf, Ok(()))
        }

        Request::Read(body) => {
            let guard = slots.begin(worker_index, info.unique);
            let len = (body.size as usize).min(scratch.len());
            let outcome = tokio::select! {
                result = fs.read(info, header.nodeid, body.fh, body.offset, &mut scratch[..len], false) => {
                    Some(result)
                }
                _ = guard.interrupted() => None,
            };
            drop(guard);

            match outcome {
                Some(Ok(n)) => {
                    let iov = encode_data(info.unique, reply_buf, &scratch[..n]);
                    finish_reply(fs, info, worker, &iov, ReplyOutcome::None)
                }
                Some(Err(ReadFault::Errno(errno))) => {
                    reply_error(fs, info, worker, reply_buf, errno)
                }
                Some(Err(ReadFault::WouldFetch)) => {
                    let request = Deferred::Read {
                        ino: header.nodeid,
                        fh: body.fh,
                        offset: body.offset,
                        size: body.size as usize,
                    };
                    defer(fs, worker.reply_sender(), deferred, *info, request);
                    Ok(())
                }
                None => reply_error(fs, info, worker, reply_buf, Errno::EINTR),
            }
        }

        Request::Interrupt(body) => {
            let target = body.unique;
            if !slots.interrupt(target) && !deferred.interrupt(target) {
                reply_error(fs, info, worker, reply_buf, Errno::EAGAIN)
            } else {
                Ok(())
            }
        }

        Request::Setattr(body) => match fs.setattr_local(info, header.nodeid, body).await {
            Err(ReadFault::WouldFetch) => {
                let request = DeferredWrite::Setattr {
                    ino: header.nodeid,
                    attr: *body,
                };
                defer_write(fs, worker, deferred, info, request)
            }
            result => reply_attr(fs, info, worker, reply_buf, result.map_err(Errno::from)),
        },

        Request::Readlink => {
            let cap = READLINK_MAX.min(scratch.len());
            match fs.readlink(info, header.nodeid, &mut scratch[..cap]).await {
                Ok(n) => {
                    let iov = encode_data(info.unique, reply_buf, &scratch[..n.min(cap)]);
                    finish_reply(fs, info, worker, &iov, ReplyOutcome::None)
                }
                Err(errno) => reply_error(fs, info, worker, reply_buf, errno),
            }
        }

        Request::Symlink { name, target } => {
            match fs.symlink_local(info, header.nodeid, name, target).await {
                Err(ReadFault::WouldFetch) => {
                    let request = DeferredWrite::Symlink {
                        parent: header.nodeid,
                        name: name.into(),
                        target: target.into(),
                    };
                    defer_write(fs, worker, deferred, info, request)
                }
                result => reply_entry(fs, info, worker, reply_buf, result.map_err(Errno::from)),
            }
        }

        Request::Mknod { body, name } => {
            match fs
                .mknod_local(info, header.nodeid, name, body.mode, body.rdev, body.umask)
                .await
            {
                Err(ReadFault::WouldFetch) => {
                    let request = DeferredWrite::Mknod {
                        parent: header.nodeid,
                        name: name.into(),
                        mode: body.mode,
                        rdev: body.rdev,
                        umask: body.umask,
                    };
                    defer_write(fs, worker, deferred, info, request)
                }
                result => reply_entry(fs, info, worker, reply_buf, result.map_err(Errno::from)),
            }
        }

        Request::Mkdir { body, name } => {
            match fs
                .mkdir_local(info, header.nodeid, name, body.mode, body.umask)
                .await
            {
                Err(ReadFault::WouldFetch) => {
                    let request = DeferredWrite::Mkdir {
                        parent: header.nodeid,
                        name: name.into(),
                        mode: body.mode,
                        umask: body.umask,
                    };
                    defer_write(fs, worker, deferred, info, request)
                }
                result => reply_entry(fs, info, worker, reply_buf, result.map_err(Errno::from)),
            }
        }

        Request::Unlink { name } => match fs.unlink_local(info, header.nodeid, name).await {
            Err(ReadFault::WouldFetch) => {
                let request = DeferredWrite::Unlink {
                    parent: header.nodeid,
                    name: name.into(),
                };
                defer_write(fs, worker, deferred, info, request)
            }
            result => reply_empty(fs, info, worker, reply_buf, result.map_err(Errno::from)),
        },

        Request::Rmdir { name } => match fs.rmdir_local(info, header.nodeid, name).await {
            Err(ReadFault::WouldFetch) => {
                let request = DeferredWrite::Rmdir {
                    parent: header.nodeid,
                    name: name.into(),
                };
                defer_write(fs, worker, deferred, info, request)
            }
            result => reply_empty(fs, info, worker, reply_buf, result.map_err(Errno::from)),
        },

        Request::Rename {
            newdir,
            flags,
            name,
            newname,
        } => {
            match fs
                .rename_local(info, header.nodeid, name, newdir, newname, flags)
                .await
            {
                Err(ReadFault::WouldFetch) => {
                    let request = DeferredWrite::Rename {
                        parent: header.nodeid,
                        name: name.into(),
                        newparent: newdir,
                        newname: newname.into(),
                        flags,
                    };
                    defer_write(fs, worker, deferred, info, request)
                }
                result => reply_empty(fs, info, worker, reply_buf, result.map_err(Errno::from)),
            }
        }

        Request::Link { body, name } => {
            let result = fs.link(info, body.oldnodeid, header.nodeid, name).await;
            reply_entry(fs, info, worker, reply_buf, result)
        }

        Request::Create { body, name } => {
            match fs
                .create_local(info, header.nodeid, name, body.mode, body.umask, body.flags)
                .await
            {
                Err(ReadFault::WouldFetch) => {
                    let request = DeferredWrite::Create {
                        parent: header.nodeid,
                        name: name.into(),
                        mode: body.mode,
                        umask: body.umask,
                        flags: body.flags,
                    };
                    defer_write(fs, worker, deferred, info, request)
                }
                result => reply_created(fs, info, worker, reply_buf, result.map_err(Errno::from)),
            }
        }

        Request::Write { body, data } => {
            match fs
                .write_local(
                    info,
                    header.nodeid,
                    body.fh,
                    body.offset,
                    data,
                    body.write_flags,
                )
                .await
            {
                Err(ReadFault::WouldFetch) => {
                    let request = DeferredWrite::Write {
                        ino: header.nodeid,
                        fh: body.fh,
                        offset: body.offset,
                        data: data.into(),
                        write_flags: body.write_flags,
                        size: body.size,
                    };
                    defer_write(fs, worker, deferred, info, request)
                }
                result => {
                    let result = result.map_err(Errno::from);
                    reply_written(fs, info, worker, reply_buf, result, body.size)
                }
            }
        }

        Request::Flush(body) => {
            let result = fs
                .flush(info, header.nodeid, body.fh, body.lock_owner)
                .await;
            reply_empty(fs, info, worker, reply_buf, result)
        }

        Request::Fsync(body) => {
            let datasync = is_datasync(body);
            match fs.fsync_local(info, header.nodeid, body.fh, datasync).await {
                Err(ReadFault::WouldFetch) => {
                    let request = DeferredWrite::Fsync {
                        ino: header.nodeid,
                        datasync,
                    };
                    defer_write(fs, worker, deferred, info, request)
                }
                result => reply_empty(fs, info, worker, reply_buf, result.map_err(Errno::from)),
            }
        }

        Request::Fsyncdir(body) => {
            let result = fs
                .fsyncdir(info, header.nodeid, body.fh, is_datasync(body))
                .await;
            reply_empty(fs, info, worker, reply_buf, result)
        }

        Request::Fallocate(body) => {
            match fs
                .fallocate_local(
                    info,
                    header.nodeid,
                    body.fh,
                    body.offset,
                    body.length,
                    body.mode,
                )
                .await
            {
                Err(ReadFault::WouldFetch) => {
                    let request = DeferredWrite::Fallocate {
                        ino: header.nodeid,
                        fh: body.fh,
                        offset: body.offset,
                        length: body.length,
                        mode: body.mode,
                    };
                    defer_write(fs, worker, deferred, info, request)
                }
                result => reply_empty(fs, info, worker, reply_buf, result.map_err(Errno::from)),
            }
        }

        Request::Unsupported(op) => reply_error(fs, info, worker, reply_buf, default_errno(op)),

        Request::Unknown(_) => reply_error(fs, info, worker, reply_buf, Errno::ENOSYS),
    }
}

/// Longest symlink target a `READLINK` reply carries: `PATH_MAX` less the NUL it counts.
const READLINK_MAX: usize = libc::PATH_MAX as usize - 1;

fn entry_out(entry: &EntryReply) -> EntryOut {
    EntryOut {
        nodeid: entry.ino,
        generation: entry.generation,
        entry_valid: entry.timeouts.entry.as_secs(),
        attr_valid: entry.timeouts.attr.as_secs(),
        entry_valid_nsec: entry.timeouts.entry.subsec_nanos(),
        attr_valid_nsec: entry.timeouts.attr.subsec_nanos(),
        attr: entry.attr,
    }
}

fn is_datasync(body: &FsyncIn) -> bool {
    body.fsync_flags & fsync_flags::FDATASYNC != 0
}

fn reply_error<F: Filesystem, R: ReplyTo>(
    fs: &Arc<F>,
    info: &RequestInfo,
    to: &mut R,
    reply_buf: &mut ReplyHeaderBuf,
    errno: Errno,
) -> Result<(), TransportError> {
    let iov = encode_error(info.unique, errno.0, reply_buf);
    finish_reply(fs, info, to, &iov, ReplyOutcome::None)
}

/// Replies with an entry, or the error. An aborted entry reply reports the entry's lookup.
fn reply_entry<F: Filesystem, R: ReplyTo>(
    fs: &Arc<F>,
    info: &RequestInfo,
    to: &mut R,
    reply_buf: &mut ReplyHeaderBuf,
    result: Result<EntryReply, Errno>,
) -> Result<(), TransportError> {
    match result {
        Ok(entry) => {
            let iov = encode_struct(info.unique, &entry_out(&entry), reply_buf);
            finish_reply(fs, info, to, &iov, ReplyOutcome::Entry { ino: entry.ino })
        }
        Err(errno) => reply_error(fs, info, to, reply_buf, errno),
    }
}

/// Replies with the created entry and its handle, or the error. An aborted reply reports the
/// entry's lookup.
fn reply_created<F: Filesystem, R: ReplyTo>(
    fs: &Arc<F>,
    info: &RequestInfo,
    to: &mut R,
    reply_buf: &mut ReplyHeaderBuf,
    result: Result<CreateReply, Errno>,
) -> Result<(), TransportError> {
    match result {
        Ok(created) => {
            let out = CreateOut {
                entry: entry_out(&created.entry),
                open: open_out(&created.open),
            };
            let iov = encode_struct(info.unique, &out, reply_buf);
            let outcome = ReplyOutcome::Entry {
                ino: created.entry.ino,
            };
            finish_reply(fs, info, to, &iov, outcome)
        }
        Err(errno) => reply_error(fs, info, to, reply_buf, errno),
    }
}

/// Replies with the bytes written, at most `size`, the length the WRITE asked for.
fn reply_written<F: Filesystem, R: ReplyTo>(
    fs: &Arc<F>,
    info: &RequestInfo,
    to: &mut R,
    reply_buf: &mut ReplyHeaderBuf,
    result: Result<usize, Errno>,
    size: u32,
) -> Result<(), TransportError> {
    match result {
        Ok(written) => {
            let out = WriteOut {
                size: u32::try_from(written).map_or(size, |n| n.min(size)),
                padding: 0,
            };
            let iov = encode_struct(info.unique, &out, reply_buf);
            finish_reply(fs, info, to, &iov, ReplyOutcome::None)
        }
        Err(errno) => reply_error(fs, info, to, reply_buf, errno),
    }
}

fn reply_attr<F: Filesystem, R: ReplyTo>(
    fs: &Arc<F>,
    info: &RequestInfo,
    to: &mut R,
    reply_buf: &mut ReplyHeaderBuf,
    result: Result<AttrReply, Errno>,
) -> Result<(), TransportError> {
    match result {
        Ok(attr) => {
            let body = AttrOut {
                attr_valid: attr.timeout.as_secs(),
                attr_valid_nsec: attr.timeout.subsec_nanos(),
                dummy: 0,
                attr: attr.attr,
            };
            let iov = encode_struct(info.unique, &body, reply_buf);
            finish_reply(fs, info, to, &iov, ReplyOutcome::None)
        }
        Err(errno) => reply_error(fs, info, to, reply_buf, errno),
    }
}

fn reply_open<F: Filesystem, R: ReplyTo>(
    fs: &Arc<F>,
    info: &RequestInfo,
    to: &mut R,
    reply_buf: &mut ReplyHeaderBuf,
    result: Result<OpenReply, Errno>,
) -> Result<(), TransportError> {
    match result {
        Ok(open) => {
            let iov = encode_struct(info.unique, &open_out(&open), reply_buf);
            finish_reply(fs, info, to, &iov, ReplyOutcome::None)
        }
        Err(errno) => reply_error(fs, info, to, reply_buf, errno),
    }
}

fn open_out(open: &OpenReply) -> OpenOut {
    OpenOut {
        fh: open.fh,
        open_flags: open.flags,
        backing_id: 0,
    }
}

fn reply_empty<F: Filesystem, R: ReplyTo>(
    fs: &Arc<F>,
    info: &RequestInfo,
    to: &mut R,
    reply_buf: &mut ReplyHeaderBuf,
    result: Result<(), Errno>,
) -> Result<(), TransportError> {
    match result {
        Ok(()) => {
            let iov = encode_empty(info.unique, reply_buf);
            finish_reply(fs, info, to, &iov, ReplyOutcome::None)
        }
        Err(errno) => reply_error(fs, info, to, reply_buf, errno),
    }
}

/// A request answered off its worker, holding what the answer needs.
enum Deferred {
    Read {
        ino: u64,
        fh: u64,
        offset: u64,
        size: usize,
    },
    Lookup {
        parent: u64,
        name: Box<[u8]>,
    },
    Readdir(DeferredListing),
    Readdirplus(DeferredListing),
    Write(DeferredWrite),
}

/// A deferred write-family request: a fetch raced against INTERRUPT, then a completion.
enum DeferredWrite {
    Create {
        parent: u64,
        name: Box<[u8]>,
        mode: u32,
        umask: u32,
        flags: u32,
    },
    Mknod {
        parent: u64,
        name: Box<[u8]>,
        mode: u32,
        rdev: u32,
        umask: u32,
    },
    Mkdir {
        parent: u64,
        name: Box<[u8]>,
        mode: u32,
        umask: u32,
    },
    Symlink {
        parent: u64,
        name: Box<[u8]>,
        target: Box<[u8]>,
    },
    Unlink {
        parent: u64,
        name: Box<[u8]>,
    },
    Rmdir {
        parent: u64,
        name: Box<[u8]>,
    },
    Rename {
        parent: u64,
        name: Box<[u8]>,
        newparent: u64,
        newname: Box<[u8]>,
        flags: u32,
    },
    Write {
        ino: u64,
        fh: u64,
        offset: u64,
        data: Box<[u8]>,
        write_flags: u32,
        size: u32,
    },
    Setattr {
        ino: u64,
        attr: SetattrIn,
    },
    Fallocate {
        ino: u64,
        fh: u64,
        offset: u64,
        length: u64,
        mode: u32,
    },
    Fsync {
        ino: u64,
        datasync: bool,
    },
}

/// The fetch of `request` on `name` in `parent`.
fn entry(parent: u64, name: &[u8], request: EntryRequest) -> Fetch<'_> {
    Fetch::Entry {
        parent,
        name,
        request,
    }
}

impl DeferredWrite {
    /// What the request fetches before its completion.
    fn fetch(&self) -> Fetch<'_> {
        match self {
            Self::Create { parent, name, .. } => entry(*parent, name, EntryRequest::Create),
            Self::Mknod { parent, name, .. } => entry(*parent, name, EntryRequest::Mknod),
            Self::Mkdir { parent, name, .. } => entry(*parent, name, EntryRequest::Mkdir),
            Self::Symlink { parent, name, .. } => entry(*parent, name, EntryRequest::Symlink),
            Self::Unlink { parent, name } => entry(*parent, name, EntryRequest::Unlink),
            Self::Rmdir { parent, name } => entry(*parent, name, EntryRequest::Rmdir),
            Self::Rename {
                parent,
                name,
                newparent,
                newname,
                flags,
            } => Fetch::Rename {
                parent: *parent,
                name,
                newparent: *newparent,
                newname,
                flags: *flags,
            },
            Self::Write { ino, .. } | Self::Fallocate { ino, .. } => Fetch::Content {
                ino: *ino,
                len: None,
            },
            Self::Setattr { ino, attr } => Fetch::Content {
                ino: *ino,
                len: (attr.valid & setattr_valid::SIZE != 0).then_some(attr.size),
            },
            Self::Fsync { ino, datasync } => Fetch::Flush {
                ino: *ino,
                datasync: *datasync,
            },
        }
    }
}

/// A deferred READDIR or READDIRPLUS: its stream, offset and reply size.
struct DeferredListing {
    ino: u64,
    fh: u64,
    offset: u64,
    size: usize,
}

/// Answers `request` on a spawned task (decision 3), so a fetch never holds a worker. The task
/// owns what the answer needs (the allocations this path is allowed), runs the fetching form of
/// the request, and replies through `reply_sender`, bound to the fd the request was read from.
/// It races the answer against [`DeferredInterrupts`] rather than an [`InFlightSlots`] slot,
/// which the request released before this runs.
fn defer<F: Filesystem>(
    fs: &Arc<F>,
    reply_sender: ReplySender,
    deferred: &Arc<DeferredInterrupts>,
    info: RequestInfo,
    request: Deferred,
) {
    let fs = Arc::clone(fs);
    let deferred = Arc::clone(deferred);
    let interrupt = deferred.register(info.unique);
    lore_base::lore_spawn!(async move {
        let mut to = reply_sender;
        // A transport error ends only this reply; the worker's own loop sees the fd close.
        let _ = answer_deferred(&fs, &info, request, &interrupt, &mut to).await;
        deferred.remove(info.unique);
    });
}

/// Defers write-family `request` ([`defer`]); its reply comes from the deferred task.
fn defer_write<F: Filesystem, W: TransportWorker>(
    fs: &Arc<F>,
    worker: &mut W,
    deferred: &Arc<DeferredInterrupts>,
    info: &RequestInfo,
    request: DeferredWrite,
) -> Result<(), TransportError> {
    defer(
        fs,
        worker.reply_sender(),
        deferred,
        *info,
        Deferred::Write(request),
    );
    Ok(())
}

/// Answers a deferred `request` through `to`, or `EINTR` once `interrupt` fires.
async fn answer_deferred<F: Filesystem>(
    fs: &Arc<F>,
    info: &RequestInfo,
    request: Deferred,
    interrupt: &Notify,
    to: &mut ReplySender,
) -> Result<(), TransportError> {
    let mut reply_buf = ReplyHeaderBuf::new();
    match request {
        Deferred::Read {
            ino,
            fh,
            offset,
            size,
        } => {
            let mut buf = vec![0u8; size];
            let outcome = tokio::select! {
                result = fs.read(info, ino, fh, offset, &mut buf, true) => Some(result),
                _ = interrupt.notified() => None,
            };
            match outcome {
                Some(Ok(n)) => {
                    let iov = encode_data(info.unique, &mut reply_buf, &buf[..n]);
                    finish_reply(fs, info, to, &iov, ReplyOutcome::None)
                }
                Some(Err(ReadFault::Errno(errno))) => {
                    reply_error(fs, info, to, &mut reply_buf, errno)
                }
                // A fetching READ cannot defer again.
                Some(Err(ReadFault::WouldFetch)) => {
                    reply_error(fs, info, to, &mut reply_buf, Errno::EIO)
                }
                None => reply_error(fs, info, to, &mut reply_buf, Errno::EINTR),
            }
        }
        Deferred::Lookup { parent, name } => {
            let outcome = tokio::select! {
                result = fs.lookup(info, parent, &name) => Some(result),
                _ = interrupt.notified() => None,
            };
            match outcome {
                Some(result) => reply_entry(fs, info, to, &mut reply_buf, result),
                None => reply_error(fs, info, to, &mut reply_buf, Errno::EINTR),
            }
        }
        Deferred::Readdir(listing) => {
            let mut buf = vec![0u8; listing.size];
            let mut writer = DirentWriter::new(&mut buf);
            let outcome = tokio::select! {
                result = fs.readdir(info, listing.ino, listing.fh, listing.offset, &mut writer) => Some(result),
                _ = interrupt.notified() => None,
            };
            let used = writer.finish();
            reply_listing(fs, info, to, &mut reply_buf, (&buf[..used], false), outcome)
        }
        Deferred::Readdirplus(listing) => {
            let mut buf = vec![0u8; listing.size];
            let mut writer = DirentWriter::new(&mut buf);
            let outcome = tokio::select! {
                result = fs.readdirplus(info, listing.ino, listing.fh, listing.offset, &mut writer) => Some(result),
                _ = interrupt.notified() => None,
            };
            let used = writer.finish();
            let outcome = outcome.map(|result| result.map(|_| ()));
            reply_listing(fs, info, to, &mut reply_buf, (&buf[..used], true), outcome)
        }
        Deferred::Write(request) => {
            let fetched = tokio::select! {
                result = fs.fetch(info, request.fetch()) => Some(result),
                _ = interrupt.notified() => None,
            };
            match fetched {
                Some(Ok(())) => complete_write(fs, info, request, to, &mut reply_buf).await,
                Some(Err(errno)) => reply_error(fs, info, to, &mut reply_buf, errno),
                None => reply_error(fs, info, to, &mut reply_buf, Errno::EINTR),
            }
        }
    }
}

/// Completes a deferred write-family `request` whose fetch is done, not raced against
/// INTERRUPT, and replies with its result. FSYNC's flush was its fetch.
async fn complete_write<F: Filesystem>(
    fs: &Arc<F>,
    info: &RequestInfo,
    request: DeferredWrite,
    to: &mut ReplySender,
    reply_buf: &mut ReplyHeaderBuf,
) -> Result<(), TransportError> {
    match request {
        DeferredWrite::Create {
            parent,
            name,
            mode,
            umask,
            flags,
        } => {
            let result = fs.create(info, parent, &name, mode, umask, flags).await;
            reply_created(fs, info, to, reply_buf, result)
        }
        DeferredWrite::Mknod {
            parent,
            name,
            mode,
            rdev,
            umask,
        } => {
            let result = fs.mknod(info, parent, &name, mode, rdev, umask).await;
            reply_entry(fs, info, to, reply_buf, result)
        }
        DeferredWrite::Mkdir {
            parent,
            name,
            mode,
            umask,
        } => {
            let result = fs.mkdir(info, parent, &name, mode, umask).await;
            reply_entry(fs, info, to, reply_buf, result)
        }
        DeferredWrite::Symlink {
            parent,
            name,
            target,
        } => {
            let result = fs.symlink(info, parent, &name, &target).await;
            reply_entry(fs, info, to, reply_buf, result)
        }
        DeferredWrite::Unlink { parent, name } => {
            let result = fs.unlink(info, parent, &name).await;
            reply_empty(fs, info, to, reply_buf, result)
        }
        DeferredWrite::Rmdir { parent, name } => {
            let result = fs.rmdir(info, parent, &name).await;
            reply_empty(fs, info, to, reply_buf, result)
        }
        DeferredWrite::Rename {
            parent,
            name,
            newparent,
            newname,
            flags,
        } => {
            let result = fs
                .rename(info, parent, &name, newparent, &newname, flags)
                .await;
            reply_empty(fs, info, to, reply_buf, result)
        }
        DeferredWrite::Write {
            ino,
            fh,
            offset,
            data,
            write_flags,
            size,
        } => {
            let result = fs.write(info, ino, fh, offset, &data, write_flags).await;
            reply_written(fs, info, to, reply_buf, result, size)
        }
        DeferredWrite::Setattr { ino, attr } => {
            let result = fs.setattr(info, ino, &attr).await;
            reply_attr(fs, info, to, reply_buf, result)
        }
        DeferredWrite::Fallocate {
            ino,
            fh,
            offset,
            length,
            mode,
        } => {
            let result = fs.fallocate(info, ino, fh, offset, length, mode).await;
            reply_empty(fs, info, to, reply_buf, result)
        }
        DeferredWrite::Fsync { .. } => reply_empty(fs, info, to, reply_buf, Ok(())),
    }
}

/// Replies with the directory entries in `listed`, READDIRPLUS records where `plus`, the error,
/// or `EINTR` for an interrupted request (`None`). A READDIRPLUS page aborted, answered with an
/// error or interrupted gives back the lookups its records carry ([`give_back_records`]).
fn reply_listing<F: Filesystem, R: ReplyTo>(
    fs: &Arc<F>,
    info: &RequestInfo,
    to: &mut R,
    reply_buf: &mut ReplyHeaderBuf,
    (listed, plus): (&[u8], bool),
    outcome: Option<Result<(), Errno>>,
) -> Result<(), TransportError> {
    match outcome {
        Some(Ok(())) => {
            let iov = encode_data(info.unique, reply_buf, listed);
            if plus {
                finish_directory_plus(fs, info, to, &iov, listed)
            } else {
                finish_reply(fs, info, to, &iov, ReplyOutcome::None)
            }
        }
        Some(Err(errno)) => {
            if plus {
                give_back_records(fs, info, listed);
            }
            reply_error(fs, info, to, reply_buf, errno)
        }
        None => {
            if plus {
                give_back_records(fs, info, listed);
            }
            reply_error(fs, info, to, reply_buf, Errno::EINTR)
        }
    }
}
