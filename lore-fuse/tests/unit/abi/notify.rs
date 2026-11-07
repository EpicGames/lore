// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::mem::size_of;

use lore_fuse::abi::notify::*;

#[test]
fn interrupt_in_is_8() {
    assert_eq!(size_of::<InterruptIn>(), 8);
}

#[test]
fn inval_inode_out_is_24() {
    assert_eq!(size_of::<NotifyInvalInodeOut>(), 24);
}

#[test]
fn inval_entry_out_is_16() {
    assert_eq!(size_of::<NotifyInvalEntryOut>(), 16);
}
