// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

mod device;
mod mock;

use lore_fuse::buffer::REQUEST_BUFFER_BYTES;
use lore_fuse::buffer::ReplyBuffer;
use lore_fuse::buffer::RequestBuffer;

#[test]
fn request_buffer_is_16k_and_reply_buffer_is_max_pages_pages() {
    assert_eq!(REQUEST_BUFFER_BYTES, 16 * 1024);
    let mut request = RequestBuffer::new();
    assert_eq!(request.as_mut().len(), REQUEST_BUFFER_BYTES);

    let max_pages = 4u16;
    let mut reply = ReplyBuffer::new(max_pages);
    assert_eq!(reply.as_mut().len(), max_pages as usize * 4096);
}
