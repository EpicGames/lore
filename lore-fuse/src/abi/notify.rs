// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `fuse_interrupt_in`, invalidation notify payloads, and `enum fuse_notify_code`, from
//! `/usr/include/linux/fuse.h`.

use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Body of an `INTERRUPT` request.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct InterruptIn {
    pub unique: u64,
}

/// Payload of an unsolicited `FUSE_NOTIFY_INVAL_INODE` message.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct NotifyInvalInodeOut {
    pub ino: u64,
    pub off: i64,
    pub len: i64,
}

/// Fixed header of an unsolicited `FUSE_NOTIFY_INVAL_ENTRY` message, followed by the name.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, Default)]
pub struct NotifyInvalEntryOut {
    pub parent: u64,
    pub namelen: u32,
    pub flags: u32,
}

/// Set in [`NotifyInvalEntryOut::flags`] when the entry is only known to have expired, not to
/// be gone.
pub const EXPIRE_ONLY: u32 = 1 << 0;

/// Opcode carried in the reply header of an unsolicited notification.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyCode {
    InvalInode = 2,
    InvalEntry = 3,
}
