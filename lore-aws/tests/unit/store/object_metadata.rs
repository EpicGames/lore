// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_aws::aws::item::ByteStr;
use lore_aws::aws::s3::ObjectMetadata;
use lore_aws::store::object_metadata::*;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;

/// The metadata an object would carry for `value` under the lore key.
fn metadata(value: &str) -> ObjectMetadata {
    ObjectMetadata::from([(
        ByteStr::from_static(KEY_FRAGMENT),
        ByteStr::copy_from_str(value),
    )])
}

fn fragment() -> Fragment {
    Fragment {
        flags: FragmentFlags::PayloadCompressedZstd.bits(),
        size_payload: 4096,
        size_content: 16384,
    }
}

#[test]
fn round_trips_a_fragment() {
    let encoded = to_object_metadata(&fragment());

    assert_eq!(from_object_metadata(&encoded), Ok(fragment()));
}

#[test]
fn the_encoded_value_is_hex_flags_and_decimal_sizes_and_decodes_back() {
    let mut buffer = [0u8; ENCODED_CAPACITY];
    let encoded = encode(&fragment(), &mut buffer);

    assert_eq!(encoded, "8:4096:16384");
    assert_eq!(decode(encoded), Ok(fragment()));
}

#[test]
fn the_largest_values_fit_the_encoding_buffer() {
    let extreme = Fragment {
        flags: PAYLOAD_FLAGS,
        size_payload: u32::MAX,
        size_content: u64::MAX,
    };
    let mut buffer = [0u8; ENCODED_CAPACITY];
    let encoded = encode(&extreme, &mut buffer);

    assert_eq!(
        encoded,
        format!("{PAYLOAD_FLAGS:x}:{}:{}", u32::MAX, u64::MAX)
    );
    assert_eq!(decode(encoded), Ok(extreme));
}

#[test]
fn writes_one_key_holding_hex_flags_and_decimal_sizes() {
    let encoded = to_object_metadata(&fragment());

    assert_eq!(encoded.len(), 1, "one key, not one per field");
    assert_eq!(encoded.get(KEY_FRAGMENT).unwrap(), "8:4096:16384");
}

#[test]
fn round_trips_the_extreme_values() {
    let extreme = Fragment {
        flags: PAYLOAD_FLAGS,
        size_payload: u32::MAX,
        size_content: u64::MAX,
    };
    let encoded = to_object_metadata(&extreme);

    assert_eq!(from_object_metadata(&encoded), Ok(extreme));
}

#[test]
fn keeps_every_flag_that_describes_the_payload() {
    for flag in [
        FragmentFlags::PayloadFragmented,
        FragmentFlags::PayloadCompressedLZ4,
        FragmentFlags::PayloadCompressedOodle2,
        FragmentFlags::PayloadCompressedZstd,
        FragmentFlags::PayloadRevisionState,
    ] {
        let carried = Fragment {
            flags: flag.bits(),
            ..fragment()
        };

        assert_eq!(
            from_object_metadata(&to_object_metadata(&carried)),
            Ok(carried),
            "{flag:?} describes the payload and must travel on the object"
        );
    }
}

#[test]
fn drops_state_store_location_and_per_machine_flags() {
    for flag in [
        FragmentFlags::PayloadObliterating,
        FragmentFlags::PayloadObliterated,
        FragmentFlags::PayloadStoredDurable,
        FragmentFlags::PayloadStoredLocal,
        FragmentFlags::PayloadLocalCachePriority,
        FragmentFlags::PayloadDoNotReplicate,
    ] {
        let mut tainted = fragment();
        tainted.flags |= flag.bits();

        assert_eq!(
            from_object_metadata(&to_object_metadata(&tainted)),
            Ok(fragment()),
            "{flag:?} is not a property of the payload and must not travel on the object"
        );
    }
}

#[test]
fn masks_flags_that_reached_the_object_some_other_way() {
    let encoded = metadata(&format!("{:x}:1:1", u32::MAX));

    let recovered = from_object_metadata(&encoded).unwrap();

    assert_eq!(recovered.flags, PAYLOAD_FLAGS);
}

#[test]
fn reports_an_object_with_no_metadata_as_absent() {
    assert_eq!(
        from_object_metadata(&ObjectMetadata::new()),
        Err(ObjectMetadataError::Absent)
    );
    assert_eq!(
        from_object_metadata(&ObjectMetadata::from([(
            ByteStr::from_static("unrelated"),
            ByteStr::from_static("8:1:1"),
        )])),
        Err(ObjectMetadataError::Absent)
    );
}

#[test]
fn reports_the_wrong_number_of_fields_as_malformed() {
    for value in ["", "8", "8:1", "8:1:1:1"] {
        let encoded = metadata(value);

        assert_eq!(
            from_object_metadata(&encoded),
            Err(ObjectMetadataError::Malformed(FIELD_COUNT)),
            "{value:?} does not hold three fields"
        );
    }
}

#[test]
fn names_the_field_that_failed_to_parse() {
    for (value, field) in [
        ("zz:1:1", FIELD_FLAGS),
        ("8:nope:1", FIELD_SIZE_PAYLOAD),
        ("8:1:nope", FIELD_SIZE_CONTENT),
    ] {
        let encoded = metadata(value);

        assert_eq!(
            from_object_metadata(&encoded),
            Err(ObjectMetadataError::Malformed(field))
        );
    }
}

#[test]
fn reports_an_overflowing_value_as_malformed() {
    let encoded = metadata(&format!("8:{}:1", u64::from(u32::MAX) + 1));

    assert_eq!(
        from_object_metadata(&encoded),
        Err(ObjectMetadataError::Malformed(FIELD_SIZE_PAYLOAD))
    );
}

/// Decimal flags would silently decode as a different bit pattern for any value above 9, so
/// the base is part of the format rather than an incidental choice.
#[test]
fn reads_flags_as_hex() {
    let encoded = metadata("a:1:1");

    assert_eq!(
        from_object_metadata(&encoded).unwrap().flags,
        0xa & PAYLOAD_FLAGS
    );
}
