// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The real `/dev/fuse` transport: one cloned fd per worker (spec: "Request loop"), driven
//! through `AsyncFd` so `receive`/`send` never block the runtime -- only non-blocking
//! `read`/`writev` on the device fd itself, per the crate's no-blocking-syscalls rule.

use std::io::IoSlice;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::sync::Arc;

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

use crate::buffer::RequestBuffer;
use crate::error::FuseError;
use crate::mount::Mount;
use crate::transport::Notifier;
use crate::transport::ReplySender;
use crate::transport::ReplySink;
use crate::transport::SendOutcome;
use crate::transport::Transport;
use crate::transport::TransportError;
use crate::transport::TransportWorker;

/// Clones the master `/dev/fuse` fd into a private fd on the same connection (kernel
/// `FUSE_DEV_IOC_CLONE`), per the spec's "Request loop": each worker reads/writes its own fd
/// rather than sharing (and locking around) one.
pub const FUSE_DEV_IOC_CLONE: libc::c_ulong = 0x8004_e500;

/// The real device transport: clones a fresh fd per worker from the mount's master fd.
pub struct DeviceTransport {
    mount: Arc<Mount>,
}

impl DeviceTransport {
    pub fn new(mount: Arc<Mount>) -> Self {
        Self { mount }
    }
}

impl Transport for DeviceTransport {
    type Worker = DeviceWorker;

    fn worker(&self) -> Result<Self::Worker, FuseError> {
        let fd = clone_device(self.mount.device_fd())?;
        // SAFETY: the `AsyncFd` takes ownership of `fd`, an `OwnedFd`, so the descriptor stays open
        // and names the same file description until the `AsyncFd` drops it.
        let async_fd = unsafe { AsyncFd::register(fd) }
            .map_err(|error| FuseError::from(std::io::Error::from(error)))?;
        Ok(DeviceWorker {
            fd: Arc::new(async_fd),
        })
    }

    fn notifier(&self) -> Result<Notifier, FuseError> {
        let fd = clone_device(self.mount.device_fd())?;
        Ok(Notifier { fd: Arc::new(fd) })
    }
}

/// One worker's private connection to `/dev/fuse`.
pub struct DeviceWorker {
    /// Shared, not exclusive: task 12's `ReplySender` writes deferred replies to this same fd
    /// from outside the worker's own request loop.
    fd: Arc<AsyncFd<OwnedFd>>,
}

impl AsRawFd for DeviceWorker {
    /// Exposes the underlying fd number, so tests can confirm each worker gets a distinct
    /// clone of the master device fd rather than sharing one.
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.fd.get_ref().as_raw_fd()
    }
}

impl TransportWorker for DeviceWorker {
    async fn receive(&mut self, buffer: &mut RequestBuffer) -> Result<usize, TransportError> {
        loop {
            // A connection abort (unmount) surfaces on this fd as a bare `EPOLLERR`, which
            // `Interest::READABLE` alone never matches (its mask is `READABLE | READ_CLOSED`);
            // `Interest::ERROR` is required to observe it and unblock this loop.
            let mut guard = self
                .fd
                .ready(Interest::READABLE | Interest::ERROR)
                .await
                .map_err(classify_io_error)?;
            let bytes = buffer.as_mut();
            let attempt = guard.try_io(|inner| {
                let raw = inner.get_ref().as_raw_fd();
                // SAFETY: `bytes` is a valid, writable buffer for the duration of this
                // non-blocking read on a fd this worker owns exclusively for reads.
                let ret = unsafe { libc::read(raw, bytes.as_mut_ptr().cast(), bytes.len()) };
                if ret < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(ret as usize)
                }
            });

            match attempt {
                Ok(Ok(n)) => return Ok(n),
                Ok(Err(error)) => return Err(classify_io_error(error)),
                Err(_would_block) => {}
            }
        }
    }

    fn send(&mut self, iov: &[IoSlice<'_>]) -> Result<SendOutcome, TransportError> {
        writev_reply(self.fd.get_ref().as_raw_fd(), iov)
    }

    fn reply_sender(&self) -> ReplySender {
        ReplySender::new(Arc::new(DeviceReplySink {
            fd: Arc::clone(&self.fd),
        }))
    }
}

/// Writes one reply via a raw, non-blocking `writev`. Shared by [`DeviceWorker::send`] (inline
/// replies) and [`DeviceReplySink`] (deferred replies, task 12): both need identical
/// ENOENT-means-`Aborted` handling, since the kernel drops a reply the same way regardless of
/// which fd clone -- the worker's own or a `ReplySender`'s -- writes it.
fn writev_reply(
    raw: std::os::fd::RawFd,
    iov: &[IoSlice<'_>],
) -> Result<SendOutcome, TransportError> {
    // SAFETY: `iov` slices are valid for the duration of this call, and `raw` is a valid, open
    // fd.
    let ret = unsafe { libc::writev(raw, iov.as_ptr().cast(), iov.len() as libc::c_int) };
    if ret < 0 {
        let error = std::io::Error::last_os_error();
        // The kernel already dropped the request this reply answers (e.g. it was interrupted):
        // not a transport failure, just an aborted send.
        return match error.raw_os_error() {
            Some(libc::ENOENT) => Ok(SendOutcome::Aborted),
            _ => Err(classify_io_error(error)),
        };
    }
    Ok(SendOutcome::Sent)
}

/// A [`ReplySender`]'s device-backed target: the same worker fd as a [`DeviceWorker`], reachable
/// from a detached task (decision 3's deferred `READ` path).
struct DeviceReplySink {
    fd: Arc<AsyncFd<OwnedFd>>,
}

impl ReplySink for DeviceReplySink {
    fn send(&self, iov: &[IoSlice<'_>]) -> Result<SendOutcome, TransportError> {
        writev_reply(self.fd.get_ref().as_raw_fd(), iov)
    }
}

/// Maps an I/O error against the device fd to `Disconnected` when the kernel has torn the
/// mount down (spec: unmount observed as `ENODEV`/`ENOTCONN` on the device fd), or `Io`
/// otherwise.
fn classify_io_error(error: std::io::Error) -> TransportError {
    match error.raw_os_error() {
        Some(libc::ENODEV | libc::ENOTCONN) => TransportError::Disconnected,
        _ => TransportError::Io(error.to_string()),
    }
}

fn clone_device(master: BorrowedFd<'_>) -> Result<OwnedFd, FuseError> {
    // SAFETY: opening a device node this process is trusted to talk to directly.
    let raw = unsafe { libc::open(c"/dev/fuse".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: `raw` was just returned by a successful `open` above.
    let clone = unsafe { OwnedFd::from_raw_fd(raw) };

    let master_raw: u32 = master.as_raw_fd() as u32;
    // SAFETY: `clone` is a freshly opened `/dev/fuse` fd; `&master_raw` is a valid pointer to
    // a `u32` for the duration of this call, as `FUSE_DEV_IOC_CLONE` requires.
    let ret = unsafe {
        libc::ioctl(
            clone.as_raw_fd(),
            FUSE_DEV_IOC_CLONE,
            &master_raw as *const u32,
        )
    };
    if ret != 0 {
        return Err(std::io::Error::last_os_error().into());
    }

    // SAFETY: `clone` is a valid, open fd.
    let flags = unsafe { libc::fcntl(clone.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: see above.
    let ret = unsafe { libc::fcntl(clone.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error().into());
    }

    Ok(clone)
}
