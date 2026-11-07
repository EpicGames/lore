// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::mem::offset_of;
use std::mem::size_of;

use lore_fuse::abi::file::*;

#[test]
fn open_in_is_8() {
    assert_eq!(size_of::<OpenIn>(), 8);
}

#[test]
fn open_out_is_16() {
    assert_eq!(size_of::<OpenOut>(), 16);
}

#[test]
fn release_in_is_24() {
    assert_eq!(size_of::<ReleaseIn>(), 24);
}

#[test]
fn read_in_is_40() {
    assert_eq!(size_of::<ReadIn>(), 40);
}

#[test]
fn write_in_is_40_with_kernel_field_offsets() {
    assert_eq!(size_of::<WriteIn>(), 40);
    assert_eq!(offset_of!(WriteIn, offset), 8);
    assert_eq!(offset_of!(WriteIn, size), 16);
    assert_eq!(offset_of!(WriteIn, write_flags), 20);
    assert_eq!(offset_of!(WriteIn, lock_owner), 24);
    assert_eq!(offset_of!(WriteIn, flags), 32);
}

#[test]
fn write_out_is_8() {
    assert_eq!(size_of::<WriteOut>(), 8);
}

#[test]
fn create_in_is_16_with_kernel_field_offsets() {
    assert_eq!(size_of::<CreateIn>(), 16);
    assert_eq!(offset_of!(CreateIn, mode), 4);
    assert_eq!(offset_of!(CreateIn, umask), 8);
    assert_eq!(offset_of!(CreateIn, open_flags), 12);
}

#[test]
fn create_out_is_entry_out_then_open_out() {
    assert_eq!(size_of::<CreateOut>(), 144);
    assert_eq!(offset_of!(CreateOut, open), 128);
}

#[test]
fn flush_in_is_24_with_lock_owner_at_16() {
    assert_eq!(size_of::<FlushIn>(), 24);
    assert_eq!(offset_of!(FlushIn, lock_owner), 16);
}

#[test]
fn fsync_in_is_16_with_flags_at_8() {
    assert_eq!(size_of::<FsyncIn>(), 16);
    assert_eq!(offset_of!(FsyncIn, fsync_flags), 8);
}

#[test]
fn fallocate_in_is_32_with_kernel_field_offsets() {
    assert_eq!(size_of::<FallocateIn>(), 32);
    assert_eq!(offset_of!(FallocateIn, offset), 8);
    assert_eq!(offset_of!(FallocateIn, length), 16);
    assert_eq!(offset_of!(FallocateIn, mode), 24);
}

#[test]
fn write_and_fsync_flag_bits_match_fuse_h() {
    assert_eq!(write_flags::CACHE, 1);
    assert_eq!(write_flags::LOCKOWNER, 2);
    assert_eq!(write_flags::KILL_SUIDGID, 4);
    assert_eq!(fsync_flags::FDATASYNC, 1);
}
