// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::types::Address;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_base::types::Hash;
use lore_base::types::validate_fragment_response;

// A scalar ABI avoids introducing wasm-bindgen into this narrowly scoped probe.
#[unsafe(no_mangle)]
pub extern "C" fn compression_code(flags: u32) -> u32 {
    match FragmentFlags::compression_label(flags) {
        "not_compressed" => 0,
        "lz4" => 1,
        "oodle2" => 2,
        "zstd" => 3,
        "unknown" => 4,
        _ => unreachable!(),
    }
}

// Return a bit for each failed domain operation, so the Workerd probe verifies
// more than compilation and can distinguish errors without sharing Wasm memory.
#[unsafe(no_mangle)]
pub extern "C" fn domain_check() -> u32 {
    let mut failures = 0;
    let digest = Hash::hash_buffer(b"abc");
    if digest.to_string() != "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85" {
        failures |= 1;
    }
    let address = format!("{digest}-00112233445566778899aabbccddeeff");
    if address.parse::<Address>().map(|value| value.to_string()) != Ok(address) {
        failures |= 2;
    }
    let fragment = Fragment {
        flags: 0,
        size_payload: 3,
        size_content: 3,
    };
    if validate_fragment_response(&fragment).is_err() {
        failures |= 4;
    }
    let malformed = Fragment {
        flags: (FragmentFlags::PayloadCompressedLZ4 | FragmentFlags::PayloadFragmented).bits(),
        ..fragment
    };
    if validate_fragment_response(&malformed).is_ok() {
        failures |= 8;
    }
    // A u64 size must not wrap through the 32-bit usize in Workers Wasm.
    let oversized = Fragment {
        size_content: 1_u64 << 32,
        ..fragment
    };
    if validate_fragment_response(&oversized).is_ok() {
        failures |= 16;
    }
    failures
}

#[cfg(test)]
mod tests {
    #[test]
    fn portable_domain_operations() {
        assert_eq!(super::domain_check(), 0);
    }
}
