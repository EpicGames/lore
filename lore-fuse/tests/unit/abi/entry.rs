// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::mem::offset_of;
use std::mem::size_of;

use lore_fuse::abi::entry::*;

#[test]
fn setattr_in_is_88_with_kernel_field_offsets() {
    assert_eq!(size_of::<SetattrIn>(), 88);
    assert_eq!(offset_of!(SetattrIn, fh), 8);
    assert_eq!(offset_of!(SetattrIn, size), 16);
    assert_eq!(offset_of!(SetattrIn, lock_owner), 24);
    assert_eq!(offset_of!(SetattrIn, atime), 32);
    assert_eq!(offset_of!(SetattrIn, mtime), 40);
    assert_eq!(offset_of!(SetattrIn, ctime), 48);
    assert_eq!(offset_of!(SetattrIn, atimensec), 56);
    assert_eq!(offset_of!(SetattrIn, mtimensec), 60);
    assert_eq!(offset_of!(SetattrIn, ctimensec), 64);
    assert_eq!(offset_of!(SetattrIn, mode), 68);
    assert_eq!(offset_of!(SetattrIn, uid), 76);
    assert_eq!(offset_of!(SetattrIn, gid), 80);
}

#[test]
fn setattr_valid_bits_match_fuse_h() {
    assert_eq!(setattr_valid::MODE, 1 << 0);
    assert_eq!(setattr_valid::UID, 1 << 1);
    assert_eq!(setattr_valid::GID, 1 << 2);
    assert_eq!(setattr_valid::SIZE, 1 << 3);
    assert_eq!(setattr_valid::ATIME, 1 << 4);
    assert_eq!(setattr_valid::MTIME, 1 << 5);
    assert_eq!(setattr_valid::FH, 1 << 6);
    assert_eq!(setattr_valid::ATIME_NOW, 1 << 7);
    assert_eq!(setattr_valid::MTIME_NOW, 1 << 8);
    assert_eq!(setattr_valid::LOCKOWNER, 1 << 9);
    assert_eq!(setattr_valid::CTIME, 1 << 10);
    assert_eq!(setattr_valid::KILL_SUIDGID, 1 << 11);
}

#[test]
fn mknod_in_is_16_with_kernel_field_offsets() {
    assert_eq!(size_of::<MknodIn>(), 16);
    assert_eq!(offset_of!(MknodIn, rdev), 4);
    assert_eq!(offset_of!(MknodIn, umask), 8);
}

#[test]
fn mkdir_in_is_8() {
    assert_eq!(size_of::<MkdirIn>(), 8);
    assert_eq!(offset_of!(MkdirIn, umask), 4);
}

#[test]
fn rename_in_is_8() {
    assert_eq!(size_of::<RenameIn>(), 8);
}

#[test]
fn rename2_in_is_16_with_flags_at_8() {
    assert_eq!(size_of::<Rename2In>(), 16);
    assert_eq!(offset_of!(Rename2In, flags), 8);
}

#[test]
fn link_in_is_8() {
    assert_eq!(size_of::<LinkIn>(), 8);
}

#[test]
fn attr_is_88() {
    assert_eq!(size_of::<Attr>(), 88);
}

#[test]
fn kstatfs_is_80() {
    assert_eq!(size_of::<Kstatfs>(), 80);
}

#[test]
fn entry_out_is_128() {
    assert_eq!(size_of::<EntryOut>(), 128);
}

#[test]
fn forget_in_is_8() {
    assert_eq!(size_of::<ForgetIn>(), 8);
}

#[test]
fn forget_one_is_16() {
    assert_eq!(size_of::<ForgetOne>(), 16);
}

#[test]
fn batch_forget_in_is_8() {
    assert_eq!(size_of::<BatchForgetIn>(), 8);
}

#[test]
fn getattr_in_is_16() {
    assert_eq!(size_of::<GetattrIn>(), 16);
}

#[test]
fn attr_out_is_104() {
    assert_eq!(size_of::<AttrOut>(), 104);
}

#[test]
fn statfs_out_is_80() {
    assert_eq!(size_of::<StatfsOut>(), 80);
}
