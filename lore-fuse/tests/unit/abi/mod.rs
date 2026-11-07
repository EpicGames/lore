// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

mod dir;
mod entry;
mod file;
mod notify;

use std::mem::size_of;

use lore_fuse::abi::*;

#[test]
fn in_header_is_40_bytes() {
    assert_eq!(size_of::<InHeader>(), 40);
}

#[test]
fn out_header_is_16_bytes() {
    assert_eq!(size_of::<OutHeader>(), 16);
}

#[test]
fn init_in_is_64_bytes() {
    assert_eq!(size_of::<InitIn>(), 64);
}

#[test]
fn init_out_is_64_bytes() {
    assert_eq!(size_of::<InitOut>(), 64);
}

#[test]
fn every_supported_opcode_roundtrips_u32() {
    const ALL: &[Opcode] = &[
        Opcode::Lookup,
        Opcode::Forget,
        Opcode::Getattr,
        Opcode::Setattr,
        Opcode::Readlink,
        Opcode::Symlink,
        Opcode::Mknod,
        Opcode::Mkdir,
        Opcode::Unlink,
        Opcode::Rmdir,
        Opcode::Rename,
        Opcode::Link,
        Opcode::Open,
        Opcode::Read,
        Opcode::Write,
        Opcode::Statfs,
        Opcode::Release,
        Opcode::Fsync,
        Opcode::Setxattr,
        Opcode::Getxattr,
        Opcode::Listxattr,
        Opcode::Removexattr,
        Opcode::Flush,
        Opcode::Init,
        Opcode::Opendir,
        Opcode::Readdir,
        Opcode::Releasedir,
        Opcode::Fsyncdir,
        Opcode::Getlk,
        Opcode::Setlk,
        Opcode::Setlkw,
        Opcode::Access,
        Opcode::Create,
        Opcode::Interrupt,
        Opcode::Bmap,
        Opcode::Destroy,
        Opcode::Ioctl,
        Opcode::Poll,
        Opcode::NotifyReply,
        Opcode::BatchForget,
        Opcode::Fallocate,
        Opcode::Readdirplus,
        Opcode::Rename2,
        Opcode::Lseek,
        Opcode::CopyFileRange,
        Opcode::Setupmapping,
        Opcode::Removemapping,
        Opcode::Syncfs,
        Opcode::Tmpfile,
        Opcode::Statx,
    ];

    for opcode in ALL {
        let raw = *opcode as u32;
        assert_eq!(Opcode::try_from(raw), Ok(*opcode));
    }
}

#[test]
fn writeback_cache_flag_is_bit_16_and_not_requested() {
    assert_eq!(init_flags::WRITEBACK_CACHE, 1 << 16);
    assert_eq!(
        lore_fuse::init::REQUIRED_FLAGS & init_flags::WRITEBACK_CACHE,
        0
    );
    assert_eq!(
        lore_fuse::init::WANTED_FLAGS & init_flags::WRITEBACK_CACHE,
        0
    );
}

#[test]
fn unknown_opcode_is_error() {
    assert_eq!(Opcode::try_from(9_999), Err(9_999));
    assert_eq!(Opcode::try_from(7), Err(7));
    assert_eq!(Opcode::try_from(19), Err(19));
}
