// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Unsolicited kernel notifications (spec: "Notifications"): invalidating a cached inode's
//! attributes/data range, or a directory entry, from outside any single worker's request loop.
//!
//! Sent with a raw `writev` on the notifier's own cloned `/dev/fuse` fd, never under a lock
//! (binding constraint: "no lock while sending"). The write blocks in the kernel until it holds
//! what it invalidates, whatever the fd's `O_NONBLOCK`: an entry invalidation takes the directory
//! lock a LOOKUP there holds until the daemon answers it, and an inode invalidation waits on the
//! page locks an in-flight READ holds. Send from a thread no request's answer depends on.

use std::io::IoSlice;
use std::mem::size_of;
use std::os::fd::AsRawFd;
use std::os::fd::RawFd;

use zerocopy::IntoBytes;

use crate::abi::NotifyCode;
use crate::abi::NotifyInvalEntryOut;
use crate::abi::NotifyInvalInodeOut;
use crate::abi::OutHeader;
use crate::abi::notify::EXPIRE_ONLY;
use crate::transport::Notifier;
use crate::transport::TransportError;

impl Notifier {
    /// Invalidates cached attributes and, when `len != 0`, a byte range of cached data for
    /// `ino` (`FUSE_NOTIFY_INVAL_INODE`).
    pub fn inval_inode(&self, ino: u64, offset: i64, len: i64) -> Result<(), TransportError> {
        let body = NotifyInvalInodeOut {
            ino,
            off: offset,
            len,
        };
        let header = OutHeader {
            len: (size_of::<OutHeader>() + size_of::<NotifyInvalInodeOut>()) as u32,
            error: NotifyCode::InvalInode as i32,
            unique: 0,
        };
        let iov = [
            IoSlice::new(header.as_bytes()),
            IoSlice::new(body.as_bytes()),
        ];
        writev_notify(self.fd.as_raw_fd(), &iov)
    }

    /// Invalidates a cached directory entry (`FUSE_NOTIFY_INVAL_ENTRY`): `name` under `parent`.
    /// `expire_only` marks the entry as merely expired rather than known-gone.
    pub fn inval_entry(
        &self,
        parent: u64,
        name: &[u8],
        expire_only: bool,
    ) -> Result<(), TransportError> {
        let flags = if expire_only { EXPIRE_ONLY } else { 0 };
        let body = NotifyInvalEntryOut {
            parent,
            namelen: name.len() as u32,
            flags,
        };
        let header = OutHeader {
            len: (size_of::<OutHeader>() + size_of::<NotifyInvalEntryOut>() + name.len() + 1)
                as u32,
            error: NotifyCode::InvalEntry as i32,
            unique: 0,
        };
        let nul = [0u8; 1];
        let iov = [
            IoSlice::new(header.as_bytes()),
            IoSlice::new(body.as_bytes()),
            IoSlice::new(name),
            IoSlice::new(&nul),
        ];
        writev_notify(self.fd.as_raw_fd(), &iov)
    }
}

/// Writes one notification with a raw `writev`, which blocks as the module doc says, mapping the
/// kernel's response the same way [`crate::transport::device`]'s reply path does, except that
/// `ENOENT` here means "the kernel already does not know this inode/entry" -- an expected
/// outcome, not an abort.
fn writev_notify(raw: RawFd, iov: &[IoSlice<'_>]) -> Result<(), TransportError> {
    // SAFETY: `iov` slices are valid for the duration of this call, and `raw` is a valid, open
    // fd.
    let ret = unsafe { libc::writev(raw, iov.as_ptr().cast(), iov.len() as libc::c_int) };
    if ret < 0 {
        return classify_notify_error(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The pure error-code mapping [`writev_notify`] applies, split out so tests can drive it with
/// synthetic errnos rather than needing a real fd in the exact failure state.
#[lore_macro::test_pub]
fn classify_notify_error(error: std::io::Error) -> Result<(), TransportError> {
    match error.raw_os_error() {
        Some(libc::ENOENT) => Ok(()),
        Some(libc::ENOTCONN) => Err(TransportError::Disconnected),
        _ => Err(TransportError::Io(error.to_string())),
    }
}
