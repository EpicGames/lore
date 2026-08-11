// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::alloc::Layout;
use std::io;
use std::marker::PhantomData;
use std::ops::Deref;
use std::ops::DerefMut;
use std::ops::Index;
use std::ops::IndexMut;
use std::ptr::NonNull;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

static GROWVEC_MEMORY_USED: AtomicU64 = AtomicU64::new(0);

const DEFAULT_CHUNK_SIZE: usize = 64;

struct GrowBox<T> {
    ptr: NonNull<T>,
    _marker: PhantomData<T>,
}

// SAFETY: Pointer to type is safe to send if type is send
unsafe impl<T: Send> Send for GrowBox<T> {}
// SAFETY: Pointer to type is safe to sync if type is sync
unsafe impl<T: Sync> Sync for GrowBox<T> {}

impl<T> GrowBox<T> {
    /// Allocates a chunk without zeroing it.
    ///
    /// # Safety
    ///
    /// Every byte must be written before anything reads the chunk. `T` here is an array of
    /// `FromBytes` elements, so any bit pattern is a valid value once written, but reading the
    /// allocation before it is filled reads uninitialised memory.
    unsafe fn new_uninit() -> Self {
        let Ok(layout) = Layout::from_size_align(size_of::<T>(), align_of::<T>()) else {
            panic!("Unable to construct memory layout for heap boxed item");
        };

        // SAFETY: The allocator is safe, we check return and panic on OOM. The caller fills
        // every byte before the contents are read.
        let block = unsafe { super::growvec_allocator().alloc(layout) };
        let Some(ptr) = NonNull::new(block.cast()) else {
            panic!("Unable to allocate memory for heap boxed item");
        };

        GROWVEC_MEMORY_USED.fetch_add(layout.size() as u64, Ordering::Relaxed);

        Self {
            ptr,
            _marker: PhantomData,
        }
    }

    fn new_zeroed() -> Self
    where
        T: FromZeros,
    {
        let Ok(layout) = Layout::from_size_align(size_of::<T>(), align_of::<T>()) else {
            panic!("Unable to construct memory layout for heap boxed item");
        };

        // SAFETY: The allocator is safe, we check return and panic on OOM
        let block = unsafe { super::growvec_allocator().alloc_zeroed(layout) };
        let Some(ptr) = NonNull::new(block.cast()) else {
            panic!("Unable to allocate memory for heap boxed item");
        };

        GROWVEC_MEMORY_USED.fetch_add(layout.size() as u64, Ordering::Relaxed);

        Self {
            ptr,
            _marker: PhantomData,
        }
    }

    fn as_ref(&self) -> &T {
        // SAFETY: Type is safe to access as zero initialized
        unsafe { self.ptr.as_ref() }
    }

    fn as_mut(&mut self) -> &mut T {
        // SAFETY: Type is safe to access as zero initialized
        unsafe { self.ptr.as_mut() }
    }
}

impl<T> Drop for GrowBox<T> {
    fn drop(&mut self) {
        let Ok(layout) = Layout::from_size_align(size_of::<T>(), align_of::<T>()) else {
            panic!("Unable to construct memory layout for heap boxed item");
        };

        GROWVEC_MEMORY_USED.fetch_sub(layout.size() as u64, Ordering::Relaxed);

        // SAFETY: The allocator is safe
        unsafe { super::growvec_allocator().dealloc(self.ptr.as_ptr().cast(), layout) };
    }
}

impl<T> Deref for GrowBox<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}

impl<T> DerefMut for GrowBox<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.as_mut()
    }
}

impl<T> Clone for GrowBox<T> {
    fn clone(&self) -> Self {
        let Ok(layout) = Layout::from_size_align(size_of::<T>(), align_of::<T>()) else {
            panic!("Unable to construct memory layout for heap boxed item");
        };

        // SAFETY: The allocator is safe, we panic on OOM and will overwrite the entire area on success
        let block = unsafe { super::growvec_allocator().alloc(layout) };
        let Some(ptr) = NonNull::new(block.cast()) else {
            panic!("Unable to allocate memory for heap boxed item");
        };

        GROWVEC_MEMORY_USED.fetch_add(layout.size() as u64, Ordering::Relaxed);

        // SAFETY: The source is safely zero initialized, target is guaranteed to be at least required size
        unsafe {
            std::ptr::copy_nonoverlapping(self.ptr.as_ptr(), ptr.as_ptr(), 1);
        }

        Self {
            ptr,
            _marker: PhantomData,
        }
    }
}

#[derive(Clone, Default)]
pub struct GrowVec<T, const N: usize = DEFAULT_CHUNK_SIZE>
where
    T: FromZeros,
{
    chunks: Vec<GrowBox<GrowChunk<T, N>>>,
    partial_len: usize,
}

#[derive(Clone, FromZeros)]
pub struct GrowChunk<T, const N: usize> {
    element: [T; N],
}

impl<T, const N: usize> GrowVec<T, N>
where
    T: FromZeros + FromBytes + IntoBytes + Immutable + Copy,
{
    pub fn new() -> Self {
        Self {
            chunks: Vec::new(),
            partial_len: 0,
        }
    }

    pub fn new_zeroed_with_size(mut size: usize) -> Self {
        let mut vec = Self::new();

        while size >= N {
            let chunk = GrowBox::new_zeroed();
            vec.chunks.push(chunk);
            size -= N;
            vec.partial_len = N;
        }

        if size > 0 {
            let chunk = GrowBox::new_zeroed();
            vec.chunks.push(chunk);
            vec.partial_len = size;
        }

        vec
    }

    /// Allocates room for `size` elements without zeroing the chunks the caller is about to
    /// fill completely.
    ///
    /// The last chunk is zeroed regardless: only its first `size % N` elements are inside the
    /// vector, and leaving the rest uninitialised would put uninitialised memory in a `[T; N]`.
    /// Every earlier chunk is written whole by the read this exists for, so zeroing it first
    /// would write it twice — the same reasoning `lore-io` applies to its own read buffers.
    ///
    /// # Safety
    ///
    /// Every byte of [`GrowVec::byte_segments_mut`] must be written before any element is read,
    /// and a read that fills less than all of it must be reported as an error rather than
    /// returning the vector.
    pub unsafe fn new_unzeroed_with_size(mut size: usize) -> Self {
        let mut vec = Self::new();

        while size >= N {
            // SAFETY: the caller fills every byte of every full chunk before reading it.
            let chunk = unsafe { GrowBox::new_uninit() };
            vec.chunks.push(chunk);
            size -= N;
            vec.partial_len = N;
        }

        if size > 0 {
            let chunk = GrowBox::new_zeroed();
            vec.chunks.push(chunk);
            vec.partial_len = size;
        }

        vec
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    pub fn len(&self) -> usize {
        if self.is_empty() {
            0
        } else {
            (self.chunks.len() - 1) * N + self.partial_len
        }
    }

    pub fn push(&mut self, item: T) {
        if self.partial_len == N || self.chunks.is_empty() {
            self.chunks.push(GrowBox::new_zeroed());
            self.partial_len = 0;
        }

        self.chunks.last_mut().unwrap().element[self.partial_len] = item;
        self.partial_len += 1;
    }

    pub fn insert(&mut self, index: usize, item: T) {
        let previous_len = self.len();
        if index > previous_len {
            panic!("insertion index (is {index}) should be <= len (is {previous_len})");
        }

        // Make space, does not matter we construct a copy, data type is copyable
        self.push(item);

        if index == previous_len {
            // Pushed in correct place, early out
            return;
        }

        // We can shuffle things in place now as size is correct
        let last_chunk_index = self.chunks.len() - 1;
        let (start_chunk_index, element_index) = Self::split_index(index);

        // Start with the first partial chunk
        let chunk = &mut self.chunks[start_chunk_index];
        let mut overflow = chunk.element[N - 1];
        if element_index < N - 1 {
            // SAFETY: Ok, capped to the element array bounds
            unsafe {
                std::ptr::copy(
                    chunk.element.as_ptr().add(element_index),
                    chunk.element.as_mut_ptr().add(element_index + 1),
                    N - (element_index + 1),
                );
            }
        }

        chunk.element[element_index] = item;

        if start_chunk_index < last_chunk_index {
            // Shuffle all the chunks. Since data is zero initialized, copyable and no drop
            // it is safe to just copy the last chunk partial data as a whole chunk
            for chunk_index in (start_chunk_index + 1)..self.chunks.len() {
                let chunk = &mut self.chunks[chunk_index];
                let next_overflow = chunk.element[N - 1];

                // SAFETY: Ok, capped to the element array bounds
                unsafe {
                    std::ptr::copy(
                        chunk.element.as_ptr(),
                        chunk.element.as_mut_ptr().add(1),
                        N - 1,
                    );
                }

                chunk.element[0] = overflow;
                overflow = next_overflow;
            }
        }
    }

    pub fn to_vec(&self) -> Vec<T>
    where
        T: Clone,
    {
        if self.is_empty() {
            return vec![];
        }

        let capacity = self.len();
        let mut vec = Vec::with_capacity(capacity);
        for chunk in self.chunks.iter().take(self.chunks.len() - 1) {
            vec.extend_from_slice(&chunk.element);
        }
        vec.extend_from_slice(&self.chunks.last().unwrap().element[..self.partial_len]);
        vec
    }

    pub fn iter(&self) -> GrowIter<'_, T, N> {
        GrowIter {
            chunks: self.chunks.as_slice(),
            chunk_index: 0,
            element_index: 0,
            remaining: self.len(),
        }
    }

    pub fn iter_mut(&mut self) -> GrowIterMut<'_, T, N> {
        let remaining = self.len();
        let chunks: *mut GrowBox<GrowChunk<T, N>> = self.chunks.as_mut_ptr();
        GrowIterMut {
            chunks,
            chunk_index: 0,
            element_index: 0,
            remaining,
            _marker: PhantomData,
        }
    }

    fn split_index(index: usize) -> (usize, usize) {
        (index / N, index % N)
    }

    pub fn get_unchecked(&self, index: usize) -> &T {
        let (chunk_index, element_index) = Self::split_index(index);
        &self.chunks[chunk_index].element[element_index]
    }

    pub fn get_unchecked_mut(&mut self, index: usize) -> &mut T {
        let (chunk_index, element_index) = Self::split_index(index);
        &mut self.chunks[chunk_index].element[element_index]
    }

    /// The occupied bytes of each chunk, in element order. The chunk
    /// allocations are stable heap blocks: they do not move when the
    /// `GrowVec` itself moves, only when elements are inserted or pushed.
    pub fn byte_segments(&self) -> impl Iterator<Item = &[u8]> {
        let chunk_count = self.chunks.len();
        let partial_len = self.partial_len;
        self.chunks.iter().enumerate().map(move |(index, chunk)| {
            if index + 1 == chunk_count {
                chunk.element[..partial_len].as_bytes()
            } else {
                chunk.element.as_bytes()
            }
        })
    }

    /// The occupied bytes of each chunk, mutably, in element order. Same
    /// stability guarantee as [`byte_segments`](Self::byte_segments).
    pub fn byte_segments_mut(&mut self) -> impl Iterator<Item = &mut [u8]> {
        let chunk_count = self.chunks.len();
        let partial_len = self.partial_len;
        self.chunks
            .iter_mut()
            .enumerate()
            .map(move |(index, chunk)| {
                if index + 1 == chunk_count {
                    chunk.element[..partial_len].as_mut_bytes()
                } else {
                    chunk.element.as_mut_bytes()
                }
            })
    }

    /// Reads `expected_count` elements from `reader` — a file, a byte
    /// slice, or any other `Read` source.
    pub fn read_from(
        reader: &mut impl io::Read,
        mut expected_count: usize,
    ) -> Result<Self, io::Error> {
        let mut vec = Self::new();

        while expected_count >= N {
            // SAFETY: `read_exact` below writes every byte of the chunk or returns an error, in
            // which case the vector is dropped here and never read from.
            let mut chunk: GrowBox<GrowChunk<T, N>> = unsafe { GrowBox::new_uninit() };
            reader.read_exact(chunk.element.as_mut_bytes())?;
            vec.chunks.push(chunk);
            expected_count -= N;
            vec.partial_len = N;
        }

        if expected_count > 0 {
            let mut chunk: GrowBox<GrowChunk<T, N>> = GrowBox::new_zeroed();
            reader.read_exact(chunk.element[..expected_count].as_mut_bytes())?;
            vec.chunks.push(chunk);
            vec.partial_len = expected_count;
        }

        Ok(vec)
    }

    /// Writes all elements to `writer` — a file, a byte-slice cursor, or
    /// any other `Write` sink.
    pub fn write_to(&self, writer: &mut impl io::Write) -> Result<(), io::Error> {
        if self.is_empty() {
            return Ok(());
        }

        for chunk in self.chunks.iter().take(self.chunks.len() - 1) {
            writer.write_all(chunk.element.as_bytes())?;
        }

        if let Some(chunk) = self.chunks.last() {
            writer.write_all(chunk.element[..self.partial_len].as_bytes())?;
        }

        Ok(())
    }
}

impl<T, const N: usize> Index<usize> for GrowVec<T, N>
where
    T: FromZeros + FromBytes + IntoBytes + Immutable + Copy,
{
    type Output = T;

    fn index(&self, index: usize) -> &Self::Output {
        self.get_unchecked(index)
    }
}

impl<T, const N: usize> IndexMut<usize> for GrowVec<T, N>
where
    T: FromZeros + FromBytes + IntoBytes + Immutable + Copy,
{
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        self.get_unchecked_mut(index)
    }
}

pub struct GrowIter<'a, T, const N: usize> {
    chunks: &'a [GrowBox<GrowChunk<T, N>>],
    chunk_index: usize,
    element_index: usize,
    remaining: usize,
}

impl<'a, T, const N: usize> Iterator for GrowIter<'a, T, N> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }

        let item = Some(&self.chunks[self.chunk_index].element[self.element_index]);

        self.remaining -= 1;
        self.element_index += 1;

        if self.element_index == N {
            self.element_index = 0;
            self.chunk_index += 1;
        }

        item
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

pub struct GrowIterMut<'a, T, const N: usize> {
    chunks: *mut GrowBox<GrowChunk<T, N>>,
    chunk_index: usize,
    element_index: usize,
    remaining: usize,
    _marker: PhantomData<&'a mut T>,
}

// SAFETY: Type is required Send, pointer is guaranteed to be valid by lifetime
unsafe impl<'a, T: Send, const N: usize> Send for GrowIterMut<'a, T, N> {}

impl<'a, T, const N: usize> Iterator for GrowIterMut<'a, T, N> {
    type Item = &'a mut T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }

        // SAFETY: Pointer validity is guaranteed by lifetime, chunk index is guaranteed to be within range
        let chunk: *mut GrowBox<GrowChunk<T, N>> = unsafe { self.chunks.add(self.chunk_index) };
        // SAFETY: Pointer validity is guaranteed by lifetime, element index is guaranteed to be within range
        let item: *mut T = unsafe {
            (*chunk)
                .ptr
                .as_mut()
                .element
                .as_mut_ptr()
                .add(self.element_index)
        };

        self.remaining -= 1;
        self.element_index += 1;

        if self.element_index == N {
            self.element_index = 0;
            self.chunk_index += 1;
        }

        // SAFETY: Pointer validity is guaranteed by lifetime
        unsafe { Some(&mut *item) }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

#[derive(Default)]
pub struct GrowVecMemoryStats {
    /// Actual bytes used in growvec chunks
    pub used_bytes: usize,
    /// Number of bytes allocated by the backing allocator (zero if using System)
    pub allocated_bytes: usize,
    /// Number of bytes committed by the backing allocator (zero if using System)
    pub committed_bytes: usize,
}

pub fn memory_stats() -> GrowVecMemoryStats {
    let stats = super::growvec_allocator_stats();
    GrowVecMemoryStats {
        used_bytes: GROWVEC_MEMORY_USED.load(Ordering::Relaxed) as usize,
        allocated_bytes: stats.allocated_bytes,
        committed_bytes: stats.committed_bytes,
    }
}
