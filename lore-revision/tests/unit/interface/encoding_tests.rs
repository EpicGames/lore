// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::event::LoreBytes;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreBinary;
use lore_revision::interface::LoreString;

/// Text crosses as its bytes, empty and not UTF-8 alike: the service checks it where it runs the
/// call. An empty string comes back as the null pointer the type documents.
#[test]
fn a_string_round_trips_its_bytes() {
    for bytes in [&b""[..], b"doc.md", b"a\xff\xfeb"] {
        let decoded: LoreString = bitcode::decode(&bitcode::encode(&LoreString::from_bytes(bytes)))
            .expect("a string must decode");
        assert_eq!(decoded.as_bytes(), bytes);
        assert_eq!(decoded.string.is_null(), bytes.is_empty());
    }
}

#[test]
fn a_binary_block_round_trips_its_bytes() {
    for bytes in [&b""[..], b"raw\x00bytes"] {
        let decoded: LoreBinary = bitcode::decode(&bitcode::encode(&LoreBinary::from_bytes(bytes)))
            .expect("a block must decode");
        assert_eq!(decoded.as_bytes(), bytes);
        assert_eq!(decoded.payload.is_null(), bytes.is_empty());
    }
}

/// A decoded view points into the input it was decoded from, not at a copy.
#[test]
fn a_decoded_view_points_into_its_input() {
    let contents = b"payload bytes";
    let encoded = bitcode::encode(&LoreBytes {
        ptr: contents.as_ptr().cast(),
        len: contents.len(),
    });

    let decoded: LoreBytes = bitcode::decode(&encoded).expect("a view must decode");

    assert!(encoded.as_ptr_range().contains(&decoded.ptr.cast()));
    // SAFETY: `encoded` is alive, and the view points into it.
    assert_eq!(unsafe { decoded.as_slice() }, contents);
}

/// The strings of an array share one column, so each has to come back with its own length.
#[test]
fn an_array_of_strings_round_trips() {
    let strings = LoreArray::from_vec(vec![
        LoreString::from_str("first"),
        LoreString::default(),
        LoreString::from_str("third"),
    ]);

    let decoded: LoreArray<LoreString> =
        bitcode::decode(&bitcode::encode(&strings)).expect("an array must decode");

    assert_eq!(decoded.as_slice(), strings.as_slice());
}

/// Input that ends before the bytes its lengths describe fails the decode instead of reading past
/// the end.
#[test]
fn a_truncated_column_fails_to_decode() {
    let encoded = bitcode::encode(&LoreString::from_str("doc.md"));
    for end in 0..encoded.len() {
        assert!(
            bitcode::decode::<LoreString>(&encoded[..end]).is_err(),
            "{end} of {} bytes must not decode",
            encoded.len()
        );
    }

    let mut too_long = u64::MAX.to_le_bytes().to_vec();
    too_long.extend_from_slice(b"doc.md");
    assert!(bitcode::decode::<LoreString>(&too_long).is_err());
}

/// An array built from a boxed slice takes over its allocation rather than copying the elements.
#[test]
fn an_array_takes_over_a_boxed_slice() {
    let boxed: Box<[u32]> = vec![1, 2, 3].into_boxed_slice();
    let elements = boxed.as_ptr();

    let array = LoreArray::from(boxed);

    assert_eq!(array.as_slice().as_ptr(), elements);
    assert_eq!(array.as_slice(), [1, 2, 3]);
}
