// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Kernel FUSE ABI: wire structs and opcodes, mirroring `/usr/include/linux/fuse.h`.
//!
//! Every struct is `#[repr(C)]` and derives zerocopy's `FromBytes`/`IntoBytes`, so a request
//! is decoded by reinterpreting bytes borrowed from the request buffer, never by copying
//! fields out one at a time.

pub mod dir;
pub mod entry;
pub mod file;
pub mod header;
pub mod init;
pub mod notify;

pub use dir::Dirent;
pub use dir::DirentPlus;
pub use dir::DirentType;
pub use entry::Attr;
pub use entry::AttrOut;
pub use entry::BatchForgetIn;
pub use entry::EntryOut;
pub use entry::ForgetIn;
pub use entry::ForgetOne;
pub use entry::GetattrIn;
pub use entry::Kstatfs;
pub use entry::LinkIn;
pub use entry::MkdirIn;
pub use entry::MknodIn;
pub use entry::Rename2In;
pub use entry::RenameIn;
pub use entry::SetattrIn;
pub use entry::StatfsOut;
pub use file::CreateIn;
pub use file::CreateOut;
pub use file::FallocateIn;
pub use file::FlushIn;
pub use file::FsyncIn;
pub use file::OpenIn;
pub use file::OpenOut;
pub use file::ReadIn;
pub use file::ReleaseIn;
pub use file::WriteIn;
pub use file::WriteOut;
pub use header::InHeader;
pub use header::OutHeader;
pub use init::InitIn;
pub use init::InitOut;
pub use notify::InterruptIn;
pub use notify::NotifyCode;
pub use notify::NotifyInvalEntryOut;
pub use notify::NotifyInvalInodeOut;

/// The kernel's fixed inode number for the mount root.
pub const ROOT_ID: u64 = 1;
/// `FUSE_MIN_READ_BUFFER`: the smallest buffer the kernel is allowed to write a request into.
pub const MIN_READ_BUFFER: usize = 8192;
/// The FUSE protocol major version this crate speaks.
pub const KERNEL_MAJOR: u32 = 7;
/// The minimum protocol minor version this crate requires (Linux 6.8).
pub const REQUIRED_MINOR: u32 = 39;

/// Request opcodes this crate knows about, from `enum fuse_opcode`.
///
/// Values are not contiguous: the kernel enum reserves 7 and 19. [`OPCODE_COUNT`] is sized to
/// the highest opcode plus one, for use as a dense per-opcode array index, not as the number of
/// variants below.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    Lookup = 1,
    Forget = 2,
    Getattr = 3,
    Setattr = 4,
    Readlink = 5,
    Symlink = 6,
    Mknod = 8,
    Mkdir = 9,
    Unlink = 10,
    Rmdir = 11,
    Rename = 12,
    Link = 13,
    Open = 14,
    Read = 15,
    Write = 16,
    Statfs = 17,
    Release = 18,
    Fsync = 20,
    Setxattr = 21,
    Getxattr = 22,
    Listxattr = 23,
    Removexattr = 24,
    Flush = 25,
    Init = 26,
    Opendir = 27,
    Readdir = 28,
    Releasedir = 29,
    Fsyncdir = 30,
    Getlk = 31,
    Setlk = 32,
    Setlkw = 33,
    Access = 34,
    Create = 35,
    Interrupt = 36,
    Bmap = 37,
    Destroy = 38,
    Ioctl = 39,
    Poll = 40,
    NotifyReply = 41,
    BatchForget = 42,
    Fallocate = 43,
    Readdirplus = 44,
    Rename2 = 45,
    Lseek = 46,
    CopyFileRange = 47,
    Setupmapping = 48,
    Removemapping = 49,
    Syncfs = 50,
    Tmpfile = 51,
    Statx = 52,
}

/// One past the highest opcode value, for sizing a dense per-opcode array.
pub const OPCODE_COUNT: usize = 53;

impl TryFrom<u32> for Opcode {
    type Error = u32;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Ok(match value {
            1 => Opcode::Lookup,
            2 => Opcode::Forget,
            3 => Opcode::Getattr,
            4 => Opcode::Setattr,
            5 => Opcode::Readlink,
            6 => Opcode::Symlink,
            8 => Opcode::Mknod,
            9 => Opcode::Mkdir,
            10 => Opcode::Unlink,
            11 => Opcode::Rmdir,
            12 => Opcode::Rename,
            13 => Opcode::Link,
            14 => Opcode::Open,
            15 => Opcode::Read,
            16 => Opcode::Write,
            17 => Opcode::Statfs,
            18 => Opcode::Release,
            20 => Opcode::Fsync,
            21 => Opcode::Setxattr,
            22 => Opcode::Getxattr,
            23 => Opcode::Listxattr,
            24 => Opcode::Removexattr,
            25 => Opcode::Flush,
            26 => Opcode::Init,
            27 => Opcode::Opendir,
            28 => Opcode::Readdir,
            29 => Opcode::Releasedir,
            30 => Opcode::Fsyncdir,
            31 => Opcode::Getlk,
            32 => Opcode::Setlk,
            33 => Opcode::Setlkw,
            34 => Opcode::Access,
            35 => Opcode::Create,
            36 => Opcode::Interrupt,
            37 => Opcode::Bmap,
            38 => Opcode::Destroy,
            39 => Opcode::Ioctl,
            40 => Opcode::Poll,
            41 => Opcode::NotifyReply,
            42 => Opcode::BatchForget,
            43 => Opcode::Fallocate,
            44 => Opcode::Readdirplus,
            45 => Opcode::Rename2,
            46 => Opcode::Lseek,
            47 => Opcode::CopyFileRange,
            48 => Opcode::Setupmapping,
            49 => Opcode::Removemapping,
            50 => Opcode::Syncfs,
            51 => Opcode::Tmpfile,
            52 => Opcode::Statx,
            other => return Err(other),
        })
    }
}

/// INIT negotiation flags, from the low and high halves of the kernel's 64-bit flag space
/// (`flags` carries bits 0..=31, `flags2` carries bits 32..=63).
pub mod init_flags {
    pub const ASYNC_READ: u64 = 1 << 0;
    /// Without it the kernel sends buffered writes one page per `WRITE`, whatever `max_write`.
    pub const BIG_WRITES: u64 = 1 << 5;
    pub const DO_READDIRPLUS: u64 = 1 << 13;
    pub const READDIRPLUS_AUTO: u64 = 1 << 14;
    /// Buffered writes go through the kernel's page cache, which then owns file size and
    /// modification time. Never requested.
    pub const WRITEBACK_CACHE: u64 = 1 << 16;
    pub const NO_OPEN_SUPPORT: u64 = 1 << 17;
    pub const PARALLEL_DIROPS: u64 = 1 << 18;
    pub const MAX_PAGES: u64 = 1 << 22;
    pub const EXPLICIT_INVAL_DATA: u64 = 1 << 25;
    pub const INIT_EXT: u64 = 1 << 30;
    pub const HAS_EXPIRE_ONLY: u64 = 1 << 35;
    pub const NO_EXPORT_SUPPORT: u64 = 1 << 38;

    /// Combines `fuse_init_{in,out}.flags` and `.flags2` into one 64-bit flag space.
    pub fn combine(flags: u32, flags2: u32) -> u64 {
        (flags as u64) | ((flags2 as u64) << 32)
    }

    /// Splits a 64-bit flag value back into the wire's `flags` and `flags2` fields.
    pub fn split(flags: u64) -> (u32, u32) {
        (flags as u32, (flags >> 32) as u32)
    }
}
