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

/// The alignment of the memory, the offsets and the lengths of an unbuffered write: a multiple of
/// every logical sector size NTFS and the Resilient File System use on 512e and 4Kn disks. A volume
/// that refuses it refuses the unbuffered write ([`crate::unbuffered_refused`]).
pub const UNBUFFERED_ALIGNMENT: usize = 4096;

/// Rounds `len` up to a multiple of [`UNBUFFERED_ALIGNMENT`].
pub const fn unbuffered_len(len: usize) -> usize {
    len.next_multiple_of(UNBUFFERED_ALIGNMENT)
}

/// Memory aligned to [`UNBUFFERED_ALIGNMENT`], holding `len` bytes of content followed by zeros up
/// to the next multiple of the alignment: what an unbuffered write writes.
///
/// [`AsRef`] answers the content with its padding, the bytes a write of the buffer writes, and
/// [`Self::content`] the content alone.
pub struct AlignedBuf {
    pointer: std::ptr::NonNull<u8>,
    len: usize,
    capacity: usize,
}

// SAFETY: the buffer owns its allocation, and nothing else refers to it.
unsafe impl Send for AlignedBuf {}
// SAFETY: shared access only reads.
unsafe impl Sync for AlignedBuf {}

impl AlignedBuf {
    /// A buffer of `len` bytes of content, all zeros.
    pub fn zeroed(len: usize) -> AlignedBuf {
        let capacity = unbuffered_len(len);
        if capacity == 0 {
            return AlignedBuf::empty();
        }
        // SAFETY: the layout is valid and its size is not zero.
        let pointer = unsafe { std::alloc::alloc_zeroed(Self::layout(capacity)) };
        AlignedBuf::from_allocation(pointer, len, capacity)
    }

    /// A buffer of `len` bytes of content whose padding is zeroed and whose content is not.
    ///
    /// # Safety
    ///
    /// No byte of the content may be read, or written to a file, before it is written.
    pub unsafe fn uninit(len: usize) -> AlignedBuf {
        let capacity = unbuffered_len(len);
        if capacity == 0 {
            return AlignedBuf::empty();
        }
        // SAFETY: the layout is valid and its size is not zero.
        let pointer = unsafe { std::alloc::alloc(Self::layout(capacity)) };
        let mut buffer = AlignedBuf::from_allocation(pointer, len, capacity);
        buffer.padded_mut()[len..].fill(0);
        buffer
    }

    /// The content.
    pub fn content(&self) -> &[u8] {
        &self.as_ref()[..self.len]
    }

    /// The content, for writing.
    pub fn content_mut(&mut self) -> &mut [u8] {
        let len = self.len;
        &mut self.padded_mut()[..len]
    }

    /// The length of the content.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the buffer holds no content.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Takes `len` bytes of content, at most the buffer's padded length, and zeroes what follows
    /// them up to the next multiple of [`UNBUFFERED_ALIGNMENT`], so a write of the buffer writes
    /// them and zeros. The bytes past that are left as they are, and a write of the buffer no
    /// longer covers them.
    ///
    /// # Safety
    ///
    /// `len` is at most the buffer's padded length. No byte of the new content that the buffer held
    /// past its earlier content may be read, or written to a file, before it is written.
    pub unsafe fn set_len(&mut self, len: usize) {
        debug_assert!(len <= self.capacity, "{len} bytes exceed the buffer");
        let end = unbuffered_len(len);
        // SAFETY: `end` is within the allocation, and every byte up to it is either content the
        // caller writes or padding zeroed here.
        let padding = unsafe { std::slice::from_raw_parts_mut(self.pointer.as_ptr(), end) };
        padding[len..].fill(0);
        self.len = len;
    }

    /// The bytes a buffer of `capacity` bytes allocates.
    fn layout(capacity: usize) -> std::alloc::Layout {
        std::alloc::Layout::from_size_align(capacity, UNBUFFERED_ALIGNMENT)
            .unwrap_or_else(|_| std::alloc::handle_alloc_error(std::alloc::Layout::new::<u8>()))
    }

    fn from_allocation(pointer: *mut u8, len: usize, capacity: usize) -> AlignedBuf {
        let Some(pointer) = std::ptr::NonNull::new(pointer) else {
            std::alloc::handle_alloc_error(Self::layout(capacity));
        };
        AlignedBuf {
            pointer,
            len,
            capacity,
        }
    }

    fn empty() -> AlignedBuf {
        AlignedBuf {
            pointer: std::ptr::NonNull::<[u8; UNBUFFERED_ALIGNMENT]>::dangling().cast(),
            len: 0,
            capacity: 0,
        }
    }

    fn padded_mut(&mut self) -> &mut [u8] {
        let padded = unbuffered_len(self.len);
        // SAFETY: the content and its padding lie within the allocation, which this value owns.
        unsafe { std::slice::from_raw_parts_mut(self.pointer.as_ptr(), padded) }
    }
}

impl AsRef<[u8]> for AlignedBuf {
    fn as_ref(&self) -> &[u8] {
        let padded = unbuffered_len(self.len);
        // SAFETY: the content and its padding lie within the allocation, which this value owns.
        unsafe { std::slice::from_raw_parts(self.pointer.as_ptr(), padded) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        if self.capacity != 0 {
            // SAFETY: the allocation was made with this layout and is not used after.
            unsafe { std::alloc::dealloc(self.pointer.as_ptr(), Self::layout(self.capacity)) };
        }
    }
}

impl StableBuf for AlignedBuf {}

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
