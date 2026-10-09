// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A zstd stream of content made from the leaves Lore delivers as it stores them.
//!
//! A leaf Lore stored with zstd, its default, is already one complete zstd frame: plain, with no
//! dictionary and nothing around it. It is passed on as it is, never expanded and compressed
//! again. A leaf stored uncompressed goes into raw blocks, which costs a few bytes of framing and
//! no compression; Lore stores a leaf uncompressed only when compressing it saved nothing. A leaf
//! stored with any other codec is refused: lore-rbe writes with Lore's default codec, zstd. Frames
//! concatenated in content order are a zstd stream of the whole content, which is what REAPI's
//! `compressed-blobs/zstd` reads answer with.

use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;

/// zstd's frame magic number, as it appears on the wire.
const FRAME_MAGIC: [u8; 4] = 0xFD2F_B528u32.to_le_bytes();

/// The largest block a zstd frame may carry, whatever its window.
const BLOCK_SIZE_MAX: usize = 128 * 1024;

/// Appends one leaf, `payload` as `fragment` describes it, to `stream` as one zstd frame.
///
/// A leaf stored with zstd is passed on and one stored uncompressed is framed. Any other codec is
/// an error, as is a payload that is not what its fragment says it is.
pub fn append_leaf(
    stream: &mut Vec<u8>,
    fragment: &Fragment,
    payload: &[u8],
) -> Result<(), String> {
    let compression = fragment.flags & FragmentFlags::PayloadCompressed.bits();
    if compression == FragmentFlags::PayloadCompressedZstd.bits() {
        if payload.len() != fragment.size_payload as usize || !payload.starts_with(&FRAME_MAGIC) {
            return Err(format!(
                "a zstd leaf of {} bytes, {} stated, is not a zstd frame",
                payload.len(),
                fragment.size_payload
            ));
        }
        stream.extend_from_slice(payload);
    } else if compression == 0 {
        if payload.len() as u64 != fragment.size_content {
            return Err(format!(
                "an uncompressed leaf of {} bytes states {} bytes of content",
                payload.len(),
                fragment.size_content
            ));
        }
        append_raw_frame(stream, payload);
    } else {
        return Err(format!(
            "a leaf compressed as {compression:#x}, which is not zstd"
        ));
    }
    Ok(())
}

/// Appends `content` to `stream` as a zstd frame of raw blocks: compressed by nothing, readable by
/// any zstd decoder.
pub fn append_raw_frame(stream: &mut Vec<u8>, content: &[u8]) {
    let blocks = content.len().div_ceil(BLOCK_SIZE_MAX).max(1);
    stream.reserve(FRAME_MAGIC.len() + 9 + 3 * blocks + content.len());
    stream.extend_from_slice(&FRAME_MAGIC);
    // Frame header descriptor: an eight-byte Frame_Content_Size (bits 7-6 = 3) in a single segment
    // (bit 5), so the window is the content and no window descriptor follows. No checksum and no
    // dictionary.
    stream.push(0b1110_0000);
    stream.extend_from_slice(&(content.len() as u64).to_le_bytes());
    // Empty content is still one block: a frame ends with its last block, even an empty one.
    let mut chunks = content.chunks(BLOCK_SIZE_MAX).peekable();
    if chunks.peek().is_none() {
        stream.extend_from_slice(&raw_block_header(0, true));
    }
    while let Some(chunk) = chunks.next() {
        stream.extend_from_slice(&raw_block_header(chunk.len(), chunks.peek().is_none()));
        stream.extend_from_slice(chunk);
    }
}

/// A raw block's three-byte header: `Last_Block` in bit 0, `Block_Type` 0 (raw) in bits 1-2, and
/// the block size from bit 3, little-endian.
fn raw_block_header(size: usize, last: bool) -> [u8; 3] {
    debug_assert!(size <= BLOCK_SIZE_MAX);
    let header = ((size as u32) << 3) | u32::from(last);
    let bytes = header.to_le_bytes();
    [bytes[0], bytes[1], bytes[2]]
}
