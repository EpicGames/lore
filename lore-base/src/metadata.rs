// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Portable operations on Lore's serialized metadata buffer.
use bytes::BytesMut;

const MAGIC: u32 = 0x6d657461;
const VERSION: u32 = 1;
const HEADER_SIZE: usize = 8;
const ITEM_SIZE: usize = 12;
/// Largest serialized metadata buffer supported by Lore.
pub const METADATA_MAX_SIZE: usize = 1024 * 1024;

fn u32_at(buffer: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(
        buffer[offset..offset + 4]
            .try_into()
            .expect("header checked"),
    )
}

/// Check the serialized header and entry boundaries before reading a buffer.
pub fn check_buffer(buffer: &[u8]) -> Result<(), &'static str> {
    if buffer.is_empty() {
        return Ok(());
    }
    if buffer.len() < HEADER_SIZE || u32_at(buffer, 0) != MAGIC || u32_at(buffer, 4) != VERSION {
        return Err("bad metadata header");
    }
    let mut offset = HEADER_SIZE;
    while offset < buffer.len() {
        if ITEM_SIZE > buffer.len() - offset {
            return Err("truncated metadata entry");
        }
        let key = u32_at(buffer, offset) as usize;
        let value = u32_at(buffer, offset + 4) as usize;
        offset += ITEM_SIZE;
        let payload = key
            .checked_add(value)
            .ok_or("metadata entry overruns the buffer")?;
        if payload > buffer.len() - offset {
            return Err("metadata entry overruns the buffer");
        }
        offset += payload;
    }
    Ok(())
}

/// Set an already encoded key/value, retaining Lore's replacement order.
/// A buffer from outside the process must first pass [`check_buffer`].
pub fn set_encoded(
    buffer: &mut BytesMut,
    key: &[u8],
    value: &[u8],
    tag: u32,
) -> Result<(), &'static str> {
    let key_len = u32::try_from(key.len()).map_err(|_| "metadata key is too large")?;
    let value_len = u32::try_from(value.len()).map_err(|_| "metadata value is too large")?;
    if buffer.is_empty() {
        buffer.extend_from_slice(&MAGIC.to_ne_bytes());
        buffer.extend_from_slice(&VERSION.to_ne_bytes());
    }
    let mut offset = HEADER_SIZE;
    while offset < buffer.len() {
        if buffer.len() - offset < ITEM_SIZE {
            return Err("truncated metadata entry");
        }
        let key_size = u32_at(buffer, offset) as usize;
        let value_size = u32_at(buffer, offset + 4) as usize;
        let start = offset;
        offset += ITEM_SIZE;
        let end = offset
            .checked_add(key_size)
            .and_then(|n| n.checked_add(value_size))
            .ok_or("metadata entry overruns the buffer")?;
        if end > buffer.len() {
            return Err("metadata entry overruns the buffer");
        }
        if buffer[offset..offset + key_size] == *key {
            if value_size == value.len() {
                buffer[start + 8..start + 12].copy_from_slice(&tag.to_ne_bytes());
                buffer[offset + key_size..end].copy_from_slice(value);
                return Ok(());
            }
            buffer.copy_within(end.., start);
            buffer.truncate(buffer.len() - (end - start));
            break;
        }
        offset = end;
    }
    buffer.extend_from_slice(&key_len.to_ne_bytes());
    buffer.extend_from_slice(&value_len.to_ne_bytes());
    buffer.extend_from_slice(&tag.to_ne_bytes());
    buffer.extend_from_slice(key);
    buffer.extend_from_slice(value);
    Ok(())
}
