// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::alloc::GlobalAlloc;
use std::alloc::Layout;
use std::ops::Deref;
use std::ops::DerefMut;
use std::ptr::NonNull;

/// Growable byte buffer allocated from a caller-chosen heap.
///
/// Exists so the node name tables can share the revision tree's dedicated heap
/// (see [`crate::allocator::node_block_allocator`]) instead of being spread
/// through the general one, which `BytesMut` and `Vec` give no way to control.
/// Only the operations the name tables need are provided.
///
/// The heap is fixed at construction: `new`, `with_capacity` and `from_slice`
/// use the global allocator, and the `_in` constructors take one. The buffer is
/// always released through the allocator it was taken from, which a dedicated
/// rpmalloc heap requires: such a heap counts every thread as its owner, so a
/// free that skips the heap's mutex races with the threads using it.
pub struct HeapBuf {
    ptr: NonNull<u8>,
    len: usize,
    capacity: usize,
    allocator: Option<&'static (dyn GlobalAlloc + Sync)>,
}

// SAFETY: HeapBuf owns its allocation exclusively and hands out references only
// through &self / &mut self, so it is as sendable and shareable as the bytes it
// holds - the same reasoning that makes Vec<u8> Send and Sync.
unsafe impl Send for HeapBuf {}
// SAFETY: see above.
unsafe impl Sync for HeapBuf {}

impl HeapBuf {
    pub fn new() -> Self {
        Self::new_in(None)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_capacity_in(capacity, None)
    }

    pub fn from_slice(bytes: &[u8]) -> Self {
        Self::from_slice_in(bytes, None)
    }

    /// Empty buffer that will take every allocation from `allocator`, or from
    /// the global allocator when it is `None`.
    pub fn new_in(allocator: Option<&'static (dyn GlobalAlloc + Sync)>) -> Self {
        HeapBuf {
            ptr: NonNull::dangling(),
            len: 0,
            capacity: 0,
            allocator,
        }
    }

    pub fn with_capacity_in(
        capacity: usize,
        allocator: Option<&'static (dyn GlobalAlloc + Sync)>,
    ) -> Self {
        let mut buffer = Self::new_in(allocator);
        if capacity > 0 {
            buffer.grow_to(capacity);
        }
        buffer
    }

    pub fn from_slice_in(
        bytes: &[u8],
        allocator: Option<&'static (dyn GlobalAlloc + Sync)>,
    ) -> Self {
        let mut buffer = Self::with_capacity_in(bytes.len(), allocator);
        buffer.extend_from_slice(bytes);
        buffer
    }

    /// The heap this buffer allocates from, for building a second buffer that
    /// is to replace it.
    pub fn allocator(&self) -> Option<&'static (dyn GlobalAlloc + Sync)> {
        self.allocator
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }

    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    pub fn extend_from_slice(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let required = self.len.saturating_add(bytes.len());
        if required > self.capacity {
            self.grow_to(required.max(self.capacity.saturating_mul(2)));
        }
        // SAFETY: the destination is inside an allocation of at least
        // `required` bytes, and source and destination cannot overlap because
        // the destination was just allocated or is uniquely borrowed.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.ptr.as_ptr().add(self.len),
                bytes.len(),
            );
        }
        self.len = required;
    }

    fn grow_to(&mut self, capacity: usize) {
        let layout = Self::layout(capacity);
        // SAFETY: `layout` has a non-zero size, so both allocators are being
        // called within their contract.
        let ptr = unsafe {
            match self.allocator {
                Some(allocator) => allocator.alloc(layout),
                None => std::alloc::alloc(layout),
            }
        };
        let Some(ptr) = NonNull::new(ptr) else {
            std::alloc::handle_alloc_error(layout);
        };
        if self.len > 0 {
            // SAFETY: the new allocation is larger than `len`, and the old one
            // holds `len` initialized bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(self.ptr.as_ptr(), ptr.as_ptr(), self.len);
            }
        }
        self.release();
        self.ptr = ptr;
        self.capacity = capacity;
    }

    fn release(&mut self) {
        if self.capacity == 0 {
            return;
        }
        let layout = Self::layout(self.capacity);
        // SAFETY: the pointer came from an allocation of exactly this layout
        // made by this allocator, and is not used again - the caller either
        // replaces it or drops self.
        unsafe {
            match self.allocator {
                Some(allocator) => allocator.dealloc(self.ptr.as_ptr(), layout),
                None => std::alloc::dealloc(self.ptr.as_ptr(), layout),
            }
        }
        self.ptr = NonNull::dangling();
        self.capacity = 0;
    }

    fn layout(capacity: usize) -> Layout {
        Layout::array::<u8>(capacity).expect("node name buffer capacity overflows a layout")
    }
}

impl Default for HeapBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for HeapBuf {
    fn drop(&mut self) {
        self.release();
    }
}

impl Clone for HeapBuf {
    fn clone(&self) -> Self {
        Self::from_slice_in(self, self.allocator)
    }
}

impl Deref for HeapBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        if self.capacity == 0 {
            return &[];
        }
        // SAFETY: the first `len` bytes of the allocation are initialized by
        // `extend_from_slice`, the only writer of `len`.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl DerefMut for HeapBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        if self.capacity == 0 {
            return &mut [];
        }
        // SAFETY: see `deref`, and the unique borrow rules out aliasing.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl AsRef<[u8]> for HeapBuf {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl std::fmt::Debug for HeapBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeapBuf")
            .field("len", &self.len)
            .field("capacity", &self.capacity)
            .finish()
    }
}
