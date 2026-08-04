// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Positional reads and writes on Windows, issued through an `OVERLAPPED` this crate owns.
//!
//! Handles opened through this crate are overlapped, because a *synchronous* Windows handle
//! serializes every operation issued on it inside the I/O manager, however many threads are
//! submitting. That defeats the shape the whole API is built around — one shared handle, many
//! concurrent operations at disjoint offsets. Measured on 64 KiB cached reads through one shared
//! handle: 45k ops/s synchronous against 87k overlapped, while separate handles to *separate*
//! files reach 477k, so the serialization is per file object rather than a device limit.
//!
//! `std`'s `seek_read`/`seek_write` cannot be used on such a handle. They put the offset in an
//! `OVERLAPPED` and pass no event, which is only defined for a synchronous handle; against an
//! overlapped one the call may return `ERROR_IO_PENDING`, which `std` reports as an ordinary
//! error — after which the caller frees a buffer the kernel is still writing into. That hazard is
//! recorded in `lore-storage/src/defragment.rs`, where it reads as a reason overlapped handles are
//! unusable; it is really a reason `std`'s shims are, and the difference is this module.
//!
//! So each operation owns its completion: its own `OVERLAPPED`, a per-thread event, and a wait
//! that a pending operation cannot return past. The wait blocks, which is right rather than a
//! concession — these run on the syscall pool, whose purpose is to hold a thread across a blocking
//! syscall, and the pool never abandons a job it has started.
//!
//! **The wait is the common path.** A buffered read on an overlapped handle reports
//! `ERROR_IO_PENDING` rather than transferring, so every positional operation on a handle this
//! crate opened parks its pool thread until the kernel is done — which makes the pool size a
//! ceiling on how many can be in flight, and is why [`crate::iocp`] exists.
//!
//! The inline branch below is not dead for that: the whole-file composites open their own
//! *synchronous* handles through `File::open`, and those complete inside the call.

use std::fs::File;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::ERROR_HANDLE_EOF;
use windows_sys::Win32::Foundation::ERROR_IO_PENDING;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::ReadFile;
use windows_sys::Win32::Storage::FileSystem::WriteFile;
use windows_sys::Win32::System::IO::CancelIoEx;
use windows_sys::Win32::System::IO::GetOverlappedResult;
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::System::Threading::CreateEventW;

/// The event one thread's operations signal completion through, created once and reused.
///
/// Supplying an event is not optional. With a null `hEvent` the I/O manager signals the *file
/// handle* instead, and two concurrent operations on one shared handle cannot both wait on that —
/// which is precisely the sharing this backend exists to allow. One event per thread rather than
/// per operation is enough because a thread runs one operation at a time: the pool hands a thread
/// a job and the job returns before the next begins.
struct CompletionEvent(HANDLE);

impl Drop for CompletionEvent {
    fn drop(&mut self) {
        // SAFETY: Calling OS functions. The handle came from `CreateEventW` and this value owns it.
        unsafe { CloseHandle(self.0) };
    }
}

thread_local! {
    static COMPLETION_EVENT: Option<CompletionEvent> = create_completion_event();
}

fn create_completion_event() -> Option<CompletionEvent> {
    // SAFETY: Calling OS functions. An unnamed, manual-reset, initially unsignalled event with
    // default security. The I/O manager resets it when it takes an operation, so nothing here has
    // to.
    let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    (!handle.is_null()).then_some(CompletionEvent(handle))
}

/// Issues one operation and does not return until the kernel is finished with the buffer.
///
/// `issue` calls the Win32 function with the handle, the byte-count slot and the `OVERLAPPED`, and
/// reports what it returned. A synchronous handle completes inside that call. An overlapped one may
/// report `ERROR_IO_PENDING`, which is not a failure: the kernel has accepted the request and owns
/// the buffer until it completes, so waiting here is what keeps the buffer alive for exactly as
/// long as the kernel can touch it.
fn run(
    file: &File,
    offset: u64,
    issue: impl FnOnce(HANDLE, *mut u32, *mut OVERLAPPED) -> i32,
) -> std::io::Result<usize> {
    COMPLETION_EVENT.with(|event| {
        let event = event.as_ref().ok_or_else(|| {
            std::io::Error::other("no completion event available for this thread")
        })?;
        let handle: HANDLE = file.as_raw_handle();

        // SAFETY: `OVERLAPPED` is a plain C structure whose all-zero value is the documented
        // starting state, and the offset fields are a union whose other arm this never reads.
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.Anonymous.Anonymous.Offset = offset as u32;
        overlapped.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
        overlapped.hEvent = event.0;

        let mut transferred: u32 = 0;
        if issue(handle, &mut transferred, &mut overlapped) != 0 {
            return Ok(transferred as usize);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
            return Err(error);
        }
        wait(handle, &mut overlapped, &mut transferred)
    })
}

/// Waits for a pending operation and reports what it transferred.
fn wait(
    handle: HANDLE,
    overlapped: &mut OVERLAPPED,
    transferred: &mut u32,
) -> std::io::Result<usize> {
    // SAFETY: Calling OS functions. `overlapped` is the structure the pending operation was issued
    // with, and it outlives this call.
    if unsafe { GetOverlappedResult(handle, overlapped, transferred, 1) } != 0 {
        return Ok(*transferred as usize);
    }
    let error = std::io::Error::last_os_error();

    // Two different things look the same here: the operation completing with a failure status, and
    // the wait itself failing. The first is finished with the buffer and the second is not, and
    // returning while the kernel can still write into it is a use-after-free rather than a wrong
    // byte count. Cancelling and waiting again costs nothing in the common case — cancellation is
    // a no-op against an operation that already completed, and a second wait cannot report
    // pending — and in the uncommon one it is the difference between an error and corruption.
    // SAFETY: Calling OS functions, with the same structure the operation was issued with.
    unsafe {
        CancelIoEx(handle, overlapped);
        GetOverlappedResult(handle, overlapped, transferred, 1);
    }
    Err(error)
}

pub(crate) fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    let len = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
    let pointer: *mut u8 = buffer.as_mut_ptr();
    let result = run(file, offset, |handle, transferred, overlapped| {
        // SAFETY: Calling OS functions. `pointer` and `len` describe `buffer`, which outlives the
        // operation because `run` does not return while it is in flight.
        unsafe { ReadFile(handle, pointer, len, transferred, overlapped) }
    });
    match result {
        // A read starting at or past the end of the file reports `ERROR_HANDLE_EOF` rather than
        // transferring nothing. The API's contract is that an empty read means exactly that, and
        // reporting zero bytes is what `std`'s positional read does with the same status.
        Err(error) if error.raw_os_error() == Some(ERROR_HANDLE_EOF as i32) => Ok(0),
        result => result,
    }
}

pub(crate) fn write_at(file: &File, buffer: &[u8], offset: u64) -> std::io::Result<usize> {
    let len = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
    let pointer: *const u8 = buffer.as_ptr();
    run(file, offset, |handle, transferred, overlapped| {
        // SAFETY: Calling OS functions. `pointer` and `len` describe `buffer`, which outlives the
        // operation because `run` does not return while it is in flight.
        unsafe { WriteFile(handle, pointer, len, transferred, overlapped) }
    })
}
