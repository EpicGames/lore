// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `fuse_attr`, `fuse_kstatfs`, the lookup/getattr/forget/statfs structs built from them, and
//! the namespace and attribute requests (setattr, mknod, mkdir, rename, link), from
//! `/usr/include/linux/fuse.h`.

use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// A node's stat-like attributes, as returned inline in several replies.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct Attr {
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub atimensec: u32,
    pub mtimensec: u32,
    pub ctimensec: u32,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u32,
    pub blksize: u32,
    pub flags: u32,
}

/// Reply to `LOOKUP`, `MKNOD`, `MKDIR`, `SYMLINK`, `LINK`, and `CREATE`.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct EntryOut {
    pub nodeid: u64,
    pub generation: u64,
    pub entry_valid: u64,
    pub attr_valid: u64,
    pub entry_valid_nsec: u32,
    pub attr_valid_nsec: u32,
    pub attr: Attr,
}

/// Reply to `GETATTR` and `SETATTR`.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct AttrOut {
    pub attr_valid: u64,
    pub attr_valid_nsec: u32,
    pub dummy: u32,
    pub attr: Attr,
}

/// Body of a `FORGET` request.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct ForgetIn {
    pub nlookup: u64,
}

/// One entry in a `BATCH_FORGET` request's trailing array.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct ForgetOne {
    pub nodeid: u64,
    pub nlookup: u64,
}

/// Fixed header of a `BATCH_FORGET` request, followed by `count` [`ForgetOne`] entries.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct BatchForgetIn {
    pub count: u32,
    pub dummy: u32,
}

/// Body of a `GETATTR` request.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct GetattrIn {
    pub getattr_flags: u32,
    pub dummy: u32,
    pub fh: u64,
}

/// `statvfs`-like filesystem statistics, from `fuse_kstatfs`.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct Kstatfs {
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub bsize: u32,
    pub namelen: u32,
    pub frsize: u32,
    pub padding: u32,
    pub spare: [u32; 6],
}

/// Reply to `STATFS`.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct StatfsOut {
    pub st: Kstatfs,
}

/// Body of a `SETATTR` request. [`SetattrIn::valid`] holds the [`setattr_valid`] bits naming
/// the fields to apply.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct SetattrIn {
    pub valid: u32,
    pub padding: u32,
    pub fh: u64,
    pub size: u64,
    pub lock_owner: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub atimensec: u32,
    pub mtimensec: u32,
    pub ctimensec: u32,
    pub mode: u32,
    pub unused4: u32,
    pub uid: u32,
    pub gid: u32,
    pub unused5: u32,
}

/// `FATTR_*` bits, set in [`SetattrIn::valid`].
pub mod setattr_valid {
    pub const MODE: u32 = 1 << 0;
    pub const UID: u32 = 1 << 1;
    pub const GID: u32 = 1 << 2;
    pub const SIZE: u32 = 1 << 3;
    pub const ATIME: u32 = 1 << 4;
    pub const MTIME: u32 = 1 << 5;
    pub const FH: u32 = 1 << 6;
    pub const ATIME_NOW: u32 = 1 << 7;
    pub const MTIME_NOW: u32 = 1 << 8;
    pub const LOCKOWNER: u32 = 1 << 9;
    pub const CTIME: u32 = 1 << 10;
    pub const KILL_SUIDGID: u32 = 1 << 11;
}

/// Body of a `MKNOD` request, followed by the NUL-terminated name.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct MknodIn {
    pub mode: u32,
    pub rdev: u32,
    pub umask: u32,
    pub padding: u32,
}

/// Body of a `MKDIR` request, followed by the NUL-terminated name.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct MkdirIn {
    pub mode: u32,
    pub umask: u32,
}

/// Body of a `RENAME` request, followed by the NUL-terminated old and new names.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct RenameIn {
    pub newdir: u64,
}

/// Body of a `RENAME2` request, followed by the NUL-terminated old and new names. `flags`
/// carries the `renameat2(2)` `RENAME_*` flags.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct Rename2In {
    pub newdir: u64,
    pub flags: u32,
    pub padding: u32,
}

/// Body of a `LINK` request, followed by the NUL-terminated new name. The header's node is
/// the new parent directory.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct LinkIn {
    pub oldnodeid: u64,
}
