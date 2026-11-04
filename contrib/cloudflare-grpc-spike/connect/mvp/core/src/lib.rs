// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Narrow synchronous ABI between the Worker adapter and Lore's portable code.
//! The synchronous adapter owns allocations and metadata handles until freed.

use bytes::BytesMut;
use lore_base::types::{
    Fragment, Hash, fragment_metadata_is_valid, validate_fragment_reference_list,
};

#[unsafe(no_mangle)]
pub extern "C" fn lore_alloc(len: usize) -> *mut u8 {
    Box::into_raw(vec![0u8; len].into_boxed_slice()) as *mut u8
}

/// # Safety
/// `ptr` and `len` must identify a live allocation returned by `lore_alloc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lore_free(ptr: *mut u8, len: usize) {
    unsafe { drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len))) };
}

#[unsafe(no_mangle)]
pub extern "C" fn lore_fragment_metadata_valid(flags: u32, payload: u32, content: u64) -> u32 {
    u32::from(fragment_metadata_is_valid(&Fragment {
        flags,
        size_payload: payload,
        size_content: content,
    }))
}

/// # Safety
/// `ptr` must identify `len` readable bytes in the module's memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lore_fragment_list_valid(ptr: *const u8, len: usize, content: u64) -> u32 {
    let payload = unsafe { std::slice::from_raw_parts(ptr, len) };
    let Ok(size_payload) = u32::try_from(len) else {
        return 0;
    };
    let fragment = Fragment {
        flags: 1,
        size_payload,
        size_content: content,
    };
    u32::from(validate_fragment_reference_list(&fragment, payload).is_ok())
}

/// # Safety
/// `ptr` must identify `len` readable bytes and `out` 32 writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lore_hash(ptr: *const u8, len: usize, out: *mut u8) {
    let payload = unsafe { std::slice::from_raw_parts(ptr, len) };
    let hash = Hash::hash_buffer(payload);
    unsafe { std::ptr::copy_nonoverlapping(hash.data().as_ptr(), out, 32) };
}

/// # Safety
/// `ptr` must identify `len` readable bytes in the module's memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lore_metadata_valid(ptr: *const u8, len: usize) -> u32 {
    let buffer = unsafe { std::slice::from_raw_parts(ptr, len) };
    u32::from(lore_base::metadata::check_buffer(buffer).is_ok())
}

#[unsafe(no_mangle)]
pub extern "C" fn lore_metadata_new() -> *mut BytesMut {
    Box::into_raw(Box::new(BytesMut::new()))
}

/// # Safety
/// The handle must be a live handle returned by `lore_metadata_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lore_metadata_free(handle: *mut BytesMut) {
    unsafe { drop(Box::from_raw(handle)) };
}

/// # Safety
/// The handle must be live and the key and value pointers must refer to readable allocations.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lore_metadata_set(
    handle: *mut BytesMut,
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
    tag: u32,
) -> u32 {
    let Some(size) = 20usize
        .checked_add(key_len)
        .and_then(|n| n.checked_add(value_len))
    else {
        return 0;
    };
    if size > lore_base::metadata::METADATA_MAX_SIZE {
        return 0;
    }
    let buffer = unsafe { &mut *handle };
    let key = unsafe { std::slice::from_raw_parts(key, key_len) };
    let value = unsafe { std::slice::from_raw_parts(value, value_len) };
    u32::from(lore_base::metadata::set_encoded(buffer, key, value, tag).is_ok())
}

/// # Safety
/// The handle must be live. The pointer is valid only until the next mutation or free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lore_metadata_data(handle: *const BytesMut) -> *const u8 {
    unsafe { (&*handle).as_ptr() }
}

/// # Safety
/// The handle must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lore_metadata_len(handle: *const BytesMut) -> usize {
    unsafe { (&*handle).len() }
}
