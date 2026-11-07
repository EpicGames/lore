// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `fuse_dirent` / `fuse_direntplus` headers and the record-alignment math, from
//! `/usr/include/linux/fuse.h`. The kernel's `name[]` flexible array member is written
//! directly into the caller's buffer by [`crate::codec::dirent::DirentWriter`], not modeled
//! as a Rust field.

use std::mem::size_of;

use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

use super::EntryOut;

/// Fixed header of one `READDIR` record, followed by `namelen` bytes of name (unpadded).
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct Dirent {
    pub ino: u64,
    pub off: u64,
    pub namelen: u32,
    pub type_: u32,
}

/// Fixed header of one `READDIRPLUS` record, followed by `namelen` bytes of name (unpadded).
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct DirentPlus {
    pub entry_out: EntryOut,
    pub dirent: Dirent,
}

/// Kernel requires each directory record to be aligned to a 64-bit boundary.
pub const DIRENT_ALIGN: usize = 8;

fn align_up(value: usize) -> usize {
    (value + DIRENT_ALIGN - 1) & !(DIRENT_ALIGN - 1)
}

/// Total padded record size for a `READDIR` entry with a name of `namelen` bytes.
pub fn dirent_size(namelen: usize) -> usize {
    align_up(size_of::<Dirent>() + namelen)
}

/// Total padded record size for a `READDIRPLUS` entry with a name of `namelen` bytes.
pub fn direntplus_size(namelen: usize) -> usize {
    align_up(size_of::<DirentPlus>() + namelen)
}

/// `d_type`-style file kind, from the low bits of `fuse_dirent.type`.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirentType {
    Unknown = 0,
    Dir = 4,
    Reg = 8,
    Lnk = 10,
}
