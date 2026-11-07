// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Encodes replies as `writev`-ready [`IoSlice`]s, borrowing from the caller's buffers rather
//! than copying into a fresh allocation.

use std::io::IoSlice;
use std::mem::size_of;

use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

use crate::abi::CreateOut;
use crate::abi::OutHeader;

/// Largest reply body [`encode_struct`] will inline into a [`ReplyHeaderBuf`]: the `CREATE`
/// reply, the largest fixed-size reply.
pub const MAX_INLINE_BODY: usize = size_of::<CreateOut>();

/// A reusable, stack-sized scratch buffer for a header plus a small inline body.
///
/// Reused across replies by the caller (typically one per worker) so encoding a reply never
/// allocates.
#[lore_macro::test_pub]
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug)]
pub struct ReplyHeaderBuf {
    header: OutHeader,
    body: [u8; MAX_INLINE_BODY],
}

impl ReplyHeaderBuf {
    pub const fn new() -> Self {
        Self {
            header: OutHeader {
                len: 0,
                error: 0,
                unique: 0,
            },
            body: [0; MAX_INLINE_BODY],
        }
    }
}

impl Default for ReplyHeaderBuf {
    fn default() -> Self {
        Self::new()
    }
}

/// Encodes an error reply: header only, with the kernel's expected negative errno.
pub fn encode_error(unique: u64, errno: i32, out: &mut ReplyHeaderBuf) -> [IoSlice<'_>; 1] {
    out.header = OutHeader {
        len: size_of::<OutHeader>() as u32,
        error: -errno,
        unique,
    };
    [IoSlice::new(out.header.as_bytes())]
}

/// Encodes a reply header followed by `body`, inlined into `out`.
pub fn encode_struct<'a, T: IntoBytes + Immutable>(
    unique: u64,
    body: &T,
    out: &'a mut ReplyHeaderBuf,
) -> [IoSlice<'a>; 1] {
    let body_bytes = body.as_bytes();
    debug_assert!(
        body_bytes.len() <= MAX_INLINE_BODY,
        "reply body exceeds inline capacity"
    );

    let total = size_of::<OutHeader>() + body_bytes.len();
    out.header = OutHeader {
        len: total as u32,
        error: 0,
        unique,
    };
    out.body[..body_bytes.len()].copy_from_slice(body_bytes);

    let all = out.as_bytes();
    [IoSlice::new(&all[..total])]
}

/// Encodes a reply header followed by a `payload` borrowed straight from the caller, with no
/// copy: the second iovec points directly at `payload`.
pub fn encode_data<'a>(
    unique: u64,
    out: &'a mut ReplyHeaderBuf,
    payload: &'a [u8],
) -> [IoSlice<'a>; 2] {
    out.header = OutHeader {
        len: (size_of::<OutHeader>() + payload.len()) as u32,
        error: 0,
        unique,
    };
    [IoSlice::new(out.header.as_bytes()), IoSlice::new(payload)]
}

/// Encodes a bodyless success reply, e.g. for `FLUSH` or `FSYNC`.
pub fn encode_empty(unique: u64, out: &mut ReplyHeaderBuf) -> [IoSlice<'_>; 1] {
    out.header = OutHeader {
        len: size_of::<OutHeader>() as u32,
        error: 0,
        unique,
    };
    [IoSlice::new(out.header.as_bytes())]
}
