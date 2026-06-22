// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use bytes::BytesMut;
use zerocopy::IntoBytes;

pub trait TypedBytes {
    fn from_type_static<T>(slice: &'static [T]) -> bytes::Bytes
    where
        T: zerocopy::IntoBytes + zerocopy::Immutable;

    fn count<T>(&self) -> usize
    where
        T: zerocopy::IntoBytes;

    fn to_aligned<T>(self) -> Self
    where
        T: zerocopy::IntoBytes;

    fn as_type_slice<T>(&self) -> &[T]
    where
        T: zerocopy::IntoBytes;

    fn clone_and_resize_zeroed<T>(&self, count: usize) -> BytesMut
    where
        T: zerocopy::IntoBytes;
}

impl TypedBytes for bytes::Bytes {
    fn from_type_static<T>(slice: &'static [T]) -> bytes::Bytes
    where
        T: zerocopy::IntoBytes + zerocopy::Immutable,
    {
        Self::from_static(slice.as_bytes())
    }

    fn count<T>(&self) -> usize
    where
        T: zerocopy::IntoBytes,
    {
        self.len() / std::mem::size_of::<T>()
    }

    fn as_type_slice<T>(&self) -> &[T]
    where
        T: zerocopy::IntoBytes,
    {
        let count = self.count::<T>();
        if count > 0 {
            let ptr = self.as_ptr().cast::<T>();
            debug_assert!(ptr.is_aligned(), "Bytes buffer is not aligned for type");
            unsafe { std::slice::from_raw_parts(ptr, count) }
        } else {
            &[]
        }
    }

    fn to_aligned<T>(self) -> Self
    where
        T: zerocopy::IntoBytes,
    {
        if !self.is_empty() {
            let ptr = self.as_ptr().cast::<T>();
            if !ptr.is_aligned() {
                let mut aligned_buffer = BytesMut::with_capacity(self.len());
                aligned_buffer.extend_from_slice(&self);
                aligned_buffer.freeze()
            } else {
                self
            }
        } else {
            self
        }
    }

    fn clone_and_resize_zeroed<T>(&self, count: usize) -> BytesMut
    where
        T: zerocopy::IntoBytes,
    {
        let capacity = count * std::mem::size_of::<T>();
        let mut buffer = BytesMut::with_capacity(capacity);
        unsafe { buffer.set_len(capacity) };

        buffer.as_mut()[..self.len()].copy_from_slice(self.as_ref());
        if self.len() < capacity {
            buffer.as_mut()[self.len()..].fill(0);
        }

        buffer
    }
}

pub trait TypedBytesMut {
    fn zeroed_count<T>(count: usize) -> bytes::BytesMut
    where
        T: zerocopy::IntoBytes;

    fn with_count_capacity<T>(count: usize) -> bytes::BytesMut
    where
        T: zerocopy::IntoBytes;

    fn count<T>(&self) -> usize
    where
        T: zerocopy::IntoBytes;

    /// Set the buffer length to the given count multiple of type size
    /// # Safety
    /// Caller must ensure buffer has the required capacity and that all
    /// elements are initialized. See `set_len` function for details.
    unsafe fn set_count<T>(&mut self, count: usize)
    where
        T: zerocopy::IntoBytes;

    fn as_type_slice<T>(&self) -> &[T]
    where
        T: zerocopy::IntoBytes;

    fn as_type_slice_mut<T>(&mut self) -> &mut [T]
    where
        T: zerocopy::IntoBytes;
}

impl TypedBytesMut for bytes::BytesMut {
    fn zeroed_count<T>(count: usize) -> bytes::BytesMut
    where
        T: zerocopy::IntoBytes,
    {
        Self::zeroed(count * std::mem::size_of::<T>())
    }

    fn with_count_capacity<T>(count: usize) -> bytes::BytesMut
    where
        T: zerocopy::IntoBytes,
    {
        Self::with_capacity(count * std::mem::size_of::<T>())
    }

    fn count<T>(&self) -> usize
    where
        T: zerocopy::IntoBytes,
    {
        self.len() / std::mem::size_of::<T>()
    }

    unsafe fn set_count<T>(&mut self, count: usize)
    where
        T: zerocopy::IntoBytes,
    {
        unsafe {
            self.set_len(count * std::mem::size_of::<T>());
        }
    }

    fn as_type_slice<T>(&self) -> &[T]
    where
        T: zerocopy::IntoBytes,
    {
        unsafe {
            std::slice::from_raw_parts(
                self.as_ptr() as *mut T,
                self.capacity() / std::mem::size_of::<T>(),
            )
        }
    }
    fn as_type_slice_mut<T>(&mut self) -> &mut [T]
    where
        T: zerocopy::IntoBytes,
    {
        unsafe {
            std::slice::from_raw_parts_mut(
                self.as_mut_ptr().cast::<T>(),
                self.capacity() / std::mem::size_of::<T>(),
            )
        }
    }
}
