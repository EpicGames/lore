// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::cell::Cell;
use std::fs::File;
use std::future::Future;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;

use bytes::Bytes;
use bytes::BytesMut;
use io_uring::IoUring;
use io_uring::opcode;
use io_uring::squeue;
use io_uring::types;
use parking_lot::Mutex;

use crate::buffer::StableBuf;
use crate::buffer::StableBufList;
use crate::buffer::StableBufListMut;
use crate::psync::MAX_IO_SEGMENTS;
use crate::psync::PsyncDriver;

const RING_ENTRIES: u32 = 256;

/// Completions taken per completion-queue lock hold, so wakers run outside
/// the lock and one drainer cannot hold it across an unbounded burst.
const DRAIN_BATCH: usize = 64;

/// Poll interval for the reaper while shutdown is pending, bounding the
/// wait for stragglers that completed without signaling.
const SHUTDOWN_POLL_MILLIS: i32 = 10;

/// Ring shard count: one per core, capped at 32.
///
/// A shard bounds how many threads can be inside its ring at once, and where the filesystem lets a
/// read complete during the submit syscall the submitting thread performs the copy itself. Shards
/// are therefore the limit on how much copying can happen in parallel, and one per core removes it.
///
/// This was `clamp(cores / 4, 2, 8)`, which was chosen against ZFS — where reads are punted to
/// kernel workers, so the submitting thread copies nothing and the shard count barely registers.
/// On ext4, where cached reads do complete inline, going from 5 shards to 16 nearly doubled 64 KiB
/// read throughput and took the backend from 0.74× the syscall pool to 1.11× of it. Beyond 16 it
/// flattened, and on ZFS the wider count is slightly better rather than worse, so there is no
/// filesystem this trades against.
fn shard_count() -> usize {
    std::thread::available_parallelism().map_or(2, |count| count.get().clamp(2, 32))
}

enum OpState {
    Waiting(Option<Waker>),
    Completed(i32),
    Abandoned,
}

struct OpShared<P> {
    state: OpState,
    payload: Option<P>,
}

/// The part of an operation a completion can reach without knowing its payload type.
///
/// A completion arrives as a bare `user_data` word, so the thread draining it has no way to name
/// the payload. Erasing the payload behind `Box<dyn Any>` would answer that with a second
/// allocation per operation and a downcast; naming the completion function instead answers it with
/// neither, because the function is monomorphised where the type is still known.
struct OpHeader {
    /// Publishes `result` and releases the kernel's reference. Monomorphised per payload type.
    complete: unsafe fn(*const OpHeader, i32),
}

/// One in-flight ring operation. The payload owns the buffer and the file
/// handle for the operation's whole kernel lifetime: an abandoned future
/// leaves the entry (and therefore the kernel-visible memory) alive until
/// the completion arrives. The kernel side holds one strong reference,
/// passed through `user_data` and reclaimed by whichever thread drains the
/// completion.
///
/// `repr(C)` with the header first is what makes that reference usable: the pointer handed to the
/// kernel is an `Arc<Op<P>>` pointer, and the completion casts it back to `*const OpHeader` to read
/// the function that knows `P`. Reordering these fields would break that cast.
#[repr(C)]
struct Op<P> {
    header: OpHeader,
    shared: Mutex<OpShared<P>>,
}

/// The header sits at offset zero, which every `user_data` round trip depends on. `repr(C)`
/// guarantees it for any payload type, so checking one instantiation checks the guarantee.
const _: () = assert!(std::mem::offset_of!(Op<()>, header) == 0);

/// Publishes a completion and drops the kernel's reference to the operation.
///
/// # Safety
///
/// `header` must be the pointer minted by [`UringDriver::submit`] for an `Op<P>` with this exact
/// `P`, and exactly one completion may call this per submission.
unsafe fn complete_typed<P>(header: *const OpHeader, result: i32) {
    // SAFETY: the caller guarantees this is the `Arc<Op<P>>` pointer submitted with the operation,
    // reclaimed exactly once, and `repr(C)` puts the header at offset zero.
    let op = unsafe { Arc::from_raw(header.cast::<Op<P>>()) };
    let waker = {
        let mut shared = op.shared.lock();
        let previous = std::mem::replace(&mut shared.state, OpState::Completed(result));
        match previous {
            OpState::Waiting(waker) => waker,
            OpState::Abandoned => {
                shared.payload = None;
                None
            }
            OpState::Completed(_) => None,
        }
    };
    if let Some(waker) = waker {
        waker.wake();
    }
}

struct OpPayload<B> {
    buffer: B,
    _file: Arc<File>,
}

/// Iovec array owned by a vectored operation's payload so the kernel can
/// read it for the whole flight of the SQE.
struct OwnedIovecs(Vec<libc::iovec>);

// Safety: the iovec pointers target segments of the StableBuf list moved
// into the same payload, which is itself Send; the pointers are only
// dereferenced by the kernel
unsafe impl Send for OwnedIovecs {}

struct VectoredPayload<B> {
    buffers: B,
    _iovecs: OwnedIovecs,
    _file: Arc<File>,
}

// The iovec builders are plain functions so the raw `Vec<libc::iovec>`
// (not Send) never exists as a local in the async fns, whose futures must
// stay Send; the Send-asserting OwnedIovecs wrapper is built here.
fn prepare_read_iovecs<B: StableBufListMut>(buffers: &mut B, skip: usize) -> OwnedIovecs {
    let mut iovecs: Vec<libc::iovec> = Vec::new();
    let mut remaining_skip = skip;
    for segment in buffers.byte_segments_mut() {
        if iovecs.len() == MAX_IO_SEGMENTS {
            break;
        }
        let len = segment.len();
        if remaining_skip >= len {
            remaining_skip -= len;
            continue;
        }
        iovecs.push(libc::iovec {
            // Safety: remaining_skip is within the segment bounds
            iov_base: unsafe { segment.as_mut_ptr().add(remaining_skip) }.cast(),
            iov_len: len - remaining_skip,
        });
        remaining_skip = 0;
    }
    OwnedIovecs(iovecs)
}

fn prepare_write_iovecs<B: StableBufList>(buffers: &B, skip: usize) -> OwnedIovecs {
    let mut iovecs: Vec<libc::iovec> = Vec::new();
    let mut remaining_skip = skip;
    for segment in buffers.byte_segments() {
        if iovecs.len() == MAX_IO_SEGMENTS {
            break;
        }
        let len = segment.len();
        if remaining_skip >= len {
            remaining_skip -= len;
            continue;
        }
        iovecs.push(libc::iovec {
            // Safety: remaining_skip is within the segment bounds; writes
            // only read through the pointer, so the const-to-mut cast that
            // the iovec layout requires never mutates the segment
            iov_base: unsafe { segment.as_ptr().add(remaining_skip) }
                .cast_mut()
                .cast(),
            iov_len: len - remaining_skip,
        });
        remaining_skip = 0;
    }
    OwnedIovecs(iovecs)
}

struct RingShard {
    ring: IoUring,
    submission: Mutex<()>,
    completion: Mutex<()>,
}

struct UringInner {
    shards: Vec<RingShard>,
    inflight: AtomicUsize,
    shutdown: AtomicBool,
    wake_fd: OwnedFd,
    counts: Counters,
}

#[derive(Default)]
struct Counters {
    submits: AtomicUsize,
    enters: AtomicUsize,
    inline: AtomicUsize,
    reaped: AtomicUsize,
}

/// A snapshot of how the rings are being driven.
///
/// The two ratios are what say whether the backend is earning its complexity. Operations per enter
/// is the batching factor: at one, every operation paid its own syscall — which is what the syscall
/// pool costs already, so the ring is doing more work for the same call count. Inline completions
/// are the ones a submitting thread drained itself; the rest each cost a wake from the reaper.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UringStats {
    /// Operations submitted to a ring.
    pub submits: usize,
    /// `io_uring_enter` calls made.
    pub enters: usize,
    /// Completions drained by a submitting thread, resolving with no reaper wake.
    pub inline: usize,
    /// Completions drained by the reaper, each costing a wake.
    pub reaped: usize,
}

/// Backend submitting data-plane operations (positional read/write, sync)
/// to sharded `io_uring` instances.
///
/// Operations that the kernel completes synchronously during the submit
/// syscall — page-cache hits, the common warm case — are drained by the
/// submitting thread immediately after its enter, so they resolve without
/// a cross-thread round trip; sharding lets those kernel-side copies run
/// in parallel, where a single ring would serialize them under the
/// kernel's per-ring submit lock. Threads stick to one shard for cache
/// locality. A single reaper thread polls the ring fds themselves, which
/// report `POLLIN` while a completion queue is non-empty — level-triggered
/// and independent of how the kernel executed the operation (inline,
/// task-work, or io-wq), where eventfd notification misses task-work
/// completions. Wakes cross to the awaiting executor via plain wakers,
/// keeping the futures runtime-independent.
///
/// Control-plane operations (open, stat, directory ops, whole-file
/// composites) forward to [`PsyncDriver`] and run on the syscall pool. A
/// ring-submitted `statx` or `openat` is punted to a kernel worker making
/// the same blocking call, so it buys no parallelism over the pool; they
/// are defined here rather than left out because a backend owns its whole
/// surface, and linked-SQE chains would replace them here without the
/// dispatch above changing.
pub(crate) struct UringDriver {
    inner: Arc<UringInner>,
    reaper: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl UringDriver {
    pub(crate) fn new() -> std::io::Result<UringDriver> {
        let mut shards = Vec::with_capacity(shard_count());
        for _ in 0..shard_count() {
            shards.push(RingShard {
                ring: IoUring::new(RING_ENTRIES)?,
                submission: Mutex::new(()),
                completion: Mutex::new(()),
            });
        }
        let inner = Arc::new(UringInner {
            shards,
            inflight: AtomicUsize::new(0),
            shutdown: AtomicBool::new(false),
            wake_fd: new_event_fd()?,
            counts: Counters::default(),
        });
        let reaper_inner = Arc::clone(&inner);
        let reaper = std::thread::Builder::new()
            .name("lore-io-uring".to_string())
            .spawn(move || reaper_loop(&reaper_inner))?;
        Ok(UringDriver {
            inner,
            reaper: Mutex::new(Some(reaper)),
        })
    }

    /// Reads up to `max_len` bytes, returning what arrived.
    ///
    /// The buffer is allocated here rather than taken from the caller, and travels into the op
    /// entry so the kernel writes into memory the operation owns for its whole lifetime. That is
    /// what lets an abandoned future be sound: the entry outlives the future, and the completion
    /// frees the buffer once the kernel is finished with it.
    pub(crate) async fn read_at(
        &self,
        file: Arc<File>,
        max_len: usize,
        offset: u64,
    ) -> std::io::Result<Bytes> {
        if max_len == 0 {
            return Ok(Bytes::new());
        }
        // SAFETY: truncated to the byte count the completion reports before it is frozen, so no
        // uninitialised byte escapes.
        let mut buffer = unsafe { crate::buffer::uninit_buffer(max_len) };
        loop {
            let entry = read_entry(&file, &mut buffer, 0, max_len, offset);
            let (payload, result) = self.submit(entry, payload(buffer, &file))?.await;
            buffer = payload.buffer;
            match interpret(result)? {
                Progress::Interrupted => {}
                Progress::Bytes(read) => {
                    buffer.truncate(read);
                    return Ok(buffer.freeze());
                }
            }
        }
    }

    /// Reads exactly `len` bytes.
    ///
    /// Each pass is its own submission, where the psync backend loops inside one dispatch: the
    /// buffer has to travel into the op entry for the kernel to write into it, so it comes back
    /// between passes and there is no thread to hold across them.
    pub(crate) async fn read_exact_at(
        &self,
        file: Arc<File>,
        len: usize,
        offset: u64,
    ) -> std::io::Result<Bytes> {
        // SAFETY: every byte up to `len` is filled before the buffer is frozen, and a short read
        // returns an error rather than the buffer.
        let mut buffer = unsafe { crate::buffer::uninit_buffer(len) };
        let mut done = 0;
        while done < len {
            let entry = read_entry(&file, &mut buffer, done, len - done, offset + done as u64);
            let (payload, result) = self.submit(entry, payload(buffer, &file))?.await;
            buffer = payload.buffer;
            match interpret(result)? {
                Progress::Interrupted => {}
                Progress::Bytes(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "file ended before the requested read length",
                    ));
                }
                Progress::Bytes(read) => done += read,
            }
        }
        Ok(buffer.freeze())
    }

    pub(crate) async fn write_at<B: StableBuf>(
        &self,
        file: Arc<File>,
        buffer: B,
        buffer_offset: usize,
        len: usize,
        offset: u64,
    ) -> std::io::Result<(B, usize)> {
        debug_assert!(buffer_offset + len <= buffer.as_ref().len());
        let mut buffer = buffer;
        loop {
            let entry = write_entry(&file, buffer.as_ref(), buffer_offset, len, offset);
            let (payload, result) = self.submit(entry, payload(buffer, &file))?.await;
            buffer = payload.buffer;
            match interpret(result)? {
                Progress::Interrupted => {}
                Progress::Bytes(written) => return Ok((buffer, written)),
            }
        }
    }

    /// Writes all `len` bytes. See [`Self::read_exact_at`] for why each pass is its own
    /// submission.
    pub(crate) async fn write_all_at<B: StableBuf>(
        &self,
        file: Arc<File>,
        buffer: B,
        len: usize,
        offset: u64,
    ) -> std::io::Result<B> {
        let mut buffer = buffer;
        let mut done = 0;
        while done < len {
            let entry = write_entry(
                &file,
                buffer.as_ref(),
                done,
                len - done,
                offset + done as u64,
            );
            let (payload, result) = self.submit(entry, payload(buffer, &file))?.await;
            buffer = payload.buffer;
            match interpret(result)? {
                Progress::Interrupted => {}
                Progress::Bytes(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "file refused further writes",
                    ));
                }
                Progress::Bytes(written) => done += written,
            }
        }
        Ok(buffer)
    }

    /// Scatters one read across the segments, returning how many bytes it filled.
    ///
    /// The iovec array is rebuilt for every pass because it travels into the op entry with the
    /// segments, and a pass that made partial progress needs a different array anyway: the skip
    /// moves and the leading segments drop out.
    pub(crate) async fn read_vectored_at<B: StableBufListMut>(
        &self,
        file: Arc<File>,
        buffers: B,
        skip: usize,
        offset: u64,
    ) -> std::io::Result<(B, usize)> {
        let mut buffers = buffers;
        loop {
            let iovecs = prepare_read_iovecs(&mut buffers, skip);
            if iovecs.0.is_empty() {
                return Ok((buffers, 0));
            }
            let entry = readv_entry(&file, &iovecs, offset);
            let payload = VectoredPayload {
                buffers,
                _iovecs: iovecs,
                _file: Arc::clone(&file),
            };
            let (payload, result) = self.submit(entry, payload)?.await;
            buffers = payload.buffers;
            match interpret(result)? {
                Progress::Interrupted => {}
                Progress::Bytes(read) => return Ok((buffers, read)),
            }
        }
    }

    /// Gathers one write from the segments, returning how many bytes it consumed. See
    /// [`Self::read_vectored_at`] for why the iovec array is rebuilt per pass.
    pub(crate) async fn write_vectored_at<B: StableBufList>(
        &self,
        file: Arc<File>,
        buffers: B,
        skip: usize,
        offset: u64,
    ) -> std::io::Result<(B, usize)> {
        let mut buffers = buffers;
        loop {
            let iovecs = prepare_write_iovecs(&buffers, skip);
            if iovecs.0.is_empty() {
                return Ok((buffers, 0));
            }
            let entry = writev_entry(&file, &iovecs, offset);
            let payload = VectoredPayload {
                buffers,
                _iovecs: iovecs,
                _file: Arc::clone(&file),
            };
            let (payload, result) = self.submit(entry, payload)?.await;
            buffers = payload.buffers;
            match interpret(result)? {
                Progress::Interrupted => {}
                Progress::Bytes(written) => return Ok((buffers, written)),
            }
        }
    }

    pub(crate) async fn sync(&self, file: Arc<File>, data_only: bool) -> std::io::Result<()> {
        let fd = types::Fd(file.as_raw_fd());
        let mut fsync = opcode::Fsync::new(fd);
        if data_only {
            fsync = fsync.flags(types::FsyncFlags::DATASYNC);
        }
        let entry = fsync.build();
        loop {
            let (_payload, result) = self.submit(entry.clone(), payload((), &file))?.await;
            match interpret(result)? {
                Progress::Interrupted => {}
                Progress::Bytes(_) => return Ok(()),
            }
        }
    }

    pub(crate) async fn open(
        &self,
        options: std::fs::OpenOptions,
        path: PathBuf,
    ) -> std::io::Result<File> {
        PsyncDriver.open(options, path).await
    }

    pub(crate) async fn open_read_head(
        &self,
        options: std::fs::OpenOptions,
        path: PathBuf,
        head_len: usize,
    ) -> std::io::Result<(File, std::fs::Metadata, Bytes)> {
        PsyncDriver.open_read_head(options, path, head_len).await
    }

    pub(crate) async fn read_file_bytes(&self, path: PathBuf) -> std::io::Result<Bytes> {
        PsyncDriver.read_file_bytes(path).await
    }

    pub(crate) async fn read_dir(&self, path: PathBuf) -> std::io::Result<crate::DirStream> {
        PsyncDriver.read_dir(path).await
    }

    pub(crate) async fn set_permissions(
        &self,
        path: PathBuf,
        permissions: std::fs::Permissions,
    ) -> std::io::Result<()> {
        PsyncDriver.set_permissions(path, permissions).await
    }

    pub(crate) async fn write_file_bytes(
        &self,
        path: PathBuf,
        data: Bytes,
        durable: bool,
    ) -> std::io::Result<std::fs::Metadata> {
        PsyncDriver.write_file_bytes(path, data, durable).await
    }

    pub(crate) async fn write_file_segments<B: StableBufList>(
        &self,
        options: std::fs::OpenOptions,
        path: PathBuf,
        buffers: B,
        durable: bool,
    ) -> std::io::Result<()> {
        PsyncDriver
            .write_file_segments(options, path, buffers, durable)
            .await
    }

    pub(crate) async fn write_file_segments_atomic<B: StableBufList>(
        &self,
        options: std::fs::OpenOptions,
        temporary_path: PathBuf,
        final_path: PathBuf,
        buffers: B,
    ) -> std::io::Result<()> {
        PsyncDriver
            .write_file_segments_atomic(options, temporary_path, final_path, buffers)
            .await
    }

    pub(crate) async fn metadata(&self, path: PathBuf) -> std::io::Result<std::fs::Metadata> {
        PsyncDriver.metadata(path).await
    }

    pub(crate) async fn file_metadata(
        &self,
        file: Arc<File>,
    ) -> std::io::Result<std::fs::Metadata> {
        PsyncDriver.file_metadata(file).await
    }

    pub(crate) async fn set_len(&self, file: Arc<File>, len: u64) -> std::io::Result<()> {
        PsyncDriver.set_len(file, len).await
    }

    pub(crate) async fn rename(&self, from: PathBuf, to: PathBuf) -> std::io::Result<()> {
        PsyncDriver.rename(from, to).await
    }

    pub(crate) async fn remove_file(&self, path: PathBuf) -> std::io::Result<()> {
        PsyncDriver.remove_file(path).await
    }

    pub(crate) async fn create_dir_all(&self, path: PathBuf) -> std::io::Result<()> {
        PsyncDriver.create_dir_all(path).await
    }

    pub(crate) async fn remove_dir(&self, path: PathBuf) -> std::io::Result<()> {
        PsyncDriver.remove_dir(path).await
    }

    pub(crate) async fn copy(&self, from: PathBuf, to: PathBuf) -> std::io::Result<u64> {
        PsyncDriver.copy(from, to).await
    }

    pub(crate) async fn remove_dir_all(&self, path: PathBuf) -> std::io::Result<()> {
        PsyncDriver.remove_dir_all(path).await
    }

    pub(crate) fn stats(&self) -> UringStats {
        UringStats {
            submits: self.inner.counts.submits.load(Ordering::Relaxed),
            enters: self.inner.counts.enters.load(Ordering::Relaxed),
            inline: self.inner.counts.inline.load(Ordering::Relaxed),
            reaped: self.inner.counts.reaped.load(Ordering::Relaxed),
        }
    }

    fn submit<P: Send + 'static>(
        &self,
        entry: squeue::Entry,
        payload: P,
    ) -> std::io::Result<OpFuture<P>> {
        let shard = &self.inner.shards[shard_index(self.inner.shards.len())];
        let op = Arc::new(Op {
            header: OpHeader {
                complete: complete_typed::<P>,
            },
            shared: Mutex::new(OpShared {
                state: OpState::Waiting(None),
                payload: Some(payload),
            }),
        });
        self.inner.counts.submits.fetch_add(1, Ordering::Relaxed);
        let user_data = Arc::into_raw(Arc::clone(&op)) as usize as u64;
        let entry = entry.user_data(user_data);
        self.inner.inflight.fetch_add(1, Ordering::SeqCst);

        {
            let _guard = shard.submission.lock();
            loop {
                // Safety: the submission lock makes this the only SQ
                // producer on the shard
                let mut queue = unsafe { shard.ring.submission_shared() };
                // Safety: the buffer and fd live in the op entry until the
                // completion arrives
                let pushed = unsafe { queue.push(&entry).is_ok() };
                queue.sync();
                drop(queue);
                if pushed {
                    break;
                }
                if let Err(error) = flush_submissions(&self.inner, shard) {
                    self.inner.inflight.fetch_sub(1, Ordering::SeqCst);
                    // Safety: the SQE was never pushed, so this reclaims the
                    // kernel-side reference minted above
                    unsafe { drop(Arc::from_raw(user_data as usize as *const Op<P>)) };
                    return Err(error);
                }
            }
        }
        // This enter both submits the SQE and, for operations the kernel
        // finishes synchronously (page-cache hits), posts the completion;
        // the drain below then resolves the future with no reaper round
        // trip. A non-transient error is not fatal here: the SQE stays
        // queued and any later enter on the shard flushes it.
        let _ = flush_submissions(&self.inner, shard);
        let drained = drain_completions(&self.inner, shard);
        self.inner
            .counts
            .inline
            .fetch_add(drained, Ordering::Relaxed);

        Ok(OpFuture {
            entry: op,
            completed: false,
        })
    }
}

impl Drop for UringDriver {
    fn drop(&mut self) {
        self.inner.shutdown.store(true, Ordering::Release);
        signal_event_fd(&self.inner.wake_fd);
        if let Some(reaper) = self.reaper.lock().take() {
            let _ = reaper.join();
        }
    }
}

fn payload<B>(buffer: B, file: &Arc<File>) -> OpPayload<B> {
    OpPayload {
        buffer,
        _file: Arc::clone(file),
    }
}

/// What a completion result means to an operation that can loop.
enum Progress {
    /// A signal arrived before the operation made progress; the caller resubmits it unchanged.
    Interrupted,
    /// The operation transferred this many bytes, zero included.
    Bytes(usize),
}

/// Splits a raw completion result into progress or an error, so the loops above resubmit on
/// `EINTR` for the same reason the psync backend retries it: a signal that ran before the
/// operation made progress carries no cancellation intent, and reporting it would fail an
/// operation that has not failed. Recovering the buffer to resubmit with is why the completion
/// future hands the payload back on every result rather than only on success.
fn interpret(result: i32) -> std::io::Result<Progress> {
    if result == -libc::EINTR {
        return Ok(Progress::Interrupted);
    }
    if result < 0 {
        return Err(std::io::Error::from_raw_os_error(-result));
    }
    Ok(Progress::Bytes(result as usize))
}

/// Builds a read of `len` bytes into `buffer` at `at`.
fn read_entry(
    file: &File,
    buffer: &mut BytesMut,
    at: usize,
    len: usize,
    offset: u64,
) -> squeue::Entry {
    // Safety: `at + len` is within the buffer's stable heap allocation, which the op entry keeps
    // alive until the completion arrives
    let pointer = unsafe { buffer.as_mut_ptr().add(at) };
    opcode::Read::new(types::Fd(file.as_raw_fd()), pointer, len as u32)
        .offset(offset)
        .build()
}

/// Builds a write of `len` bytes from `buffer` at `at`.
fn write_entry(file: &File, buffer: &[u8], at: usize, len: usize, offset: u64) -> squeue::Entry {
    debug_assert!(at + len <= buffer.len());
    // Safety: `at + len` is within the buffer's stable heap allocation, which the op entry keeps
    // alive until the completion arrives
    let pointer = unsafe { buffer.as_ptr().add(at) };
    opcode::Write::new(types::Fd(file.as_raw_fd()), pointer, len as u32)
        .offset(offset)
        .build()
}

/// Builds a scattering read into the segments the iovecs point at.
fn readv_entry(file: &File, iovecs: &OwnedIovecs, offset: u64) -> squeue::Entry {
    // Safety: the segments and the iovec array both live in the op entry until the completion
    // arrives — `StableBufListMut` keeps the segment memory in place, and moving the vector into
    // the payload does not move its heap allocation
    opcode::Readv::new(
        types::Fd(file.as_raw_fd()),
        iovecs.0.as_ptr(),
        iovecs.0.len() as u32,
    )
    .offset(offset)
    .build()
}

/// Builds a gathering write from the segments the iovecs point at.
fn writev_entry(file: &File, iovecs: &OwnedIovecs, offset: u64) -> squeue::Entry {
    // Safety: as in `readv_entry`, with `StableBufList` providing the segment stability
    opcode::Writev::new(
        types::Fd(file.as_raw_fd()),
        iovecs.0.as_ptr(),
        iovecs.0.len() as u32,
    )
    .offset(offset)
    .build()
}

fn new_event_fd() -> std::io::Result<OwnedFd> {
    // Safety: eventfd creates a new descriptor owned by the OwnedFd below
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Safety: fd was just created and is exclusively owned here
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn signal_event_fd(fd: &OwnedFd) {
    let value: u64 = 1;
    // Safety: writes 8 bytes from a valid u64 to an owned eventfd
    unsafe {
        libc::write(
            fd.as_raw_fd(),
            std::ptr::from_ref(&value).cast(),
            std::mem::size_of::<u64>(),
        )
    };
}

fn consume_event_fd(fd: &OwnedFd) {
    let mut value: u64 = 0;
    // Safety: reads 8 bytes into a valid u64 from an owned nonblocking
    // eventfd
    unsafe {
        libc::read(
            fd.as_raw_fd(),
            std::ptr::from_mut(&mut value).cast(),
            std::mem::size_of::<u64>(),
        )
    };
}

/// The submitting thread's shard, assigned round-robin on first use and
/// sticky thereafter so a thread's submissions and inline drains stay on
/// one ring.
fn shard_index(count: usize) -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    thread_local! {
        static HINT: Cell<usize> = const { Cell::new(usize::MAX) };
    }
    HINT.with(|hint| {
        let mut index = hint.get();
        if index == usize::MAX {
            index = NEXT.fetch_add(1, Ordering::Relaxed);
            hint.set(index);
        }
        index % count
    })
}

/// Submits the shard's pending SQEs, absorbing transient errors: `EINTR`
/// retries immediately, and queue-pressure errors drain completions to
/// free ring space before retrying.
fn flush_submissions(inner: &UringInner, shard: &RingShard) -> std::io::Result<()> {
    loop {
        inner.counts.enters.fetch_add(1, Ordering::Relaxed);
        match shard.ring.submitter().submit() {
            Ok(_) => return Ok(()),
            Err(error) => match error.raw_os_error() {
                Some(libc::EINTR) => {}
                Some(libc::EAGAIN | libc::EBUSY | libc::ENOMEM) => {
                    let drained = drain_completions(inner, shard);
                    inner.counts.inline.fetch_add(drained, Ordering::Relaxed);
                    std::thread::yield_now();
                }
                _ => return Err(error),
            },
        }
    }
}

/// Drains and completes whatever the shard's completion queue holds.
///
/// Lock acquisition is opportunistic: while the ring fd reports `POLLIN`
/// for a non-empty completion queue, anything a contended drainer leaves
/// behind wakes the reaper, so skipping under contention loses nothing.
fn drain_completions(inner: &UringInner, shard: &RingShard) -> usize {
    let mut total = 0;
    loop {
        let mut batch = [(0u64, 0i32); DRAIN_BATCH];
        let mut count = 0;
        {
            let Some(_guard) = shard.completion.try_lock() else {
                return total;
            };
            // Safety: the completion lock makes this the only CQ consumer
            // on the shard
            let mut queue = unsafe { shard.ring.completion_shared() };
            while count < DRAIN_BATCH {
                let Some(cqe) = queue.next() else {
                    break;
                };
                batch[count] = (cqe.user_data(), cqe.result());
                count += 1;
            }
            queue.sync();
        }
        for &(user_data, result) in &batch[..count] {
            complete_op(inner, user_data, result);
        }
        total += count;
        if count < DRAIN_BATCH {
            return total;
        }
    }
}

fn complete_op(inner: &UringInner, user_data: u64, result: i32) {
    let header = user_data as usize as *const OpHeader;
    // Safety: every user_data is an `Arc::into_raw` pointer to an `Op<P>` whose header carries the
    // completion function for that `P`, and exactly one completion reclaims it
    let complete = unsafe { (*header).complete };
    inner.inflight.fetch_sub(1, Ordering::SeqCst);
    // Safety: as above — this is that pointer, and this is its one completion
    unsafe { complete(header, result) };
}

fn reaper_loop(inner: &UringInner) {
    loop {
        // The ring fds report POLLIN while their completion queue is
        // non-empty, regardless of how the kernel ran the operation.
        let mut fds: Vec<libc::pollfd> = inner
            .shards
            .iter()
            .map(|shard| shard.ring.as_raw_fd())
            .chain([inner.wake_fd.as_raw_fd()])
            .map(|fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let shutdown = inner.shutdown.load(Ordering::Acquire);
        let timeout = if shutdown { SHUTDOWN_POLL_MILLIS } else { -1 };
        // Safety: fds points at a live, correctly sized pollfd array
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINTR) {
                std::thread::sleep(Duration::from_millis(1));
            }
            continue;
        }
        for (index, shard) in inner.shards.iter().enumerate() {
            if shutdown || fds[index].revents & libc::POLLIN != 0 {
                let drained = drain_completions(inner, shard);
                inner.counts.reaped.fetch_add(drained, Ordering::Relaxed);
            }
        }
        if shutdown {
            consume_event_fd(&inner.wake_fd);
            if inner.inflight.load(Ordering::SeqCst) == 0 {
                return;
            }
        }
    }
}

/// Completion future for one ring operation. Wakes through a plain waker
/// and therefore runs under any executor. Dropping it before completion
/// abandons the operation: whichever thread drains the completion releases
/// the payload when the kernel is done with it.
///
/// The payload comes back on every result, failures included, so a caller can resubmit with the
/// same buffer. Only a submission that never reached the ring consumes it.
struct OpFuture<P> {
    entry: Arc<Op<P>>,
    completed: bool,
}

impl<P: Send + 'static> Future for OpFuture<P> {
    type Output = (P, i32);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut shared = this.entry.shared.lock();
        match &mut shared.state {
            OpState::Completed(result) => {
                let result = *result;
                let payload = shared.payload.take().expect("op payload already taken");
                drop(shared);
                this.completed = true;
                Poll::Ready((payload, result))
            }
            OpState::Waiting(waker) => {
                *waker = Some(cx.waker().clone());
                Poll::Pending
            }
            OpState::Abandoned => unreachable!("abandoned op polled"),
        }
    }
}

impl<P> Drop for OpFuture<P> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut shared = self.entry.shared.lock();
        match shared.state {
            OpState::Waiting(_) => shared.state = OpState::Abandoned,
            OpState::Completed(_) => shared.payload = None,
            OpState::Abandoned => {}
        }
    }
}
