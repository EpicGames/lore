// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Read/reply buffers.
//!
//! Backed by `Box<[u64]>` rather than `Box<[u8]>`: the kernel ABI's zerocopy structs
//! (`InHeader`, `InitIn`, ...) are read via `FromBytes::ref_from_prefix`, which rejects a
//! misaligned slice. A `u64`-backed allocation guarantees 8-byte alignment for the `&mut [u8]`
//! view handed to the decoder, with no extra copy.

use zerocopy::IntoBytes;

use crate::abi::InHeader;
use crate::abi::WriteIn;

/// Size of a single read-request buffer: large enough for `abi::MIN_READ_BUFFER` plus the
/// largest inline request body the kernel can send, `WRITE` payloads over 4 KiB excepted.
pub const REQUEST_BUFFER_BYTES: usize = 16 * 1024;

const WORD_BYTES: usize = size_of::<u64>();

fn aligned_bytes(len_bytes: usize) -> Box<[u64]> {
    vec![0u64; len_bytes.div_ceil(WORD_BYTES)].into_boxed_slice()
}

/// A single worker's read-request buffer.
pub struct RequestBuffer {
    data: Box<[u64]>,
}

impl RequestBuffer {
    pub fn new() -> Self {
        Self {
            data: aligned_bytes(REQUEST_BUFFER_BYTES),
        }
    }

    /// A buffer that holds a `WRITE` of `max_write` bytes, and never less than
    /// [`REQUEST_BUFFER_BYTES`]. The kernel refuses to read any request into a buffer smaller
    /// than a full `WRITE`.
    pub fn for_max_write(max_write: u32) -> Self {
        let full_write = size_of::<InHeader>() + size_of::<WriteIn>() + max_write as usize;
        Self {
            data: aligned_bytes(full_write.max(REQUEST_BUFFER_BYTES)),
        }
    }

    /// The buffer as bytes, for handing to `receive`/`decode`. Always 8-byte aligned.
    ///
    /// Named to match the plan's interface, not `std::convert::AsMut` (there is no `AsMut<[u8]>`
    /// impl here).
    #[allow(clippy::should_implement_trait)]
    pub fn as_mut(&mut self) -> &mut [u8] {
        self.data.as_mut_bytes()
    }
}

impl Default for RequestBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// A single worker's reply buffer, sized from the negotiated `max_pages` so the largest
/// READ/READDIRPLUS reply always fits without a second allocation.
pub struct ReplyBuffer {
    data: Box<[u64]>,
}

impl ReplyBuffer {
    pub fn new(max_pages: u16) -> Self {
        Self {
            data: aligned_bytes(max_pages as usize * 4096),
        }
    }

    /// The buffer as bytes. Always 8-byte aligned.
    ///
    /// Named to match the plan's interface, not `std::convert::AsMut` (there is no `AsMut<[u8]>`
    /// impl here).
    #[allow(clippy::should_implement_trait)]
    pub fn as_mut(&mut self) -> &mut [u8] {
        self.data.as_mut_bytes()
    }
}
