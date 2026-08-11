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

/// Owned multi-segment memory that vectored write operations gather from.
///
/// The [`StableBuf`] contract applies to every segment: the segment
/// memory must not move for the value's lifetime, since a backend holds
/// raw pointers into each segment while an operation is in flight.
/// Moving the value itself must not move the segments (heap-allocated
/// segments behind pointers satisfy this).
pub trait StableBufList: Send + 'static {
    fn byte_segments(&self) -> impl Iterator<Item = &[u8]>;
}

/// Owned multi-segment memory that vectored read operations scatter into.
///
/// Same stability contract as [`StableBufList`]. Segment contents are
/// unspecified until the read fills them.
///
/// A scattering read is the one case where a read does not allocate its own memory: the caller
/// already owns the structure the bytes belong in, and the point of the operation is to land them
/// there without a staging copy.
pub trait StableBufListMut: Send + 'static {
    fn byte_segments_mut(&mut self) -> impl Iterator<Item = &mut [u8]>;
}

impl StableBufList for Vec<Vec<u8>> {
    fn byte_segments(&self) -> impl Iterator<Item = &[u8]> {
        self.iter().map(|segment| segment.as_slice())
    }
}

impl StableBufListMut for Vec<Vec<u8>> {
    fn byte_segments_mut(&mut self) -> impl Iterator<Item = &mut [u8]> {
        self.iter_mut().map(|segment| segment.as_mut_slice())
    }
}

/// Allocates `len` bytes for a read to fill, without zeroing them.
///
/// The kernel overwrites what it fills and the caller only ever sees the filled prefix — every
/// read here truncates to the byte count before converting to [`Bytes`] — so zeroing first would
/// write every byte twice. That second pass is not free at these sizes: it measured around a tenth
/// of the benchmark's CPU time when reads allocated zeroed buffers.
///
/// # Safety
///
/// The returned buffer's contents are uninitialised. No byte may be read before it is written:
/// the reads here fill a prefix and truncate to it, and a caller reading into a buffer it holds
/// across reads must confine itself to the region those reads filled.
pub unsafe fn uninit_buffer(len: usize) -> bytes::BytesMut {
    let mut buffer = bytes::BytesMut::with_capacity(len);
    // SAFETY: `with_capacity` guarantees the allocation, and the caller only reads bytes it has
    // written.
    unsafe { buffer.set_len(len) };
    buffer
}
