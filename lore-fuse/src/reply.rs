// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Reply-shaped types the `Filesystem` trait's methods return, and the outcomes reported back
//! through `Filesystem::reply_failed` when the kernel aborts a reply before it lands (spec:
//! "Reply accounting").

use std::time::Duration;

use crate::abi::Attr;

/// How long the kernel may cache an entry or its attributes before asking again.
#[derive(Clone, Copy)]
pub struct Timeouts {
    pub entry: Duration,
    pub attr: Duration,
}

impl Timeouts {
    pub const ZERO: Timeouts = Timeouts {
        entry: Duration::ZERO,
        attr: Duration::ZERO,
    };
}

/// A raw kernel errno, always positive (the codec negates it for the wire).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Errno(pub i32);

impl Errno {
    pub const ENOENT: Errno = Errno(libc::ENOENT);
    pub const EIO: Errno = Errno(libc::EIO);
    pub const EROFS: Errno = Errno(libc::EROFS);
    pub const ENOSYS: Errno = Errno(libc::ENOSYS);
    pub const EINVAL: Errno = Errno(libc::EINVAL);
    pub const ENOTTY: Errno = Errno(libc::ENOTTY);
    pub const EAGAIN: Errno = Errno(libc::EAGAIN);
    pub const EINTR: Errno = Errno(libc::EINTR);
    pub const ENAMETOOLONG: Errno = Errno(libc::ENAMETOOLONG);
    pub const ENOTDIR: Errno = Errno(libc::ENOTDIR);
    pub const EISDIR: Errno = Errno(libc::EISDIR);
    pub const EEXIST: Errno = Errno(libc::EEXIST);
    pub const ENOTEMPTY: Errno = Errno(libc::ENOTEMPTY);
    pub const EACCES: Errno = Errno(libc::EACCES);
    pub const EPERM: Errno = Errno(libc::EPERM);
    pub const EXDEV: Errno = Errno(libc::EXDEV);
    pub const ENOSPC: Errno = Errno(libc::ENOSPC);
    pub const EFBIG: Errno = Errno(libc::EFBIG);
    pub const ENODATA: Errno = Errno(libc::ENODATA);
    pub const ELOOP: Errno = Errno(libc::ELOOP);
    pub const EBADF: Errno = Errno(libc::EBADF);
}

/// Reply to a successful `LOOKUP`, `MKNOD`, `MKDIR`, `SYMLINK` or `LINK`, and the entry half of
/// a `CREATE` reply. Each counts one kernel lookup of `ino`.
pub struct EntryReply {
    pub ino: u64,
    pub generation: u64,
    pub attr: Attr,
    pub timeouts: Timeouts,
}

/// Reply to a successful `GETATTR` or `SETATTR`.
pub struct AttrReply {
    pub attr: Attr,
    pub timeout: Duration,
}

/// Reply to a successful `OPEN`/`OPENDIR`.
pub struct OpenReply {
    pub fh: u64,
    pub flags: u32,
}

/// Reply to a successful `CREATE`: the new entry and the handle the creating open holds.
pub struct CreateReply {
    pub entry: EntryReply,
    pub open: OpenReply,
}

/// The failure of `read` and of the local forms `lookup_local`, `readdir_local` and
/// `readdirplus_local`, beyond a plain errno.
pub enum ReadFault {
    Errno(Errno),
    /// Answering needs a remote fetch. Legal only from `read` with `fetch: false` and from the
    /// local forms; the dispatcher then runs the fetching form (`read` with `fetch: true`,
    /// `lookup`, `readdir`, `readdirplus`) through the deferred path.
    WouldFetch,
}

impl From<Errno> for ReadFault {
    fn from(errno: Errno) -> Self {
        ReadFault::Errno(errno)
    }
}

/// The errno a fault answers where it is not deferred: `WouldFetch` reaching a reply is `EIO`.
impl From<ReadFault> for Errno {
    fn from(fault: ReadFault) -> Self {
        match fault {
            ReadFault::Errno(errno) => errno,
            ReadFault::WouldFetch => Errno::EIO,
        }
    }
}

/// What a reply the kernel never took counted, for `Filesystem::reply_failed` (spec: "Reply
/// accounting").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyOutcome {
    /// One lookup of `ino`: the entry of an aborted reply carrying an [`EntryReply`], `CREATE`
    /// included, or a record of a `READDIRPLUS` page aborted, answered with an error or
    /// interrupted ([`crate::codec::dirent::referenced_nodeids`]).
    Entry {
        ino: u64,
    },
    None,
}
