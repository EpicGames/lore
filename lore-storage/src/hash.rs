// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use core::mem::MaybeUninit;

use crate::FragmentFlags;
use crate::compress::FRAGMENT_SIZE_THRESHOLD;
use crate::compress::FragmentError;
use crate::compress::decompress;
use crate::types::Fragment;
use crate::types::Hash;

/// Hash a function name with a domain salt prefix.
pub fn hash_function(salt: &[u8], function: &str) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(salt);
    hasher.update(function.as_bytes());
    hasher.finalize().as_bytes().into()
}

/// Hash a function name with a domain salt prefix and a single byte-slice argument.
pub fn hash_function_arg_slice(salt: &[u8], function: &str, arg: &[u8]) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(salt);
    hasher.update(function.as_bytes());
    hasher.update(arg);
    hasher.finalize().as_bytes().into()
}

/// Hash a function name with a domain salt prefix and a single string argument.
pub fn hash_function_arg(salt: &[u8], function: &str, arg: &str) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(salt);
    hasher.update(function.as_bytes());
    hasher.update(arg.as_bytes());
    hasher.finalize().as_bytes().into()
}

/// Hash a function name with a domain salt prefix and two string arguments.
pub fn hash_function_args(salt: &[u8], function: &str, first_arg: &str, second_arg: &str) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(salt);
    hasher.update(function.as_bytes());
    hasher.update(first_arg.as_bytes());
    hasher.update(second_arg.as_bytes());
    hasher.finalize().as_bytes().into()
}

/// Hash a function name with a domain salt prefix and two byte-slice arguments.
pub fn hash_function_args_slice(
    salt: &[u8],
    function: &str,
    first_arg: &[u8],
    second_arg: &[u8],
) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(salt);
    hasher.update(function.as_bytes());
    hasher.update(first_arg);
    hasher.update(second_arg);
    hasher.finalize().as_bytes().into()
}

/// Hash a function name with a domain salt prefix and a variable number of string arguments.
pub fn hash_function_strs_slice(salt: &[u8], function: &str, args: &[&str]) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(salt);
    hasher.update(function.as_bytes());
    for arg in args {
        hasher.update(arg.as_bytes());
    }
    hasher.finalize().as_bytes().into()
}

/// Hash a raw data slice using blake3.
pub fn hash_slice(data: &[u8]) -> Hash {
    blake3::hash(data).as_bytes().into()
}

/// Hash a fragment's content if it matches the payload metadata, decompressing first if needed
pub fn hash_fragment(fragment: Fragment, data: &[u8]) -> Result<Hash, FragmentError> {
    if fragment.size_payload as usize != data.len() {
        return Err(FragmentError::internal(
            "Invalid payload size for fragment hash",
        ));
    }

    if (fragment.flags & FragmentFlags::PayloadCompressed) == 0 {
        return Ok(hash_slice(data));
    }

    debug_assert!((fragment.flags & FragmentFlags::PayloadFragmented) == 0);
    debug_assert!(fragment.size_content as usize <= FRAGMENT_SIZE_THRESHOLD);

    let (_, decompressed) = decompress(fragment, data)?;

    if fragment.size_content as usize != decompressed.len() {
        return Err(FragmentError::internal(
            "Invalid content size for fragment hash after decompression",
        ));
    }

    Ok(hash_slice(decompressed.as_ref()))
}

/// 64-bit string hash type, used for node name lookups.
pub type StringHash = u64;

/// Longest name [`hash_string`] folds without allocating, one cache line wide.
const HASH_STRING_STACK_BYTES: usize = 64;

/// Compute the 64-bit xxh3 hash of the lowercase form of a string.
///
/// Names up to [`HASH_STRING_STACK_BYTES`] fold in one branchless pass over a
/// stack buffer; the high bits it accumulates say whether the name is ASCII and
/// the fold usable. Testing per byte instead would stop the pass vectorizing.
pub fn hash_string(string: &str) -> StringHash {
    let bytes = string.as_bytes();
    if bytes.len() <= HASH_STRING_STACK_BYTES {
        let mut buffer = [const { MaybeUninit::<u8>::uninit() }; HASH_STRING_STACK_BYTES];
        let mut high_bits = 0u8;
        for (target, &source) in buffer.iter_mut().zip(bytes) {
            high_bits |= source;
            target.write(source.to_ascii_lowercase());
        }
        if high_bits & 0x80 == 0 {
            // SAFETY: the loop above wrote every byte of `buffer[..bytes.len()]`.
            return xxhash_rust::xxh3::xxh3_64(unsafe { buffer[..bytes.len()].assume_init_ref() });
        }
    }
    xxhash_rust::xxh3::xxh3_64(string.to_lowercase().as_bytes())
}

/// Zero-alloc xxh3 of raw string-like bytes (same digest family as [`hash_string`] without the lowercasing, distinct from the blake3 [`hash_slice`]).
pub fn hash_string_bytes(bytes: &[u8]) -> StringHash {
    xxhash_rust::xxh3::xxh3_64(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Unicode fold both paths of [`hash_string`] must agree with. Node
    /// names store the digest and lookups match on it, so it cannot move.
    fn hash_string_reference(string: &str) -> StringHash {
        xxhash_rust::xxh3::xxh3_64(string.to_lowercase().as_bytes())
    }

    #[test]
    fn hash_string_matches_the_unicode_fold() {
        let long_ascii = "a-name-that-does-not-fit-the-stack-buffer-".repeat(10);
        let long_unicode = "\u{00e4}-name-that-does-not-fit-the-stack-buffer-".repeat(10);
        for string in [
            "",
            "a",
            "A",
            "rock.mesh",
            "Rock.mesh",
            "ROCK.MESH",
            "MiXeD.CaSe-123_x",
            "0123456789 !@#$%^&*()",
            // Above ASCII: length-changing and context-dependent folds.
            "\u{00c4}pfel",
            "Stra\u{00df}e",
            "\u{0130}stanbul",
            "\u{03a3}\u{03bf}\u{03c6}\u{03bf}\u{03c2}",
            // A capital sigma ending a word, the fold that depends on position.
            "\u{039f}\u{0394}\u{039f}\u{03a3}",
            "\u{4f60}\u{597d}",
            &long_ascii,
            &long_unicode,
        ] {
            assert_eq!(
                hash_string(string),
                hash_string_reference(string),
                "{string:?} does not match the Unicode fold"
            );
        }
    }

    /// Exhaustive over one- and two-character ASCII, plus a sample of the code
    /// points above it.
    #[test]
    fn hash_string_matches_the_unicode_fold_across_ascii() {
        for first in 0u8..128 {
            let single = String::from_utf8(vec![first]).expect("ascii is utf-8");
            assert_eq!(
                hash_string(&single),
                hash_string_reference(&single),
                "{single:?} does not match the Unicode fold"
            );
            for second in 0u8..128 {
                let pair = String::from_utf8(vec![first, second]).expect("ascii is utf-8");
                assert_eq!(
                    hash_string(&pair),
                    hash_string_reference(&pair),
                    "{pair:?} does not match the Unicode fold"
                );
            }
        }

        for code in (0u32..0x3000).step_by(7) {
            let Some(character) = char::from_u32(code) else {
                continue;
            };
            let alone = character.to_string();
            let after_ascii = format!("A{character}");
            for string in [&alone, &after_ascii] {
                assert_eq!(
                    hash_string(string),
                    hash_string_reference(string),
                    "{string:?} does not match the Unicode fold"
                );
            }
        }
    }

    /// Pins the digests themselves. Agreement between the two paths says
    /// nothing if xxh3 moves underneath both, which would silently void every
    /// stored name lookup.
    #[test]
    fn hash_string_digests_are_the_ones_already_stored() {
        for (string, digest) in [
            ("", 0x2d06_8005_38d3_94c2u64),
            ("a", 0xe6c6_32b6_1e96_4e1f),
            ("Rock.mesh", 0x1e82_9b1c_9417_add2),
            ("ROCK.MESH", 0x1e82_9b1c_9417_add2),
            ("MiXeD.CaSe-123_x", 0xd03b_4303_99ba_f75a),
            ("Stra\u{00df}e", 0x862b_1b43_f409_32bc),
            ("\u{0130}stanbul", 0xf1c9_09f6_e5c8_2711),
            ("\u{4f60}\u{597d}", 0xad90_5e65_cd72_90f0),
        ] {
            assert_eq!(hash_string(string), digest, "{string:?} moved");
        }
    }

    /// The paths meet at the buffer boundary and on either side of it.
    #[test]
    fn hash_string_agrees_across_the_stack_buffer_boundary() {
        for length in [
            HASH_STRING_STACK_BYTES - 1,
            HASH_STRING_STACK_BYTES,
            HASH_STRING_STACK_BYTES + 1,
        ] {
            let string = "Aa".repeat(length).split_at(length).0.to_string();
            assert_eq!(string.len(), length);
            assert_eq!(hash_string(&string), hash_string_reference(&string));
        }
    }

    #[test]
    fn hash_fragment_uncompressed_ok() {
        let data = b"hello world";
        let fragment = Fragment {
            flags: 0,
            size_payload: data.len() as u32,
            size_content: data.len() as u64,
        };
        let result = hash_fragment(fragment, data);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), hash_slice(data));
    }

    #[test]
    fn hash_fragment_uncompressed_deterministic() {
        let data = b"deterministic content";
        let fragment = Fragment {
            flags: 0,
            size_payload: data.len() as u32,
            size_content: data.len() as u64,
        };
        let hash1 = hash_fragment(fragment, data).unwrap();
        let hash2 = hash_fragment(fragment, data).unwrap();
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn hash_fragment_different_data_different_hash() {
        let data_a = b"content a";
        let data_b = b"content b";
        let frag_a = Fragment {
            flags: 0,
            size_payload: data_a.len() as u32,
            size_content: data_a.len() as u64,
        };
        let frag_b = Fragment {
            flags: 0,
            size_payload: data_b.len() as u32,
            size_content: data_b.len() as u64,
        };
        assert_ne!(
            hash_fragment(frag_a, data_a).unwrap(),
            hash_fragment(frag_b, data_b).unwrap()
        );
    }

    #[test]
    fn hash_fragment_payload_size_mismatch() {
        let data = b"hello";
        let fragment = Fragment {
            flags: 0,
            size_payload: data.len() as u32 + 1,
            size_content: data.len() as u64,
        };
        assert!(hash_fragment(fragment, data).is_err());
    }

    #[test]
    fn hash_fragment_empty_payload() {
        let data: &[u8] = b"";
        let fragment = Fragment {
            flags: 0,
            size_payload: 0,
            size_content: 0,
        };
        let result = hash_fragment(fragment, data);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), hash_slice(data));
    }

    #[test]
    fn hash_fragment_compressed_payload_size_mismatch() {
        let data = b"short";
        let fragment = Fragment {
            flags: crate::FragmentFlags::PayloadCompressedLZ4.into(),
            size_payload: data.len() as u32 + 5,
            size_content: 100,
        };
        assert!(hash_fragment(fragment, data).is_err());
    }

    #[test]
    fn hash_fragment_compressed_invalid_data() {
        let data = b"this is not valid lz4 compressed data!!";
        let fragment = Fragment {
            flags: crate::FragmentFlags::PayloadCompressedLZ4.into(),
            size_payload: data.len() as u32,
            size_content: 100,
        };
        assert!(hash_fragment(fragment, data).is_err());
    }

    #[test]
    fn hash_function_different_salts_produce_different_keys() {
        let hash_urc = hash_function(b"urc", "test_function");
        let hash_lore = hash_function(b"lore", "test_function");
        assert_ne!(hash_urc, hash_lore);
    }

    #[test]
    fn hash_function_same_salt_is_deterministic() {
        let hash1 = hash_function(b"urc", "test_function");
        let hash2 = hash_function(b"urc", "test_function");
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn hash_function_arg_with_salt() {
        let hash_urc = hash_function_arg(b"urc", "func", "arg");
        let hash_lore = hash_function_arg(b"lore", "func", "arg");
        assert_ne!(hash_urc, hash_lore);
    }

    #[test]
    fn hash_function_compressed_roundtrip() {
        let original = vec![0u8; 4096];
        let original = original.as_slice();
        let uncompressed_fragment = Fragment {
            flags: 0,
            size_payload: original.len() as u32,
            size_content: original.len() as u64,
        };
        let (compressed_fragment, compressed_data) =
            crate::compress::compress(uncompressed_fragment, original, crate::CompressionMode::Lz4)
                .unwrap();
        let hash = hash_fragment(compressed_fragment, compressed_data.as_ref()).unwrap();
        assert_eq!(hash, hash_slice(original));
    }
}
