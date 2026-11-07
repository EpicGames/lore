// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The `Filesystem` trait: the interface a provider implements, and the default reply the
//! dispatcher gives every opcode that trait does not cover (spec: "Opcodes").

use std::future::Future;
use std::future::ready;

use crate::abi::Kstatfs;
use crate::abi::Opcode;
use crate::abi::SetattrIn;
use crate::codec::dirent::DirentWriter;
use crate::reply::AttrReply;
use crate::reply::CreateReply;
use crate::reply::EntryReply;
use crate::reply::Errno;
use crate::reply::OpenReply;
use crate::reply::ReadFault;
use crate::reply::ReplyOutcome;

/// Identity and origin of one kernel request, common to every `Filesystem` call.
#[derive(Clone, Copy, Debug)]
pub struct RequestInfo {
    pub unique: u64,
    pub nodeid: u64,
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
}

/// What a deferred request fetches before its completion runs ([`Filesystem::fetch`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fetch<'a> {
    /// What `request` of `name` in `parent` reads.
    Entry {
        parent: u64,
        name: &'a [u8],
        request: EntryRequest,
    },
    /// What a RENAME of `name` in `parent` to `newname` in `newparent`, with the `renameat2(2)`
    /// `flags`, reads.
    Rename {
        parent: u64,
        name: &'a [u8],
        newparent: u64,
        newname: &'a [u8],
        flags: u32,
    },
    /// The content a WRITE, SETATTR or FALLOCATE of `ino` copies up: its first `len` bytes, a
    /// SETATTR's new size, or all of it.
    Content { ino: u64, len: Option<u64> },
    /// FSYNC's flush of `ino`; the request completes with the flush.
    Flush { ino: u64, datasync: bool },
}

/// The requests whose fetch is [`Fetch::Entry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryRequest {
    Create,
    Mknod,
    Mkdir,
    Symlink,
    Unlink,
    Rmdir,
}

impl EntryRequest {
    /// Whether the request attaches a new node: CREATE, MKNOD, MKDIR and SYMLINK.
    pub fn attaches(self) -> bool {
        matches!(
            self,
            Self::Create | Self::Mknod | Self::Mkdir | Self::Symlink
        )
    }
}

/// What a provider implements to answer the protocol layer's requests. Every method that can
/// fail returns a plain [`Errno`], except `read` and the local forms (`lookup_local`,
/// `create_local`, ...), which return a [`ReadFault`] so that a miss needing a remote fetch can
/// be deferred. Opcodes this trait does not cover get [`default_errno`] instead of ever reaching
/// a provider. The write-family methods default to [`default_errno`] too, so a read-only
/// provider implements none of them.
///
/// A write-family request whose local form answers `WouldFetch` changes nothing on the worker.
/// Off the worker, [`Self::fetch`] runs raced against INTERRUPT, then the full form runs as its
/// completion, not raced: it resolves again whatever it depends on, as the directory may have
/// changed meanwhile.
pub trait Filesystem: Send + Sync + 'static {
    fn lookup(
        &self,
        req: &RequestInfo,
        parent: u64,
        name: &[u8],
    ) -> impl Future<Output = Result<EntryReply, Errno>> + Send;

    fn forget(&self, ino: u64, nlookup: u64);

    fn getattr(
        &self,
        req: &RequestInfo,
        ino: u64,
    ) -> impl Future<Output = Result<AttrReply, Errno>> + Send;

    fn statfs(&self, req: &RequestInfo) -> Result<Kstatfs, Errno>;

    fn opendir(&self, req: &RequestInfo, ino: u64) -> Result<OpenReply, Errno>;

    /// `fh` is the handle `opendir` returned, so a directory stream can stay on the listing it
    /// pinned even when another stream on the same directory starts after a swap.
    fn readdir(
        &self,
        req: &RequestInfo,
        ino: u64,
        fh: u64,
        offset: u64,
        out: &mut DirentWriter<'_>,
    ) -> impl Future<Output = Result<(), Errno>> + Send;

    /// [`Self::readdir`] with each entry's attributes, answering how many lookups it counted.
    /// The provider counts one lookup for each record it writes with a nonzero node id, other than
    /// `.` and `..`, and no other: the kernel takes exactly those references, and the dispatcher
    /// gives back those of a page the kernel never takes, by reading the records back
    /// ([`crate::codec::dirent::referenced_nodeids`], [`Self::reply_failed`]). The kernel takes
    /// none from a page aborted, interrupted, or answered with an error, whatever it wrote.
    fn readdirplus(
        &self,
        req: &RequestInfo,
        ino: u64,
        fh: u64,
        offset: u64,
        out: &mut DirentWriter<'_>,
    ) -> impl Future<Output = Result<usize, Errno>> + Send;

    /// LOOKUP answered without a remote fetch. `WouldFetch` means answering needs one; the
    /// dispatcher then runs [`Self::lookup`] off the worker. Defaults to [`Self::lookup`].
    fn lookup_local(
        &self,
        req: &RequestInfo,
        parent: u64,
        name: &[u8],
    ) -> impl Future<Output = Result<EntryReply, ReadFault>> + Send {
        async move { Ok(self.lookup(req, parent, name).await?) }
    }

    /// READDIR answered without a remote fetch, as [`Self::lookup_local`] is; `WouldFetch`
    /// leaves `out` empty. Defaults to [`Self::readdir`].
    fn readdir_local(
        &self,
        req: &RequestInfo,
        ino: u64,
        fh: u64,
        offset: u64,
        out: &mut DirentWriter<'_>,
    ) -> impl Future<Output = Result<(), ReadFault>> + Send {
        async move { Ok(self.readdir(req, ino, fh, offset, out).await?) }
    }

    /// READDIRPLUS answered without a remote fetch, as [`Self::lookup_local`] is; `WouldFetch`
    /// leaves `out` empty and counts no lookup. Defaults to [`Self::readdirplus`].
    fn readdirplus_local(
        &self,
        req: &RequestInfo,
        ino: u64,
        fh: u64,
        offset: u64,
        out: &mut DirentWriter<'_>,
    ) -> impl Future<Output = Result<usize, ReadFault>> + Send {
        async move { Ok(self.readdirplus(req, ino, fh, offset, out).await?) }
    }

    fn releasedir(&self, ino: u64, fh: u64);

    fn open(&self, req: &RequestInfo, ino: u64, flags: u32) -> Result<OpenReply, Errno>;

    #[allow(clippy::too_many_arguments)]
    fn read(
        &self,
        req: &RequestInfo,
        ino: u64,
        fh: u64,
        offset: u64,
        out: &mut [u8],
        fetch: bool,
    ) -> impl Future<Output = Result<usize, ReadFault>> + Send;

    fn release(&self, ino: u64, fh: u64);

    /// Gives back what a reply the kernel never took counted (spec: "Reply accounting").
    fn reply_failed(&self, req: &RequestInfo, outcome: &ReplyOutcome);

    fn destroy(&self) {}

    /// Applies the attributes `attr.valid` names (`abi::entry::setattr_valid`) and returns the
    /// result.
    fn setattr(
        &self,
        _req: &RequestInfo,
        _ino: u64,
        _attr: &SetattrIn,
    ) -> impl Future<Output = Result<AttrReply, Errno>> + Send {
        ready(Err(default_errno(Opcode::Setattr)))
    }

    /// Writes a symlink's target into `out`, without a NUL, and returns its length. `out` holds
    /// at most `PATH_MAX - 1` bytes.
    fn readlink(
        &self,
        _req: &RequestInfo,
        _ino: u64,
        _out: &mut [u8],
    ) -> impl Future<Output = Result<usize, Errno>> + Send {
        ready(Err(default_errno(Opcode::Readlink)))
    }

    fn symlink(
        &self,
        _req: &RequestInfo,
        _parent: u64,
        _name: &[u8],
        _target: &[u8],
    ) -> impl Future<Output = Result<EntryReply, Errno>> + Send {
        ready(Err(default_errno(Opcode::Symlink)))
    }

    #[allow(clippy::too_many_arguments)]
    fn mknod(
        &self,
        _req: &RequestInfo,
        _parent: u64,
        _name: &[u8],
        _mode: u32,
        _rdev: u32,
        _umask: u32,
    ) -> impl Future<Output = Result<EntryReply, Errno>> + Send {
        ready(Err(default_errno(Opcode::Mknod)))
    }

    fn mkdir(
        &self,
        _req: &RequestInfo,
        _parent: u64,
        _name: &[u8],
        _mode: u32,
        _umask: u32,
    ) -> impl Future<Output = Result<EntryReply, Errno>> + Send {
        ready(Err(default_errno(Opcode::Mkdir)))
    }

    fn unlink(
        &self,
        _req: &RequestInfo,
        _parent: u64,
        _name: &[u8],
    ) -> impl Future<Output = Result<(), Errno>> + Send {
        ready(Err(default_errno(Opcode::Unlink)))
    }

    fn rmdir(
        &self,
        _req: &RequestInfo,
        _parent: u64,
        _name: &[u8],
    ) -> impl Future<Output = Result<(), Errno>> + Send {
        ready(Err(default_errno(Opcode::Rmdir)))
    }

    /// `RENAME` and `RENAME2`. `flags` holds the `renameat2(2)` `RENAME_*` flags; `RENAME`
    /// passes 0.
    #[allow(clippy::too_many_arguments)]
    fn rename(
        &self,
        _req: &RequestInfo,
        _parent: u64,
        _name: &[u8],
        _newparent: u64,
        _newname: &[u8],
        _flags: u32,
    ) -> impl Future<Output = Result<(), Errno>> + Send {
        ready(Err(default_errno(Opcode::Rename)))
    }

    /// Links the existing `ino` into `newparent` as `newname`.
    fn link(
        &self,
        _req: &RequestInfo,
        _ino: u64,
        _newparent: u64,
        _newname: &[u8],
    ) -> impl Future<Output = Result<EntryReply, Errno>> + Send {
        ready(Err(default_errno(Opcode::Link)))
    }

    /// Creates and opens a regular file. The kernel sends `RELEASE` for the returned handle,
    /// and passes it in `WRITE`, `FLUSH` and `FSYNC` on the created file's descriptors.
    #[allow(clippy::too_many_arguments)]
    fn create(
        &self,
        _req: &RequestInfo,
        _parent: u64,
        _name: &[u8],
        _mode: u32,
        _umask: u32,
        _flags: u32,
    ) -> impl Future<Output = Result<CreateReply, Errno>> + Send {
        ready(Err(default_errno(Opcode::Create)))
    }

    /// Writes `data`, borrowed from the request buffer, and returns the bytes written. `fh` is
    /// the handle `open` or `create` returned, or 0 when zero-message open is in effect.
    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        _req: &RequestInfo,
        _ino: u64,
        _fh: u64,
        _offset: u64,
        _data: &[u8],
        _write_flags: u32,
    ) -> impl Future<Output = Result<usize, Errno>> + Send {
        ready(Err(default_errno(Opcode::Write)))
    }

    /// Called on every `close(2)` of a descriptor. `ENOSYS` stops the kernel sending it.
    fn flush(
        &self,
        _req: &RequestInfo,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
    ) -> impl Future<Output = Result<(), Errno>> + Send {
        ready(Err(default_errno(Opcode::Flush)))
    }

    /// `ENOSYS` makes the kernel treat every later `fsync(2)` as a success.
    fn fsync(
        &self,
        _req: &RequestInfo,
        _ino: u64,
        _fh: u64,
        _datasync: bool,
    ) -> impl Future<Output = Result<(), Errno>> + Send {
        ready(Err(default_errno(Opcode::Fsync)))
    }

    fn fsyncdir(
        &self,
        _req: &RequestInfo,
        _ino: u64,
        _fh: u64,
        _datasync: bool,
    ) -> impl Future<Output = Result<(), Errno>> + Send {
        ready(Err(default_errno(Opcode::Fsyncdir)))
    }

    #[allow(clippy::too_many_arguments)]
    fn fallocate(
        &self,
        _req: &RequestInfo,
        _ino: u64,
        _fh: u64,
        _offset: u64,
        _length: u64,
        _mode: u32,
    ) -> impl Future<Output = Result<(), Errno>> + Send {
        ready(Err(default_errno(Opcode::Fallocate)))
    }

    /// CREATE answered without a remote fetch; `WouldFetch` before any change. Defaults to
    /// [`Self::create`].
    #[allow(clippy::too_many_arguments)]
    fn create_local(
        &self,
        req: &RequestInfo,
        parent: u64,
        name: &[u8],
        mode: u32,
        umask: u32,
        flags: u32,
    ) -> impl Future<Output = Result<CreateReply, ReadFault>> + Send {
        async move { Ok(self.create(req, parent, name, mode, umask, flags).await?) }
    }

    /// MKNOD answered without a remote fetch, as [`Self::create_local`] is.
    #[allow(clippy::too_many_arguments)]
    fn mknod_local(
        &self,
        req: &RequestInfo,
        parent: u64,
        name: &[u8],
        mode: u32,
        rdev: u32,
        umask: u32,
    ) -> impl Future<Output = Result<EntryReply, ReadFault>> + Send {
        async move { Ok(self.mknod(req, parent, name, mode, rdev, umask).await?) }
    }

    /// MKDIR answered without a remote fetch, as [`Self::create_local`] is.
    fn mkdir_local(
        &self,
        req: &RequestInfo,
        parent: u64,
        name: &[u8],
        mode: u32,
        umask: u32,
    ) -> impl Future<Output = Result<EntryReply, ReadFault>> + Send {
        async move { Ok(self.mkdir(req, parent, name, mode, umask).await?) }
    }

    /// SYMLINK answered without a remote fetch, as [`Self::create_local`] is.
    fn symlink_local(
        &self,
        req: &RequestInfo,
        parent: u64,
        name: &[u8],
        target: &[u8],
    ) -> impl Future<Output = Result<EntryReply, ReadFault>> + Send {
        async move { Ok(self.symlink(req, parent, name, target).await?) }
    }

    /// UNLINK answered without a remote fetch, as [`Self::create_local`] is.
    fn unlink_local(
        &self,
        req: &RequestInfo,
        parent: u64,
        name: &[u8],
    ) -> impl Future<Output = Result<(), ReadFault>> + Send {
        async move { Ok(self.unlink(req, parent, name).await?) }
    }

    /// RMDIR answered without a remote fetch, as [`Self::create_local`] is.
    fn rmdir_local(
        &self,
        req: &RequestInfo,
        parent: u64,
        name: &[u8],
    ) -> impl Future<Output = Result<(), ReadFault>> + Send {
        async move { Ok(self.rmdir(req, parent, name).await?) }
    }

    /// RENAME and RENAME2 answered without a remote fetch, as [`Self::create_local`] is.
    #[allow(clippy::too_many_arguments)]
    fn rename_local(
        &self,
        req: &RequestInfo,
        parent: u64,
        name: &[u8],
        newparent: u64,
        newname: &[u8],
        flags: u32,
    ) -> impl Future<Output = Result<(), ReadFault>> + Send {
        async move {
            Ok(self
                .rename(req, parent, name, newparent, newname, flags)
                .await?)
        }
    }

    /// WRITE answered without a remote fetch, as [`Self::create_local`] is: a first write to a
    /// file whose content lives in the store answers `WouldFetch`.
    #[allow(clippy::too_many_arguments)]
    fn write_local(
        &self,
        req: &RequestInfo,
        ino: u64,
        fh: u64,
        offset: u64,
        data: &[u8],
        write_flags: u32,
    ) -> impl Future<Output = Result<usize, ReadFault>> + Send {
        async move { Ok(self.write(req, ino, fh, offset, data, write_flags).await?) }
    }

    /// SETATTR answered without a remote fetch, as [`Self::create_local`] is: one that copies up
    /// a file whose content lives in the store answers `WouldFetch`.
    fn setattr_local(
        &self,
        req: &RequestInfo,
        ino: u64,
        attr: &SetattrIn,
    ) -> impl Future<Output = Result<AttrReply, ReadFault>> + Send {
        async move { Ok(self.setattr(req, ino, attr).await?) }
    }

    /// FALLOCATE answered without a remote fetch, as [`Self::setattr_local`] is.
    #[allow(clippy::too_many_arguments)]
    fn fallocate_local(
        &self,
        req: &RequestInfo,
        ino: u64,
        fh: u64,
        offset: u64,
        length: u64,
        mode: u32,
    ) -> impl Future<Output = Result<(), ReadFault>> + Send {
        async move { Ok(self.fallocate(req, ino, fh, offset, length, mode).await?) }
    }

    /// FSYNC answered on the worker; `WouldFetch` sends its flush off the worker
    /// ([`Fetch::Flush`]).
    fn fsync_local(
        &self,
        req: &RequestInfo,
        ino: u64,
        fh: u64,
        datasync: bool,
    ) -> impl Future<Output = Result<(), ReadFault>> + Send {
        async move { Ok(self.fsync(req, ino, fh, datasync).await?) }
    }

    /// Fetches what a request whose local form answered `WouldFetch` reads, changing nothing
    /// the request changes; for [`Fetch::Flush`], flushes. Defaults to nothing.
    fn fetch(
        &self,
        _req: &RequestInfo,
        _what: Fetch<'_>,
    ) -> impl Future<Output = Result<(), Errno>> + Send {
        ready(Ok(()))
    }
}

/// The reply for every opcode the dispatcher routes to no `Filesystem` method, and what the
/// write-family methods' default bodies return (spec: "Opcodes" table). Opcodes the dispatcher
/// routes to required methods (`LOOKUP`, `GETATTR`, ...) never reach this function; their
/// fallback arm here is unreachable in practice but still total, so the match compiles without
/// a wildcard swallowing a typo.
// The wildcard arm intentionally returns the same `Errno::ENOSYS` as the explicit xattr/flush/
// etc. arm above it: the two are semantically distinct (one lists opcodes this table legislates
// for, the other is the exhaustiveness backstop for opcodes routed inline elsewhere), they just
// happen to agree on the value. Merging them would lose the "compiles without a wildcard
// swallowing a typo" property the doc comment above describes.
#[allow(clippy::match_same_arms)]
pub fn default_errno(opcode: Opcode) -> Errno {
    match opcode {
        // Every modifying opcode the trait covers. On a read-only mount the kernel refuses
        // writes first, so this is a defensive fallback there.
        Opcode::Setattr
        | Opcode::Symlink
        | Opcode::Mknod
        | Opcode::Mkdir
        | Opcode::Unlink
        | Opcode::Rmdir
        | Opcode::Rename
        | Opcode::Link
        | Opcode::Write
        | Opcode::Create
        | Opcode::Fallocate
        | Opcode::Rename2
        | Opcode::Setupmapping
        | Opcode::Removemapping => Errno::EROFS,

        // Modifying opcodes no provider implements. `ENOSYS` makes the kernel fall back to a
        // splice copy for `COPY_FILE_RANGE` and answer `EOPNOTSUPP` for the others; a
        // read-only mount refuses them in the VFS first.
        Opcode::Setxattr
        | Opcode::Removexattr
        | Opcode::CopyFileRange
        | Opcode::Tmpfile
        | Opcode::Getxattr
        | Opcode::Listxattr
        | Opcode::Flush
        | Opcode::Fsync
        | Opcode::Fsyncdir
        | Opcode::Lseek
        | Opcode::Poll
        | Opcode::Bmap
        | Opcode::Statx
        | Opcode::Access
        | Opcode::Getlk
        | Opcode::Setlk
        | Opcode::Setlkw
        | Opcode::Syncfs => Errno::ENOSYS,

        Opcode::Readlink => Errno::EINVAL,
        Opcode::Ioctl => Errno::ENOTTY,

        // Opcodes the dispatcher routes inline, never through this table.
        _ => Errno::ENOSYS,
    }
}
