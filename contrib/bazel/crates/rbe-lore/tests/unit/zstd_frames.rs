// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use rbe_lore::zstd_frames::*;

fn zstd_leaf(content: &[u8]) -> (Fragment, Vec<u8>) {
    let payload = zstd::bulk::compress(content, 6).unwrap();
    let fragment = Fragment {
        flags: FragmentFlags::PayloadCompressedZstd.bits(),
        size_payload: payload.len() as u32,
        size_content: content.len() as u64,
    };
    (fragment, payload)
}

fn raw_leaf(content: &[u8]) -> Fragment {
    Fragment {
        flags: 0,
        size_payload: content.len() as u32,
        size_content: content.len() as u64,
    }
}

/// A zstd leaf is passed on byte for byte, and leaves of both forms, empty content and content
/// spanning several raw blocks included, concatenate into one stream libzstd decodes.
#[test]
fn leaves_of_either_form_make_one_stream() {
    let compressible: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let incompressible: Vec<u8> = (0..300_000u64)
        .map(|i| (i.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 56) as u8)
        .collect();
    let (fragment, payload) = zstd_leaf(&compressible);

    let mut stream = Vec::new();
    append_leaf(&mut stream, &fragment, &payload).unwrap();
    assert_eq!(stream, payload, "a zstd leaf goes out as stored");
    append_leaf(&mut stream, &raw_leaf(&incompressible), &incompressible).unwrap();
    append_leaf(&mut stream, &raw_leaf(&[]), &[]).unwrap();

    let expected = [compressible, incompressible].concat();
    let decoded = zstd::stream::decode_all(stream.as_slice()).unwrap();
    assert_eq!(decoded, expected);
    assert!(
        zstd::stream::decode_all(
            &{
                let mut empty = Vec::new();
                append_raw_frame(&mut empty, &[]);
                empty
            }[..]
        )
        .unwrap()
        .is_empty()
    );
}

/// A leaf in a form other than zstd or uncompressed, or one that is not what its fragment
/// says, is refused.
#[test]
fn leaves_lore_would_not_deliver_are_refused() {
    let content = vec![7u8; 1000];
    let (fragment, payload) = zstd_leaf(&content);
    let mut stream = Vec::new();

    let lz4 = Fragment {
        flags: FragmentFlags::PayloadCompressedLZ4.bits(),
        ..fragment
    };
    assert!(append_leaf(&mut stream, &lz4, &payload).is_err());
    assert!(append_leaf(&mut stream, &fragment, &payload[1..]).is_err());
    assert!(append_leaf(&mut stream, &fragment, &content[..payload.len()]).is_err());
    assert!(append_leaf(&mut stream, &raw_leaf(&content), &content[1..]).is_err());
    assert!(stream.is_empty(), "nothing is appended for a refused leaf");
}
