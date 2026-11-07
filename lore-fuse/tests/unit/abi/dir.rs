// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_fuse::abi::dir::*;

#[test]
fn dirent_header_is_24() {
    assert_eq!(size_of::<Dirent>(), 24);
}

#[test]
fn direntplus_header_is_152() {
    assert_eq!(size_of::<DirentPlus>(), 152);
}

#[test]
fn dirent_size_pads_to_8() {
    // 24-byte header + 1-byte name = 25, padded up to 32.
    assert_eq!(dirent_size(1), 32);
    // Already a multiple of 8: no padding added.
    assert_eq!(dirent_size(8), 32);
}
