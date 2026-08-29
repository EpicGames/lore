// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::alloc::GlobalAlloc;
use std::alloc::Layout;
use std::ops::Deref;
use std::ops::DerefMut;
use std::ptr::NonNull;

/// Owned heap value that is freed through the allocator it came from.
///
/// `Box` cannot express this: it always frees through the global allocator, and
/// a dedicated rpmalloc heap treats every thread as its owner, so a free that
/// bypasses the heap's mutex races with the other threads using it. Anything
/// allocated from [`crate::allocator::node_block_allocator`] therefore has to be
/// held in one of these rather than in a `Box`.
pub struct HeapBox<T> {
    ptr: NonNull<T>,
    allocator: Option<&'static (dyn GlobalAlloc + Sync)>,
}

// SAFETY: HeapBox owns the value exclusively and gives access only through
// &self / &mut self, so it inherits the value's own thread-safety - the same
// reasoning that makes Box<T> Send and Sync.
unsafe impl<T: Send> Send for HeapBox<T> {}
// SAFETY: see above.
unsafe impl<T: Sync> Sync for HeapBox<T> {}

impl<T> HeapBox<T> {
    /// Allocate an all-zeroes `T` from `allocator`, or from the global allocator
    /// when it is `None`. Callers are restricted to types for which all-zeroes
    /// is a valid value.
    pub fn new_zeroed_in(allocator: Option<&'static (dyn GlobalAlloc + Sync)>) -> Self
    where
        T: zerocopy::FromBytes,
    {
        let layout = Layout::new::<T>();
        debug_assert!(layout.size() > 0);
        // SAFETY: the layout has a non-zero size, and `FromBytes` promises the
        // zeroed allocation is a valid `T`.
        let ptr = unsafe {
            match allocator {
                Some(allocator) => allocator.alloc_zeroed(layout),
                None => std::alloc::alloc_zeroed(layout),
            }
        }
        .cast::<T>();
        let Some(ptr) = NonNull::new(ptr) else {
            std::alloc::handle_alloc_error(layout);
        };
        HeapBox { ptr, allocator }
    }

    /// Copy `value`'s bytes into a fresh allocation from `allocator`, or from the
    /// global allocator when it is `None`.
    pub fn copy_from_in(value: &T, allocator: Option<&'static (dyn GlobalAlloc + Sync)>) -> Self
    where
        T: zerocopy::IntoBytes + zerocopy::Immutable,
    {
        let layout = Layout::new::<T>();
        debug_assert!(layout.size() > 0);
        // SAFETY: the layout has a non-zero size.
        let ptr = unsafe {
            match allocator {
                Some(allocator) => allocator.alloc(layout),
                None => std::alloc::alloc(layout),
            }
        }
        .cast::<T>();
        let Some(ptr) = NonNull::new(ptr) else {
            std::alloc::handle_alloc_error(layout);
        };
        // SAFETY: both regions are `size_of::<T>()` bytes and cannot overlap,
        // the destination having just been allocated.
        unsafe {
            std::ptr::copy_nonoverlapping(
                value.as_bytes().as_ptr(),
                ptr.as_ptr().cast::<u8>(),
                layout.size(),
            );
        }
        HeapBox { ptr, allocator }
    }
}

impl<T> Drop for HeapBox<T> {
    fn drop(&mut self) {
        let layout = Layout::new::<T>();
        // SAFETY: the value was created here and is dropped exactly once, and
        // the pointer came from an allocation of this layout made by the
        // allocator it is being returned to.
        unsafe {
            std::ptr::drop_in_place(self.ptr.as_ptr());
            match self.allocator {
                Some(allocator) => allocator.dealloc(self.ptr.as_ptr().cast::<u8>(), layout),
                None => std::alloc::dealloc(self.ptr.as_ptr().cast::<u8>(), layout),
            }
        }
    }
}

impl<T> Deref for HeapBox<T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the pointer is valid and initialized for as long as self is.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T> DerefMut for HeapBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: see `deref`, and the unique borrow rules out aliasing.
        unsafe { self.ptr.as_mut() }
    }
}

impl<T> AsRef<T> for HeapBox<T> {
    fn as_ref(&self) -> &T {
        self
    }
}

impl<T> AsMut<T> for HeapBox<T> {
    fn as_mut(&mut self) -> &mut T {
        self
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for HeapBox<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        (**self).fmt(f)
    }
}
