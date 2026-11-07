// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_fuse::abi::Opcode;
use lore_fuse::filesystem::*;
use lore_fuse::reply::Errno;
use lore_fuse::reply::Timeouts;

#[test]
fn write_family_defaults_to_erofs() {
    for opcode in [
        Opcode::Setattr,
        Opcode::Symlink,
        Opcode::Mknod,
        Opcode::Mkdir,
        Opcode::Unlink,
        Opcode::Rmdir,
        Opcode::Rename,
        Opcode::Link,
        Opcode::Write,
        Opcode::Create,
        Opcode::Fallocate,
        Opcode::Rename2,
        Opcode::Setupmapping,
        Opcode::Removemapping,
    ] {
        assert_eq!(default_errno(opcode), Errno::EROFS, "{opcode:?}");
    }
}

/// The kernel falls back to a copy or answers `EOPNOTSUPP` only after `ENOSYS`; `EROFS`
/// would fail `cp`, xattr copies and `O_TMPFILE` on a read-write mount.
#[test]
fn xattr_writes_copy_file_range_and_tmpfile_default_to_enosys() {
    for opcode in [
        Opcode::Setxattr,
        Opcode::Removexattr,
        Opcode::CopyFileRange,
        Opcode::Tmpfile,
    ] {
        assert_eq!(default_errno(opcode), Errno::ENOSYS, "{opcode:?}");
    }
}

#[test]
fn xattr_flush_fsync_lseek_poll_bmap_statx_access_default_to_enosys() {
    for opcode in [
        Opcode::Getxattr,
        Opcode::Listxattr,
        Opcode::Flush,
        Opcode::Fsync,
        Opcode::Fsyncdir,
        Opcode::Lseek,
        Opcode::Poll,
        Opcode::Bmap,
        Opcode::Statx,
        Opcode::Access,
    ] {
        assert_eq!(default_errno(opcode), Errno::ENOSYS, "{opcode:?}");
    }
}

#[test]
fn readlink_defaults_to_einval() {
    assert_eq!(default_errno(Opcode::Readlink), Errno::EINVAL);
}

#[test]
fn ioctl_defaults_to_enotty() {
    assert_eq!(default_errno(Opcode::Ioctl), Errno::ENOTTY);
}

#[test]
fn timeouts_zero_encodes_zero_seconds_and_nsec() {
    assert_eq!(Timeouts::ZERO.entry.as_secs(), 0);
    assert_eq!(Timeouts::ZERO.entry.subsec_nanos(), 0);
    assert_eq!(Timeouts::ZERO.attr.as_secs(), 0);
    assert_eq!(Timeouts::ZERO.attr.subsec_nanos(), 0);
}
