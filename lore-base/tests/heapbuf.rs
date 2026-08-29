// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    use lore_base::allocator::HeapBuf;
    use lore_base::allocator::node_block_allocator;

    #[test]
    fn empty_buffer_allocates_nothing() {
        let buffer = HeapBuf::new();
        assert!(buffer.is_empty());
        assert_eq!(buffer.len(), 0);
        assert_eq!(buffer.capacity(), 0);
        assert_eq!(&buffer[..], b"");
    }

    #[test]
    fn extend_grows_and_preserves_contents() {
        let mut buffer = HeapBuf::new();
        for index in 0..1000u32 {
            buffer.extend_from_slice(&index.to_le_bytes());
        }
        assert_eq!(buffer.len(), 4000);
        assert!(buffer.capacity() >= 4000);
        for index in 0..1000u32 {
            let offset = index as usize * 4;
            let bytes: [u8; 4] = buffer[offset..offset + 4].try_into().unwrap();
            assert_eq!(u32::from_le_bytes(bytes), index);
        }
    }

    #[test]
    fn with_capacity_does_not_grow_until_exceeded() {
        let mut buffer = HeapBuf::with_capacity(64);
        assert_eq!(buffer.capacity(), 64);
        assert!(buffer.is_empty());
        buffer.extend_from_slice(&[7u8; 64]);
        assert_eq!(buffer.len(), 64);
        assert_eq!(buffer.capacity(), 64);
        buffer.extend_from_slice(&[9u8; 1]);
        assert!(buffer.capacity() > 64);
        assert_eq!(buffer[63], 7);
        assert_eq!(buffer[64], 9);
    }

    #[test]
    fn from_slice_and_clone_are_independent_copies() {
        let buffer = HeapBuf::from_slice(b"revision tree");
        let mut clone = buffer.clone();
        clone.extend_from_slice(b" name table");
        assert_eq!(&buffer[..], b"revision tree");
        assert_eq!(&clone[..], b"revision tree name table");
    }

    #[test]
    fn the_chosen_allocator_survives_growth_and_cloning() {
        let allocator = node_block_allocator();
        let mut buffer = HeapBuf::with_capacity_in(8, allocator);
        buffer.extend_from_slice(&[3u8; 64]);
        assert_eq!(buffer.allocator().is_some(), allocator.is_some());

        let clone = buffer.clone();
        assert_eq!(clone.allocator().is_some(), allocator.is_some());
        assert_eq!(&clone[..], &buffer[..]);

        // The plain constructors stay on the global allocator, so a buffer that
        // is not part of the revision tree cannot end up on its heap.
        assert!(HeapBuf::new().allocator().is_none());
        assert!(HeapBuf::from_slice(b"global").allocator().is_none());
    }

    #[test]
    fn writes_through_the_mutable_pointer_are_visible() {
        let mut buffer = HeapBuf::from_slice(b"aaaa");
        // The name table patches names in place through this pointer when a
        // replacement fits the previous slot.
        unsafe {
            std::ptr::copy_nonoverlapping(b"bb".as_ptr(), buffer.as_mut_ptr().add(1), 2);
        }
        assert_eq!(&buffer[..], b"abba");
    }
}
