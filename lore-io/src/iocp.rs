// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Overlapped I/O on an I/O completion port: the Windows completion backend.
//!
//! Positional reads and writes are issued on the calling thread as overlapped operations against
//! one completion port, and finished by a reaper thread draining it. Everything else — open, stat,
//! sync, directory operations, the whole-file composites — runs on the syscall pool, because
//! Windows offers no overlapped form of those calls.
//!
//! This is not [`crate::overlapped`], which is the blocking shim `psync` uses on this platform.
//! Both issue an `OVERLAPPED` this crate owns; the difference is only what happens after the
//! submission, a wait on a pool thread there and a completion port here.
//!
//! **A buffered read on an overlapped handle defers.** `ReadFile` reports `ERROR_IO_PENDING`
//! rather than transferring, so an operation costs a syscall on the calling thread and a packet on
//! the port, and no thread is held across the device round trip. That is what this backend is for:
//! the same read through `psync` parks a pool thread until the kernel is done, which makes the
//! pool size a ceiling on how many reads can be in flight. Two threads here against a saturated
//! thirty-two there, at 1.76× and 4.80× the throughput on the synthetic read phases —
//! `lore-io/BENCHMARKS.md` has the figures, and the workload where the trade goes the other way.
//!
//! **Inline completions never reach the port.** `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS` is a
//! correctness requirement rather than the optimisation it looks like: without it an operation the
//! kernel finishes during the call both returns `TRUE` and queues a packet, so the submitting
//! thread would complete it and the reaper would complete it again — a double free of the entry.
//! [`IocpDriver::register`] therefore fails an open that cannot set it.
//! `FILE_SKIP_SET_EVENT_ON_HANDLE` comes with it, because nothing here waits on the file handle's
//! event, and leaving the I/O manager to signal it would make the handle a write-shared cache line
//! between concurrent operations — the sharing this API exists to allow.
//!
//! **One port and one reaper.** A ring serializes submissions under a per-ring lock, which is why
//! [`crate::uring`] shards; a completion port has no such lock and is built for many threads on
//! both sides. On the completion side the reaper count is [`REAPERS`], and one is not a placeholder.
//!
//! Buffer ownership is the ring's contract unchanged: the operation entry owns the buffer and the
//! file handle for the kernel's whole view of the operation, an abandoned future marks the entry
//! rather than freeing anything, and the `OVERLAPPED` the kernel writes into lives in that entry.
use std::cell::UnsafeCell;
use std::fs::File;
use std::future::Future;
use std::os::windows::io::AsRawHandle;
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
use parking_lot::Mutex;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::ERROR_HANDLE_EOF;
use windows_sys::Win32::Foundation::ERROR_IO_PENDING;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::NTSTATUS;
use windows_sys::Win32::Foundation::RtlNtStatusToDosError;
use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
use windows_sys::Win32::Storage::FileSystem::ReadFile;
use windows_sys::Win32::Storage::FileSystem::SetFileCompletionNotificationModes;
use windows_sys::Win32::Storage::FileSystem::WriteFile;
use windows_sys::Win32::System::IO::CreateIoCompletionPort;
use windows_sys::Win32::System::IO::GetQueuedCompletionStatusEx;
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::System::IO::OVERLAPPED_ENTRY;
use windows_sys::Win32::System::IO::PostQueuedCompletionStatus;
use windows_sys::Win32::System::Threading::INFINITE;
use windows_sys::Win32::System::WindowsProgramming::FILE_SKIP_COMPLETION_PORT_ON_SUCCESS;
use windows_sys::Win32::System::WindowsProgramming::FILE_SKIP_SET_EVENT_ON_HANDLE;

use crate::buffer::StableBuf;
use crate::buffer::StableBufList;
use crate::buffer::StableBufListMut;
use crate::psync::PsyncDriver;

/// Completion packets taken per `GetQueuedCompletionStatusEx` call, so a burst costs one syscall
/// per batch rather than one per operation.
const DRAIN_BATCH: usize = 64;

/// Poll interval for the reaper while shutdown is pending, bounding the wait for operations still
/// in flight when the driver went away.
const SHUTDOWN_POLL_MILLIS: u32 = 10;

/// Completion key for the packet [`IocpDriver::drop`] posts. Real operations carry key zero and
/// are told apart by their non-null `OVERLAPPED` anyway; the key is what makes the wake readable.
const SHUTDOWN_KEY: usize = usize::MAX;

/// The reaper count override, read once when a driver is created.
const REAPERS_VAR: &str = "LORE_IO_IOCP_REAPERS";

/// Reaper threads draining the port. One, and it does not scale with the machine.
///
/// A reaper's value is in the batch: one `GetQueuedCompletionStatusEx` takes up to [`DRAIN_BATCH`]
/// packets for a single syscall, and the per-completion work is a lock, a state write and a waker.
/// Threads added to the same port divide the arriving packets between them, so each wakes for a
/// partial batch — the syscall count rises with the thread count while the work does not. Two
/// measure inside the noise of one and four cost most of the throughput; `lore-io/BENCHMARKS.md`
/// has the sweep.
///
/// One thread is also the whole of what this backend adds to a process, which is half of the case
/// for choosing it.
const REAPERS: usize = 1;

/// The reaper count a driver is built with: [`REAPERS`] unless [`REAPERS_VAR`] names a usable one.
///
/// The knob is for re-running the sweep on a machine that is not the one it was tuned on, the same
/// reason [`crate::pool`]'s own override exists, rather than for sizing a deployment. An unusable
/// value reports itself and yields to the default rather than failing, because this crate runs
/// inside host applications and a typo in a diagnostic variable must not take one down on its
/// first file read.
fn configured_reapers() -> usize {
    let Ok(value) = std::env::var(REAPERS_VAR) else {
        return REAPERS;
    };
    match value.trim().parse::<usize>() {
        Ok(count) if (1..=64).contains(&count) => count,
        _ => {
            eprintln!(
                "lore-io: unsupported {REAPERS_VAR} \"{value}\" (expected 1 to 64); \
                 using the default reaper count instead"
            );
            REAPERS
        }
    }
}

enum OpState {
    Waiting(Option<Waker>),
    Completed(std::io::Result<usize>),
    /// Result taken by the future. Distinct from [`OpState::Abandoned`] so that polling a future
    /// past its own completion reports itself rather than looking like a cancellation.
    Taken,
    Abandoned,
}

struct OpShared<P> {
    state: OpState,
    payload: Option<P>,
}

/// The part of an operation a completion can reach without knowing its payload type.
///
/// A packet arrives as a bare `OVERLAPPED` pointer, so the thread draining it has no way to name
/// the payload. Erasing the payload behind `Box<dyn Any>` would answer that with a second
/// allocation per operation and a downcast; naming the completion function instead answers it with
/// neither, because the function is monomorphised where the type is still known.
///
/// `repr(C)` with `overlapped` first is what makes the round trip work: the pointer handed to the
/// kernel *is* this structure's address, so a completion casts it straight back. Reordering these
/// fields would break that cast.
#[repr(C)]
struct OpPrefix {
    /// The kernel writes the operation's status and transferred length here while it runs, which
    /// is why it is a cell rather than a plain field: the awaiting future holds a shared reference
    /// to the same allocation throughout.
    overlapped: UnsafeCell<OVERLAPPED>,
    /// The byte count slot `ReadFile`/`WriteFile` fill on a synchronous completion.
    ///
    /// It lives here rather than on the submitting thread's stack deliberately. A stack slot is
    /// the documented hazard with overlapped I/O — the call may return before the kernel is
    /// finished with the pointer — and a slot in the operation entry has the entry's lifetime,
    /// which is exactly as long as the kernel can touch it.
    transferred: UnsafeCell<u32>,
    /// Publishes the result and releases the kernel's reference. Monomorphised per payload type.
    complete: unsafe fn(*mut OVERLAPPED, std::io::Result<usize>),
}

/// One in-flight operation. The payload owns the buffer and the file handle for the operation's
/// whole kernel lifetime: an abandoned future leaves the entry — and therefore the kernel-visible
/// memory — alive until the completion arrives. The kernel side holds one strong reference,
/// carried as the `OVERLAPPED` pointer and reclaimed by whichever thread finishes the operation.
#[repr(C)]
struct Op<P> {
    prefix: OpPrefix,
    shared: Mutex<OpShared<P>>,
}

/// The prefix sits at offset zero of the entry, and the `OVERLAPPED` at offset zero of the prefix.
/// Every completion round trip is those two casts, so this is the layout guarantee the module
/// rests on. `repr(C)` gives it for any payload type, which is why checking one instantiation
/// checks all of them.
const _: () = assert!(std::mem::offset_of!(Op<()>, prefix) == 0);
const _: () = assert!(std::mem::offset_of!(OpPrefix, overlapped) == 0);

// SAFETY: the cells hold memory only one party writes at a time — the kernel while the operation
// is in flight, and nobody afterwards. The submitting thread reads `transferred` only on a
// synchronous completion, which is by definition after the kernel is finished, and the reaper
// takes its byte count from the completion packet rather than from either cell. `P` carries the
// buffer and the file handle, so its own bound is what decides whether an entry may cross threads.
unsafe impl<P: Send> Send for Op<P> {}
// SAFETY: as above.
unsafe impl<P: Send> Sync for Op<P> {}

/// Publishes a completion and drops the kernel's reference to the operation.
///
/// # Safety
///
/// `overlapped` must be the pointer minted by [`IocpDriver::submit`] for an `Op<P>` with this
/// exact `P`, and exactly one completion may call this per submission.
unsafe fn complete_typed<P>(overlapped: *mut OVERLAPPED, result: std::io::Result<usize>) {
    // SAFETY: the caller guarantees this is the `Arc<Op<P>>` pointer submitted with the operation,
    // reclaimed exactly once, and `repr(C)` puts the overlapped at offset zero of the entry.
    let op = unsafe { Arc::from_raw(overlapped.cast_const().cast::<Op<P>>()) };
    let waker = {
        let mut shared = op.shared.lock();
        let previous = std::mem::replace(&mut shared.state, OpState::Completed(result));
        match previous {
            OpState::Waiting(waker) => waker,
            OpState::Abandoned => {
                shared.payload = None;
                None
            }
            OpState::Completed(_) | OpState::Taken => None,
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

fn payload<B>(buffer: B, file: &Arc<File>) -> OpPayload<B> {
    OpPayload {
        buffer,
        _file: Arc::clone(file),
    }
}

/// The completion port, closed when the last reference to the driver's state goes away — which is
/// after the reaper has joined and nothing is in flight.
struct Port(HANDLE);

// SAFETY: a completion port handle is usable from any thread, which is the whole point of one.
unsafe impl Send for Port {}
// SAFETY: as above.
unsafe impl Sync for Port {}

impl Drop for Port {
    fn drop(&mut self) {
        // SAFETY: Calling OS functions. The handle came from `CreateIoCompletionPort` and this
        // value owns it.
        unsafe { CloseHandle(self.0) };
    }
}

struct IocpInner {
    port: Port,
    inflight: AtomicUsize,
    shutdown: AtomicBool,
    counts: Counters,
}

#[derive(Default)]
struct Counters {
    submits: AtomicUsize,
    inline: AtomicUsize,
    reaped: AtomicUsize,
}

/// A snapshot of how the port is being driven.
///
/// The inline share is what says whether this backend is earning its complexity on a given
/// workload. An operation completed inline cost one syscall on the calling thread and no wake at
/// all; a reaped one cost a packet, a batch drain and a cross-thread wake, which is the same shape
/// of round trip the syscall pool charges.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IocpStats {
    /// Operations issued against the port's handles.
    pub submits: usize,
    /// Operations the kernel finished during the issuing call, resolving with no packet.
    pub inline: usize,
    /// Operations completed through the port, each costing a wake.
    pub reaped: usize,
}

/// Backend issuing data-plane operations (positional read and write) as overlapped I/O against a
/// single completion port.
///
/// Control-plane operations (open, stat, sync, directory operations, whole-file composites)
/// forward to [`PsyncDriver`] and run on the syscall pool. Windows offers no overlapped form of
/// `FlushFileBuffers`, `CreateFile` or the metadata calls: they block whoever issues them, so a
/// pool thread is the right place for them and this backend cannot improve on it. They are defined
/// here rather than left out because a backend owns its whole surface.
pub(crate) struct IocpDriver {
    inner: Arc<IocpInner>,
    reapers: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl IocpDriver {
    pub(crate) fn new() -> std::io::Result<IocpDriver> {
        // SAFETY: Calling OS functions. `INVALID_HANDLE_VALUE` with no existing port is the
        // documented way to create one; zero concurrent threads means "as many as there are
        // processors", which only bounds runnable waiters and this has one.
        let port =
            unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 0) };
        if port.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let inner = Arc::new(IocpInner {
            port: Port(port),
            inflight: AtomicUsize::new(0),
            shutdown: AtomicBool::new(false),
            counts: Counters::default(),
        });
        // Threads all wait on the same port, which is what a port is for: the kernel hands each
        // packet to exactly one of them, so no coordination is needed here beyond counting them.
        let mut reapers = Vec::new();
        for index in 0..configured_reapers() {
            let reaper_inner = Arc::clone(&inner);
            reapers.push(
                std::thread::Builder::new()
                    .name(format!("lore-io-iocp-{index}"))
                    .spawn(move || reaper_loop(&reaper_inner))?,
            );
        }
        Ok(IocpDriver {
            inner,
            reapers: Mutex::new(reapers),
        })
    }

    /// Binds a freshly opened handle to the port and turns off the notifications this backend does
    /// not use.
    ///
    /// Both settings are required rather than preferred, so a failure fails the open. Without
    /// `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS` a synchronous success both returns `TRUE` and queues
    /// a packet, and the submitting thread and the reaper would each complete the same operation —
    /// a double free of the entry rather than a wrong byte count. Reporting the error at the open
    /// leaves the caller a backend to fall back to; guessing here would not.
    fn register(&self, file: &File) -> std::io::Result<()> {
        let handle: HANDLE = file.as_raw_handle();
        // SAFETY: Calling OS functions. The handle is open and owned by `file`, and a completion
        // key of zero is what every operation here carries.
        let bound = unsafe { CreateIoCompletionPort(handle, self.inner.port.0, 0, 0) };
        if bound.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let modes = (FILE_SKIP_COMPLETION_PORT_ON_SUCCESS | FILE_SKIP_SET_EVENT_ON_HANDLE) as u8;
        // SAFETY: Calling OS functions, on the handle just bound above.
        if unsafe { SetFileCompletionNotificationModes(handle, modes) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Reads up to `max_len` bytes, returning what arrived.
    ///
    /// The buffer is allocated here rather than taken from the caller, and travels into the
    /// operation entry so the kernel writes into memory the operation owns for its whole lifetime.
    /// That is what lets an abandoned future be sound: the entry outlives the future, and the
    /// completion frees the buffer once the kernel is finished with it.
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
        let buffer = unsafe { crate::buffer::uninit_buffer(max_len) };
        let (payload, result) = self.read(&file, buffer, 0, max_len, offset).await;
        let mut buffer = payload.buffer;
        buffer.truncate(at_eof(result)?);
        Ok(buffer.freeze())
    }

    /// Reads exactly `len` bytes.
    ///
    /// Each pass is its own submission, where the psync backend loops inside one dispatch: the
    /// buffer has to travel into the entry for the kernel to write into it, so it comes back
    /// between passes and there is no thread held across them.
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
            let (payload, result) = self
                .read(&file, buffer, done, len - done, offset + done as u64)
                .await;
            buffer = payload.buffer;
            match at_eof(result)? {
                0 => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "file ended before the requested read length",
                    ));
                }
                read => done += read,
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
        if len == 0 {
            return Ok((buffer, 0));
        }
        let (payload, result) = self.write(&file, buffer, buffer_offset, len, offset).await;
        let written = result?;
        Ok((payload.buffer, written))
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
            let (payload, result) = self
                .write(&file, buffer, done, len - done, offset + done as u64)
                .await;
            buffer = payload.buffer;
            match result? {
                0 => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "file refused further writes",
                    ));
                }
                written => done += written,
            }
        }
        Ok(buffer)
    }

    /// Scatters a read across the segments, one segment per operation.
    ///
    /// Windows has no positional scatter/gather call for an ordinary handle — `ReadFileScatter`
    /// takes page-aligned, sector-sized segments only — so a segment list becomes a sequence of
    /// operations however it is issued. Issuing them here rather than forwarding to
    /// [`PsyncDriver`] is what keeps them off the pool: the psync backend walks the segments with
    /// blocking positional calls inside one dispatch, holding a thread for the whole list.
    ///
    /// Forwarding would also be unsound. Every operation on a handle registered with the port
    /// queues a packet to it, and the psync path issues its own `OVERLAPPED` from the stack, so the
    /// reaper would take a foreign pointer for an operation entry. The composites below forward
    /// safely because each opens its own handle, which is never registered.
    pub(crate) async fn read_vectored_at<B: StableBufListMut>(
        &self,
        file: Arc<File>,
        buffers: B,
        skip: usize,
        offset: u64,
    ) -> std::io::Result<(B, usize)> {
        match self.read_segment(&file, buffers, skip, offset) {
            Ok(operation) => {
                let (payload, result) = operation.await;
                Ok((payload.buffer, at_eof(result)?))
            }
            Err(buffers) => Ok((buffers, 0)),
        }
    }

    /// Gathers a write from the segments, one segment per operation. See
    /// [`Self::read_vectored_at`].
    pub(crate) async fn write_vectored_at<B: StableBufList>(
        &self,
        file: Arc<File>,
        buffers: B,
        skip: usize,
        offset: u64,
    ) -> std::io::Result<(B, usize)> {
        match self.write_segment(&file, buffers, skip, offset) {
            Ok(operation) => {
                let (payload, result) = operation.await;
                Ok((payload.buffer, result?))
            }
            Err(buffers) => Ok((buffers, 0)),
        }
    }

    /// Issues one read into the first segment `skip` does not cover, handing the segments straight
    /// back when it covers them all.
    ///
    /// Building the operation outside the `async fn` keeps the raw pointer out of the future, which
    /// has to stay `Send`; it is the same reason the ring backend builds its iovec array in a plain
    /// function.
    fn read_segment<B: StableBufListMut>(
        &self,
        file: &Arc<File>,
        mut buffers: B,
        skip: usize,
        offset: u64,
    ) -> Result<OpFuture<OpPayload<B>>, B> {
        let mut remaining = skip;
        let mut target = None;
        for segment in buffers.byte_segments_mut() {
            let len = segment.len();
            if remaining >= len {
                remaining -= len;
                continue;
            }
            // SAFETY: `remaining` is within the segment's bounds, having failed the test above.
            target = Some((
                unsafe { segment.as_mut_ptr().add(remaining) },
                len - remaining,
            ));
            break;
        }
        let Some((pointer, len)) = target else {
            return Err(buffers);
        };
        let len = transfer_len(len);
        let handle: HANDLE = file.as_raw_handle();
        Ok(self.submit(
            handle,
            offset,
            payload(buffers, file),
            |handle, transferred, overlapped| {
                // SAFETY: Calling OS functions. `pointer` and `len` describe segment memory the
                // entry owns for the operation's whole kernel lifetime — moving the segment list
                // into the payload does not move the segments themselves.
                unsafe { ReadFile(handle, pointer, len, transferred, overlapped) }
            },
        ))
    }

    /// Issues one write from the first segment `skip` does not cover. See [`Self::read_segment`].
    fn write_segment<B: StableBufList>(
        &self,
        file: &Arc<File>,
        buffers: B,
        skip: usize,
        offset: u64,
    ) -> Result<OpFuture<OpPayload<B>>, B> {
        let mut remaining = skip;
        let mut target = None;
        for segment in buffers.byte_segments() {
            let len = segment.len();
            if remaining >= len {
                remaining -= len;
                continue;
            }
            // SAFETY: `remaining` is within the segment's bounds, having failed the test above.
            target = Some((unsafe { segment.as_ptr().add(remaining) }, len - remaining));
            break;
        }
        let Some((pointer, len)) = target else {
            return Err(buffers);
        };
        let len = transfer_len(len);
        let handle: HANDLE = file.as_raw_handle();
        Ok(self.submit(
            handle,
            offset,
            payload(buffers, file),
            |handle, transferred, overlapped| {
                // SAFETY: Calling OS functions, as in `read_segment`.
                unsafe { WriteFile(handle, pointer, len, transferred, overlapped) }
            },
        ))
    }

    /// Issues one read of `len` bytes into `buffer` at `at`.
    ///
    /// The pointer is taken before the buffer moves into the payload, which is the same order the
    /// ring backend builds its submission in and for the same reason: the address has to be
    /// handed to the kernel, and the memory has to be owned by the operation.
    fn read(
        &self,
        file: &Arc<File>,
        mut buffer: BytesMut,
        at: usize,
        len: usize,
        offset: u64,
    ) -> OpFuture<OpPayload<BytesMut>> {
        // SAFETY: `at` is within the buffer's stable heap allocation, which the entry keeps alive
        // until the completion arrives.
        let pointer = unsafe { buffer.as_mut_ptr().add(at) };
        let len = transfer_len(len);
        let handle: HANDLE = file.as_raw_handle();
        self.submit(
            handle,
            offset,
            payload(buffer, file),
            |handle, transferred, overlapped| {
                // SAFETY: Calling OS functions. `pointer` and `len` describe memory the entry owns
                // for the operation's whole kernel lifetime.
                unsafe { ReadFile(handle, pointer, len, transferred, overlapped) }
            },
        )
    }

    /// Issues one write of `len` bytes from `buffer` at `at`. See [`Self::read`].
    fn write<B: StableBuf>(
        &self,
        file: &Arc<File>,
        buffer: B,
        at: usize,
        len: usize,
        offset: u64,
    ) -> OpFuture<OpPayload<B>> {
        debug_assert!(at + len <= buffer.as_ref().len());
        // SAFETY: `at` is within the buffer's stable allocation, which the entry keeps alive until
        // the completion arrives.
        let pointer = unsafe { buffer.as_ref().as_ptr().add(at) };
        let len = transfer_len(len);
        let handle: HANDLE = file.as_raw_handle();
        self.submit(
            handle,
            offset,
            payload(buffer, file),
            |handle, transferred, overlapped| {
                // SAFETY: Calling OS functions. `pointer` and `len` describe memory the entry owns
                // for the operation's whole kernel lifetime.
                unsafe { WriteFile(handle, pointer, len, transferred, overlapped) }
            },
        )
    }

    /// Builds an operation entry, issues it, and resolves it immediately if the kernel finished
    /// during the call.
    ///
    /// Three outcomes, and the middle one is the only one that reaches the port. A `TRUE` return
    /// is a synchronous success, and `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS` guarantees no packet
    /// follows it, so this thread owns the completion. `ERROR_IO_PENDING` means the kernel has
    /// accepted the request and owns the buffer until a packet says otherwise. Any other error is
    /// a synchronous failure, which also queues nothing.
    fn submit<P: Send + 'static>(
        &self,
        handle: HANDLE,
        offset: u64,
        payload: P,
        issue: impl FnOnce(HANDLE, *mut u32, *mut OVERLAPPED) -> i32,
    ) -> OpFuture<P> {
        // SAFETY: `OVERLAPPED` is a plain C structure whose all-zero value is the documented
        // starting state, and the offset fields are a union whose other arm this never reads.
        let mut start: OVERLAPPED = unsafe { std::mem::zeroed() };
        start.Anonymous.Anonymous.Offset = offset as u32;
        start.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;

        let op = Arc::new(Op {
            prefix: OpPrefix {
                overlapped: UnsafeCell::new(start),
                transferred: UnsafeCell::new(0),
                complete: complete_typed::<P>,
            },
            shared: Mutex::new(OpShared {
                state: OpState::Waiting(None),
                payload: Some(payload),
            }),
        });
        self.inner.counts.submits.fetch_add(1, Ordering::Relaxed);
        self.inner.inflight.fetch_add(1, Ordering::SeqCst);

        // The kernel's reference, reclaimed by whichever thread completes the operation. The two
        // pointers below are the entry's own address, which the const assertions above pin.
        let entry = Arc::into_raw(Arc::clone(&op)).cast_mut();
        let overlapped = entry.cast::<OVERLAPPED>();
        // SAFETY: `entry` points at a live `Op<P>` this thread just allocated, and `&raw mut`
        // takes the field's address without forming a reference to it.
        let transferred = unsafe { &raw mut (*entry).prefix.transferred }.cast::<u32>();

        let issued = issue(handle, transferred, overlapped);
        if issued != 0 {
            // SAFETY: the kernel is finished with the entry, so the count it wrote is stable.
            let bytes = unsafe { transferred.read() } as usize;
            self.finish(overlapped, Ok(bytes));
        } else {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                self.finish(overlapped, Err(error));
            }
        }

        OpFuture {
            entry: op,
            completed: false,
        }
    }

    /// Completes an operation this thread issued and the kernel finished during the call.
    fn finish(&self, overlapped: *mut OVERLAPPED, result: std::io::Result<usize>) {
        self.inner.counts.inline.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the operation queued no packet, so this is its one and only completion, and the
        // pointer is the one minted for it.
        unsafe { complete_op(&self.inner, overlapped, result) };
    }

    pub(crate) async fn open(
        &self,
        options: std::fs::OpenOptions,
        path: PathBuf,
    ) -> std::io::Result<File> {
        let file = PsyncDriver.open(options, path).await?;
        self.register(&file)?;
        Ok(file)
    }

    /// Syncs through the syscall pool, because Windows has no overlapped `FlushFileBuffers`: it
    /// blocks its caller whatever the handle, so the only question is whose thread it blocks.
    pub(crate) async fn sync(&self, file: Arc<File>, data_only: bool) -> std::io::Result<()> {
        PsyncDriver.sync(file, data_only).await
    }

    /// Opens through the syscall pool and registers the handle, so the positional reads the caller
    /// makes on it afterwards go through the port. The head itself is read before registration, by
    /// the pool thread that opened the file.
    pub(crate) async fn open_read_head(
        &self,
        options: std::fs::OpenOptions,
        path: PathBuf,
        head_len: usize,
    ) -> std::io::Result<(File, std::fs::Metadata, Bytes)> {
        let (file, metadata, head) = PsyncDriver.open_read_head(options, path, head_len).await?;
        self.register(&file)?;
        Ok((file, metadata, head))
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

    pub(crate) async fn holds_name_exactly(&self, path: PathBuf) -> Option<bool> {
        PsyncDriver.holds_name_exactly(path).await
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

    pub(crate) fn stats(&self) -> IocpStats {
        IocpStats {
            submits: self.inner.counts.submits.load(Ordering::Relaxed),
            inline: self.inner.counts.inline.load(Ordering::Relaxed),
            reaped: self.inner.counts.reaped.load(Ordering::Relaxed),
        }
    }
}

impl Drop for IocpDriver {
    /// Wakes every reaper and waits for them.
    ///
    /// One packet per reaper, because a packet is delivered to one waiter: a single one would
    /// leave the rest parked until their shutdown poll expired. A reaper still returns only once
    /// nothing is in flight, so this waits out operations whose future is already gone — which is
    /// the point, since their buffers are still kernel-visible until the completion arrives.
    fn drop(&mut self) {
        let reapers = std::mem::take(&mut *self.reapers.lock());
        self.inner.shutdown.store(true, Ordering::Release);
        for _ in &reapers {
            // SAFETY: Calling OS functions. A null `OVERLAPPED` is what marks this packet as the
            // driver's rather than an operation's.
            unsafe {
                PostQueuedCompletionStatus(self.inner.port.0, 0, SHUTDOWN_KEY, std::ptr::null())
            };
        }
        for reaper in reapers {
            let _ = reaper.join();
        }
    }
}

/// The transfer length one call can carry. Longer requests come back short and the callers that
/// must move every byte loop, which they already do for a short read at end of file.
fn transfer_len(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

/// Reads past the end of a file report `ERROR_HANDLE_EOF` rather than transferring nothing. The
/// API's contract is that an empty read means exactly that, and reporting zero bytes is what
/// `std`'s positional read does with the same status.
fn at_eof(result: std::io::Result<usize>) -> std::io::Result<usize> {
    match result {
        Err(error) if error.raw_os_error() == Some(ERROR_HANDLE_EOF as i32) => Ok(0),
        result => result,
    }
}

/// Publishes a completion, whoever drained it.
///
/// # Safety
///
/// `overlapped` must be the pointer minted by [`IocpDriver::submit`], and exactly one completion
/// may call this per submission.
unsafe fn complete_op(
    inner: &IocpInner,
    overlapped: *mut OVERLAPPED,
    result: std::io::Result<usize>,
) {
    let prefix = overlapped.cast_const().cast::<OpPrefix>();
    // SAFETY: the caller guarantees this is an entry's `OVERLAPPED`, and `repr(C)` puts it at
    // offset zero of the prefix, which carries the function that knows the payload type.
    let complete = unsafe { (*prefix).complete };
    inner.inflight.fetch_sub(1, Ordering::SeqCst);
    // SAFETY: as above — this is that pointer, and this is its one completion.
    unsafe { complete(overlapped, result) };
}

/// Turns a completion packet into the result the awaiting operation sees.
///
/// The packet carries an `NTSTATUS` rather than a Win32 error, and the byte count is only
/// meaningful when that status succeeded.
fn packet_result(entry: &OVERLAPPED_ENTRY) -> std::io::Result<usize> {
    let status = entry.Internal as u32 as NTSTATUS;
    if status >= 0 {
        return Ok(entry.dwNumberOfBytesTransferred as usize);
    }
    // SAFETY: Calling OS functions. A pure translation of a status code to its Win32 equivalent,
    // which is how the same failure would have surfaced from a synchronous call.
    let code = unsafe { RtlNtStatusToDosError(status) };
    Err(std::io::Error::from_raw_os_error(code as i32))
}

fn reaper_loop(inner: &IocpInner) {
    let mut entries = [OVERLAPPED_ENTRY::default(); DRAIN_BATCH];
    loop {
        let shutdown = inner.shutdown.load(Ordering::Acquire);
        let timeout = if shutdown {
            SHUTDOWN_POLL_MILLIS
        } else {
            INFINITE
        };
        let mut removed: u32 = 0;
        // SAFETY: Calling OS functions. `entries` is a live, correctly sized array this thread
        // owns, and the port outlives the reaper because the reaper holds a reference to it.
        let drained = unsafe {
            GetQueuedCompletionStatusEx(
                inner.port.0,
                entries.as_mut_ptr(),
                DRAIN_BATCH as u32,
                &mut removed,
                timeout,
                0,
            )
        };
        if drained == 0 {
            // A timeout is the shutdown poll expiring with nothing to do; anything else is a port
            // that stopped working, where spinning would burn a core to no purpose.
            if std::io::Error::last_os_error().raw_os_error() != Some(WAIT_TIMEOUT as i32) {
                std::thread::sleep(Duration::from_millis(1));
            }
            removed = 0;
        }
        let mut completed = 0;
        for entry in &entries[..removed as usize] {
            // The driver's own wake carries no operation.
            if entry.lpOverlapped.is_null() {
                continue;
            }
            let result = packet_result(entry);
            // SAFETY: every packet with an `OVERLAPPED` was posted by the kernel for an operation
            // this crate submitted, and a packet is delivered exactly once.
            unsafe { complete_op(inner, entry.lpOverlapped, result) };
            completed += 1;
        }
        inner.counts.reaped.fetch_add(completed, Ordering::Relaxed);
        // Re-read rather than reusing the value from the top of the loop: the driver's wake is
        // what delivered this batch, and waiting a poll interval to notice it would put the
        // shutdown latency into every process teardown.
        if inner.shutdown.load(Ordering::Acquire) && inner.inflight.load(Ordering::SeqCst) == 0 {
            return;
        }
    }
}

/// Completion future for one overlapped operation. Wakes through a plain waker and therefore runs
/// under any executor. Dropping it before completion abandons the operation: whichever thread
/// completes it releases the payload when the kernel is done with it.
///
/// The payload comes back on every result, failures included, so a caller can resubmit with the
/// same buffer — which is what lets the read and write loops above continue after a short
/// transfer.
struct OpFuture<P> {
    entry: Arc<Op<P>>,
    completed: bool,
}

impl<P: Send + 'static> Future for OpFuture<P> {
    type Output = (P, std::io::Result<usize>);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut shared = this.entry.shared.lock();
        match &mut shared.state {
            OpState::Completed(_) => {
                let OpState::Completed(result) =
                    std::mem::replace(&mut shared.state, OpState::Taken)
                else {
                    unreachable!("the state was Completed under this lock");
                };
                let payload = shared.payload.take().expect("op payload already taken");
                drop(shared);
                this.completed = true;
                Poll::Ready((payload, result))
            }
            OpState::Waiting(waker) => {
                *waker = Some(cx.waker().clone());
                Poll::Pending
            }
            OpState::Taken => panic!("an overlapped operation was polled after it completed"),
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
            OpState::Taken | OpState::Abandoned => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::windows::io::FromRawHandle;
    use std::sync::atomic::AtomicU64;

    use windows_sys::Win32::Foundation::GENERIC_WRITE;
    use windows_sys::Win32::Storage::FileSystem::CreateFileW;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_NONE;
    use windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING;
    use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_INBOUND;
    use windows_sys::Win32::System::Pipes::CreateNamedPipeW;
    use windows_sys::Win32::System::Pipes::PIPE_TYPE_BYTE;
    use windows_sys::Win32::System::Pipes::PIPE_WAIT;

    use super::*;

    /// A pipe pair whose reading end is overlapped, for the one operation a file cannot be made to
    /// perform on demand.
    ///
    /// Whether a *file* read defers is a property of the host — its filesystem, its cache state and
    /// whatever filters sit in front of them — so a test that reads one asserts nothing it can rely
    /// on. A pipe with no data waiting defers by definition. The handle underneath is an ordinary
    /// overlapped handle bound to the same port, so the submission, the packet, the reaper and the
    /// wake are the file path's exactly.
    fn overlapped_pipe_pair() -> (File, File) {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let name: Vec<u16> = format!(
            "\\\\.\\pipe\\lore-io-iocp-{}-{}\0",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )
        .encode_utf16()
        .collect();

        // SAFETY: Calling OS functions. One instance of a byte-mode pipe, readable by this process
        // and overlapped, named by the null-terminated string above.
        let server = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_INBOUND | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_WAIT,
                1,
                4096,
                4096,
                0,
                std::ptr::null(),
            )
        };
        assert_ne!(
            server,
            INVALID_HANDLE_VALUE,
            "{}",
            std::io::Error::last_os_error()
        );

        // SAFETY: Calling OS functions. Opens the instance created above, which is listening.
        let client = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_NONE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(
            client,
            INVALID_HANDLE_VALUE,
            "{}",
            std::io::Error::last_os_error()
        );

        // SAFETY: both handles were just created and are owned by nothing else.
        unsafe { (File::from_raw_handle(server), File::from_raw_handle(client)) }
    }

    /// The path this backend exists for: an operation the kernel does not finish during the
    /// issuing call is completed by the reaper, through the port, and hands its buffer back.
    ///
    /// The assertion that the first poll is pending is half the test. Without it a run where the
    /// kernel happened to complete inline would still pass, and inline completion is the path this
    /// case is not about.
    #[test]
    fn a_pending_operation_completes_through_the_port() {
        let driver = IocpDriver::new().expect("a completion port");
        let (server, mut client) = overlapped_pipe_pair();
        driver.register(&server).expect("binding the pipe");

        futures::executor::block_on(async {
            let mut read = Box::pin(driver.read_at(Arc::new(server), 8, 0));
            let mut context = Context::from_waker(Waker::noop());
            assert!(
                read.as_mut().poll(&mut context).is_pending(),
                "a read of an empty pipe completed during the issuing call"
            );

            client.write_all(b"deferred").expect("writing to the pipe");
            let bytes = read.await.expect("the deferred read");
            assert_eq!(&bytes[..], b"deferred");
        });

        let stats = driver.stats();
        assert_eq!(stats.reaped, 1, "the completion did not come from the port");
        assert_eq!(stats.inline, 0, "{stats:?}");
    }

    /// Abandoning a pending operation must not free what the kernel still owns. The buffer lives
    /// in the entry, so the completion arriving after the future is gone has somewhere to land —
    /// and the reaper accounting for it is what says the entry was still there to complete.
    #[test]
    fn an_abandoned_pending_operation_is_completed_and_released() {
        let driver = IocpDriver::new().expect("a completion port");
        let (server, mut client) = overlapped_pipe_pair();
        driver.register(&server).expect("binding the pipe");

        let mut read = Box::pin(driver.read_at(Arc::new(server), 8, 0));
        let mut context = Context::from_waker(Waker::noop());
        assert!(read.as_mut().poll(&mut context).is_pending());
        drop(read);

        client.write_all(b"deferred").expect("writing to the pipe");
        for _ in 0..2000 {
            if driver.stats().reaped == 1 {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!(
            "an abandoned operation was never completed: {:?}",
            driver.stats()
        );
    }

    /// Every operation is completed once, by exactly one of the two paths.
    ///
    /// Which path is a property of the host, so the test does not pin it. What it pins is that the
    /// two cannot both claim an operation, which is the failure
    /// `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS` exists to make impossible; the benchmark reports the
    /// share over a real workload.
    #[test]
    fn every_file_operation_is_completed_by_exactly_one_path() {
        let path = std::env::temp_dir().join(format!("lore-io-iocp-count-{}", std::process::id()));
        std::fs::write(&path, vec![7u8; 64 * 1024]).expect("seeding the file");

        let driver = IocpDriver::new().expect("a completion port");
        futures::executor::block_on(async {
            let file = Arc::new(
                driver
                    .open(
                        crate::file::OpenOptions::new().read(true).to_std(),
                        path.clone(),
                    )
                    .await
                    .expect("opening the file"),
            );
            for offset in [0, 16 * 1024, 48 * 1024] {
                let bytes = driver
                    .read_at(Arc::clone(&file), 16 * 1024, offset)
                    .await
                    .expect("reading the file");
                assert_eq!(bytes.len(), 16 * 1024);
                assert!(bytes.iter().all(|byte| *byte == 7));
            }
        });

        let stats = driver.stats();
        assert_eq!(stats.submits, 3, "{stats:?}");
        assert_eq!(stats.submits, stats.inline + stats.reaped, "{stats:?}");
        let _ = std::fs::remove_file(&path);
    }
}
