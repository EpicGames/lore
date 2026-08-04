// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use bytes::Bytes;

/// Buffer types a write can own while the operation is in flight.
///
/// Implementors must guarantee the underlying memory does not move for the
/// lifetime of the value, since a backend may hold a raw pointer into it
/// while an operation is in flight.
///
/// Only writes take a buffer from the caller. Reads allocate their own and hand back [`Bytes`],
/// so that the memory the kernel copies into belongs to the thread receiving the copy.
pub trait StableBuf: AsRef<[u8]> + Send + 'static {}

impl StableBuf for Bytes {}
impl StableBuf for Vec<u8> {}

/// Allocates `len` bytes for a read to fill, without zeroing them.
///
/// The kernel overwrites what it fills and the caller only ever sees the filled prefix — every
/// read here truncates to the byte count before converting to [`Bytes`] — so zeroing first would
/// write every byte twice. That second pass is not free at these sizes: it measured around a tenth
/// of the benchmark's CPU time when reads allocated zeroed buffers.
///
/// # Safety
///
/// The returned vector's contents are uninitialised. Callers must fill a prefix and truncate to
/// it before exposing any of it.
pub(crate) unsafe fn uninit_buffer(len: usize) -> bytes::BytesMut {
    let mut buffer = bytes::BytesMut::with_capacity(len);
    // SAFETY: `with_capacity` guarantees the allocation, and the caller truncates to what it
    // fills before any byte is read.
    unsafe { buffer.set_len(len) };
    buffer
}
