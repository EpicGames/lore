// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! File-handle requests and replies (open, create, read, write, release, flush, fsync,
//! fallocate) and their flags, from `/usr/include/linux/fuse.h`.

use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

use super::EntryOut;

/// Body of an `OPEN`/`OPENDIR` request.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct OpenIn {
    pub flags: u32,
    pub open_flags: u32,
}

/// Reply to `OPEN`/`OPENDIR`.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct OpenOut {
    pub fh: u64,
    pub open_flags: u32,
    pub backing_id: i32,
}

/// Body of a `RELEASE`/`RELEASEDIR` request.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct ReleaseIn {
    pub fh: u64,
    pub flags: u32,
    pub release_flags: u32,
    pub lock_owner: u64,
}

/// Body of a `READ`/`READDIR`/`READDIRPLUS` request.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct ReadIn {
    pub fh: u64,
    pub offset: u64,
    pub size: u32,
    pub read_flags: u32,
    pub lock_owner: u64,
    pub flags: u32,
    pub padding: u32,
}

/// Body of a `CREATE` request, followed by the NUL-terminated name.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct CreateIn {
    pub flags: u32,
    pub mode: u32,
    pub umask: u32,
    pub open_flags: u32,
}

/// Reply to `CREATE`: the new entry, then the open handle.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct CreateOut {
    pub entry: EntryOut,
    pub open: OpenOut,
}

/// Body of a `WRITE` request, followed by `size` bytes of data.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct WriteIn {
    pub fh: u64,
    pub offset: u64,
    pub size: u32,
    pub write_flags: u32,
    pub lock_owner: u64,
    pub flags: u32,
    pub padding: u32,
}

/// Reply to `WRITE`.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct WriteOut {
    pub size: u32,
    pub padding: u32,
}

/// Body of a `FLUSH` request.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct FlushIn {
    pub fh: u64,
    pub unused: u32,
    pub padding: u32,
    pub lock_owner: u64,
}

/// Body of a `FSYNC`/`FSYNCDIR` request.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct FsyncIn {
    pub fh: u64,
    pub fsync_flags: u32,
    pub padding: u32,
}

/// Body of a `FALLOCATE` request.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct FallocateIn {
    pub fh: u64,
    pub offset: u64,
    pub length: u64,
    pub mode: u32,
    pub padding: u32,
}

/// `FOPEN_*` reply flags, set in [`OpenOut::open_flags`].
pub mod open_flags {
    pub const DIRECT_IO: u32 = 1 << 0;
    pub const KEEP_CACHE: u32 = 1 << 1;
    pub const CACHE_DIR: u32 = 1 << 3;
    pub const NOFLUSH: u32 = 1 << 5;
}

/// `FUSE_WRITE_*` flags, set in [`WriteIn::write_flags`].
pub mod write_flags {
    /// A delayed write from the page cache; the handle is a guess.
    pub const CACHE: u32 = 1 << 0;
    /// [`super::WriteIn::lock_owner`] is valid.
    pub const LOCKOWNER: u32 = 1 << 1;
    /// The write must clear the set-user-ID and set-group-ID bits.
    pub const KILL_SUIDGID: u32 = 1 << 2;
}

/// `FUSE_FSYNC_*` flags, set in [`FsyncIn::fsync_flags`].
pub mod fsync_flags {
    /// Only data needs to reach stable storage, as `fdatasync(2)`.
    pub const FDATASYNC: u32 = 1 << 0;
}
