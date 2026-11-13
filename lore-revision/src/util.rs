// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

pub mod collect_stream;
pub mod config;
pub mod encoding;
pub mod fan_out;
pub mod fs;
pub mod inflight;
pub mod path;
pub mod request_tracker;
pub mod serde;
pub mod task_queue;
pub mod time;
pub mod url;

/// Provides a mechanism for converting data to a hex `&str` without unnecessary allocations.
/// Note: the `dst` parameter must be passed in in order to give us something to which we can tie
///     the lifetime of the returned &str.
#[inline]
pub fn to_hex_str<'a>(src: &[u8], dst: &'a mut [u8]) -> &'a str {
    debug_assert_eq!(dst.len(), src.len() * 2);

    // These should never fail, but if it does, we'd rather panic than use a default value
    hex::encode_to_slice(src, dst).expect("hex encode failed");
    std::str::from_utf8(dst).expect("hex was not valid utf8")
}

/// The decimal digits of `value`, as `value.to_string()` writes them, written to the end of `dst`,
/// which holds the 20 digits of `u64::MAX`, without an allocation.
#[lore_macro::test_pub]
#[inline]
pub(crate) fn to_decimal_str(value: u64, dst: &mut [u8; 20]) -> &str {
    let mut start = dst.len();
    let mut rest = value;
    loop {
        start -= 1;
        dst[start] = b'0' + (rest % 10) as u8;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    std::str::from_utf8(&dst[start..]).unwrap_or_default()
}
