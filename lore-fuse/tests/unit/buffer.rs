// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_fuse::abi::InHeader;
use lore_fuse::abi::WriteIn;
use lore_fuse::buffer::*;

#[test]
fn request_buffer_is_8_byte_aligned() {
    let mut buffer = RequestBuffer::new();

    let address = buffer.as_mut().as_ptr() as usize;

    assert_eq!(address % 8, 0);
}

#[test]
fn request_buffer_for_max_write_holds_a_full_write() {
    let max_write = 1 << 20;
    let mut buffer = RequestBuffer::for_max_write(max_write);

    let bytes = buffer.as_mut();

    let write_header = size_of::<InHeader>() + size_of::<WriteIn>();
    assert!(bytes.len() >= write_header + max_write as usize);
    assert_eq!(bytes.as_ptr() as usize % 8, 0);
}

#[test]
fn request_buffer_for_a_small_max_write_keeps_the_default_size() {
    let mut buffer = RequestBuffer::for_max_write(4096);

    assert_eq!(buffer.as_mut().len(), REQUEST_BUFFER_BYTES);
}
