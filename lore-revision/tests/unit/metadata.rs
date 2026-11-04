// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::metadata::*;

#[test]
fn metadata_wire_format_and_replacement_order_are_stable() {
    let mut metadata = Metadata::new();
    metadata
        .set_typed("a", b"one", MetadataType::String)
        .unwrap();
    metadata
        .set_typed("b", &[1], MetadataType::Boolean)
        .unwrap();
    metadata
        .set_typed("a", b"two", MetadataType::Binary)
        .unwrap();
    let expected = [
        0x61, 0x74, 0x65, 0x6d, 1, 0, 0, 0, 1, 0, 0, 0, 3, 0, 0, 0, 255, 0, 0, 0, b'a', b't', b'w',
        b'o', 1, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, b'b', 1,
    ];
    if cfg!(target_endian = "little") {
        assert_eq!(&metadata.buffer[..], expected);
    }
    metadata
        .set_typed("a", b"longer", MetadataType::String)
        .unwrap();
    let mut rewritten = Metadata::new();
    rewritten
        .set_typed("b", &[1], MetadataType::Boolean)
        .unwrap();
    rewritten
        .set_typed("a", b"longer", MetadataType::String)
        .unwrap();
    assert_eq!(metadata.buffer, rewritten.buffer);
}

/// A number that names no type is refused rather than defaulted: a buffer
/// carrying one was written by something that disagrees with this build
/// about what tags mean, and guessing would hand the caller a value of the
/// wrong type instead of saying the metadata is unreadable. That every tag
/// decodes to its own type needs no assertion — the decoder is written from
/// the same discriminants.
#[test]
fn a_tag_that_names_no_type_is_refused() {
    for unknown in [0u32, 7, 254, 256, u32::MAX] {
        assert!(
            MetadataType::try_from(unknown).is_err(),
            "{unknown} is not a type and must be refused, not defaulted"
        );
    }
}

/// `set_typed` is the only setter that carries the type tag separately from
/// the value, so the tag has to survive to the read rather than being
/// implied by a Rust type. Binary is the case the typed getters cannot
/// express, which is why it is the one that matters.
#[test]
fn set_typed_round_trips_every_type_tag() {
    let cases: [(&str, &[u8], MetadataType); 4] = [
        ("text", b"hello", MetadataType::String),
        ("count", &42u64.to_le_bytes(), MetadataType::Numeric),
        ("flag", &[1u8], MetadataType::Boolean),
        ("blob", &[0xde, 0xad, 0xbe, 0xef], MetadataType::Binary),
    ];

    let mut metadata = Metadata::new();
    for (key, value, value_type) in cases {
        metadata.set_typed(key, value, value_type).unwrap();
    }
    for (key, value, value_type) in cases {
        let (read_value, read_type) = metadata.get_typed(key).unwrap();
        assert_eq!(read_value, value, "value for {key}");
        assert_eq!(read_type, value_type, "type tag for {key}");
    }
}

/// Overwrite the stored kind of the entry at `entry_index` with a number no
/// build knows, standing in for metadata written by something newer. No
/// setter can express this: they all take a [`MetadataType`].
fn plant_unknown_tag(metadata: &mut Metadata, entries: &[(&str, &str)], entry_index: usize) {
    let header_size = std::mem::size_of::<u32>() * 2;
    let item_size = std::mem::size_of::<u32>() * 3;
    let item_start = entries[..entry_index]
        .iter()
        .fold(header_size, |offset, (key, value)| {
            offset + item_size + key.len() + value.len()
        });
    let tag = item_start + std::mem::size_of::<u32>() * 2;
    metadata.buffer[tag..tag + std::mem::size_of::<u32>()].copy_from_slice(&99u32.to_ne_bytes());
}

fn metadata_of(entries: &[(&str, &str)]) -> Metadata {
    let mut metadata = Metadata::new();
    for (key, value) in entries {
        metadata
            .set_typed(key, value.as_bytes(), MetadataType::String)
            .unwrap();
    }
    metadata
}

/// Every accessor trusts the stored lengths, so a blob arriving from the
/// store has to be shown to stay inside itself before one reads it. A
/// truncated blob and a forged length are the two ways it would not.
#[test]
fn a_buffer_whose_entries_leave_it_is_refused() {
    let entries = [("key", "value")];
    let sound = metadata_of(&entries);
    sound
        .check_buffer()
        .expect("a buffer written here must pass");

    for cut in 1..sound.buffer.len() - std::mem::size_of::<u32>() * 2 {
        let mut truncated = sound.clone();
        truncated.buffer.truncate(sound.buffer.len() - cut);
        assert!(
            truncated.check_buffer().is_err(),
            "a blob cut {cut} bytes short must be refused"
        );
    }

    let mut forged = sound.clone();
    let length = std::mem::size_of::<u32>() * 2 + std::mem::size_of::<u32>();
    forged.buffer[length..length + std::mem::size_of::<u32>()]
        .copy_from_slice(&u32::MAX.to_ne_bytes());
    assert!(
        forged.check_buffer().is_err(),
        "a value length reaching past the blob must be refused"
    );
}

/// An entry bigger than the whole metadata buffer may hold can never be
/// committed, so it is refused where it is written rather than recorded and
/// failed later. It also cannot be stored honestly: the per-entry header
/// records lengths as `u32`, so an entry past that would read back short and
/// throw off every entry after it.
///
/// The bound is exact at the byte, because the band either side of it is
/// where a guard that forgot the buffer's own header would accept a pair no
/// revision could ever serialize.
#[test]
fn set_refuses_an_entry_larger_than_the_whole_cap() {
    let key = "blob";
    let framing = METADATA_MAX_SIZE - Metadata::stored_size(key.len(), 0);
    let largest = vec![0u8; framing];

    let mut metadata = Metadata::new();
    assert!(
        metadata.set_binary(key, &largest).is_ok(),
        "the largest pair that fits must be accepted"
    );

    let mut metadata = Metadata::new();
    assert!(
        metadata
            .set_binary(key, &[largest.as_slice(), &[0u8]].concat())
            .is_err(),
        "one byte more than fits must be refused"
    );
    assert!(
        metadata.is_empty(),
        "a refused entry must leave the buffer untouched"
    );
}

/// An entry this build cannot type must not cost the caller the entries
/// around it: a walk that stopped there would hand back a prefix, and every
/// caller that ignores the outcome would read it as the whole buffer.
#[test]
fn walk_passes_over_an_entry_it_cannot_type() {
    let entries = [("first", "one"), ("second", "two"), ("third", "three")];
    let mut metadata = metadata_of(&entries);
    plant_unknown_tag(&mut metadata, &entries, 1);

    let mut keys: Vec<Vec<u8>> = Vec::new();
    metadata.walk(|key, _, _| keys.push(key.to_vec()));
    assert_eq!(
        keys,
        vec![b"first".to_vec(), b"third".to_vec()],
        "the entries either side of an unreadable one must still be visited"
    );
}

/// A key that is not stored and a key stored under a kind this build does
/// not know both fail the read, and a caller has to be able to tell them
/// apart: the first means the key is simply not there, the second that the
/// metadata was written by something this build disagrees with.
#[test]
fn an_unknown_tag_fails_differently_from_a_missing_key() {
    let entries = [("key", "value")];
    let mut metadata = metadata_of(&entries);

    assert!(
        matches!(
            metadata.get_typed("absent"),
            Err(MetadataError::FileNotFound(_))
        ),
        "a key that was never stored is not found"
    );

    plant_unknown_tag(&mut metadata, &entries, 0);

    let error = metadata
        .get_typed("key")
        .expect_err("a tag this build does not know must not decode");
    assert!(
        !matches!(error, MetadataError::FileNotFound(_)),
        "an unreadable kind must not be reported as a key that is not there"
    );
}

/// A value the same length as the one it replaces is written in place
/// rather than erased and re-appended, and that path has to carry the tag
/// too — every type has a length it shares with a binary value of the same
/// size, so a tag left behind reads the new bytes as the old type.
#[test]
fn set_typed_retypes_a_key_whose_value_is_the_same_length() {
    let mut metadata = Metadata::new();
    metadata
        .set_typed("key", b"hello", MetadataType::String)
        .unwrap();
    metadata
        .set_typed("key", &[0xffu8; 5], MetadataType::Binary)
        .unwrap();

    let (value, value_type) = metadata.get_typed("key").unwrap();
    assert_eq!(value, &[0xffu8; 5]);
    assert_eq!(value_type, MetadataType::Binary);
}

/// Re-setting a key replaces the value and its tag, which is what makes a
/// batch of sets a compressed sequence rather than an error.
#[test]
fn set_typed_overwrites_a_key_and_its_type() {
    let mut metadata = Metadata::new();
    metadata
        .set_typed("key", b"first", MetadataType::String)
        .unwrap();
    metadata
        .set_typed("key", &7u64.to_le_bytes(), MetadataType::Numeric)
        .unwrap();

    let (value, value_type) = metadata.get_typed("key").unwrap();
    assert_eq!(Metadata::to_u64(value).unwrap(), 7);
    assert_eq!(value_type, MetadataType::Numeric);

    let mut keys = 0;
    metadata.walk(|_, _, _| keys += 1);
    assert_eq!(keys, 1, "an overwrite must not leave the old entry behind");
}

#[test]
fn to_string_rejects_truncated_utf8() {
    // \xe4\xb8 is a truncated 3-byte UTF-8 sequence (missing final byte)
    let bad_utf8: &[u8] = b"hello \xe4\xb8";
    let result = Metadata::to_string(bad_utf8);
    assert!(result.is_err());
}

#[test]
fn walk_with_invalid_utf8_key() {
    let mut metadata = Metadata::new();
    // Store a value with an invalid UTF-8 key using the private `set` method
    metadata
        .set(b"\xe4\xb8", b"value", MetadataType::String)
        .unwrap();

    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    metadata.walk(|key, value, _value_type| {
        entries.push((key.to_vec(), value.to_vec()));
    });

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0, b"\xe4\xb8");
    assert_eq!(entries[0].1, b"value");
}

#[test]
fn get_with_invalid_utf8_key() {
    let mut metadata = Metadata::new();
    metadata
        .set(b"\xe4\xb8", b"value", MetadataType::String)
        .unwrap();

    // get() works on raw &[u8] keys, so it should find the value
    let result = metadata.get(b"\xe4\xb8").unwrap();
    assert_eq!(result, b"value");
}

/// Keys and values of differing lengths, empty ones included, so no two
/// entries shift by the same offset.
const COMPACTION_ENTRIES: [(&str, &[u8]); 5] = [
    ("first", b"1"),
    ("k", b""),
    ("third-key-longer", b"three three three"),
    ("", b"no-key"),
    ("fifth", &[0xff, 0x00, 0xfe]),
];

fn compaction_subject(entries: &[(&str, &[u8])]) -> Metadata {
    let mut metadata = Metadata::new();
    for (key, value) in entries {
        metadata.set_binary(key, value).unwrap();
    }
    metadata
}

/// Every subset, compared byte for byte against a buffer written with only
/// the retained entries.
#[test]
fn retain_entries_leaves_what_writing_the_retained_entries_would() {
    let count = COMPACTION_ENTRIES.len();
    for mask in 0..(1u32 << count) {
        let kept: Vec<(&str, &[u8])> = COMPACTION_ENTRIES
            .iter()
            .enumerate()
            .filter(|(index, _)| mask & (1 << index) != 0)
            .map(|(_, entry)| *entry)
            .collect();

        let mut subject = compaction_subject(&COMPACTION_ENTRIES);
        let dropped = subject
            .retain_entries(|key| kept.iter().any(|(kept_key, _)| kept_key.as_bytes() == key));

        assert_eq!(
            dropped,
            count - kept.len(),
            "mask {mask:#07b} must report every entry it dropped"
        );

        if kept.is_empty() {
            assert_eq!(
                subject.buffer.len(),
                Metadata::HEADER_SIZE,
                "mask {mask:#07b} must leave the header and nothing else"
            );
        } else {
            let expected = compaction_subject(&kept);
            assert_eq!(
                subject.buffer.as_ref(),
                expected.buffer.as_ref(),
                "mask {mask:#07b} must match a buffer written with only those entries"
            );
        }

        for (key, value) in &kept {
            assert_eq!(
                subject.get(key.as_bytes()).unwrap(),
                *value,
                "mask {mask:#07b} must preserve the value of {key}"
            );
        }
        let mut walked = 0;
        subject.walk(|_, _, _| walked += 1);
        assert_eq!(walked, kept.len(), "mask {mask:#07b} entry count");
    }
}

/// Nothing to walk, so nothing to drop and nothing to truncate.
#[test]
fn retain_entries_on_a_buffer_with_no_entries() {
    let mut empty = Metadata::new();
    assert_eq!(empty.retain_entries(|_| false), 0);
    assert!(empty.is_empty());

    let mut header_only = compaction_subject(&COMPACTION_ENTRIES[..1]);
    header_only.retain_entries(|_| false);
    assert_eq!(header_only.buffer.len(), Metadata::HEADER_SIZE);
    assert_eq!(header_only.retain_entries(|_| true), 0);
    assert_eq!(header_only.buffer.len(), Metadata::HEADER_SIZE);
}

/// The predicate sees raw key bytes, so a key that is not text is decided on
/// like any other rather than ending the walk.
#[test]
fn retain_entries_judges_a_key_that_is_not_text() {
    let mut metadata = Metadata::new();
    metadata
        .set(b"\xe4\xb8", b"binary key", MetadataType::String)
        .unwrap();
    metadata.set_string("text", "kept").unwrap();

    assert_eq!(metadata.retain_entries(|key| key != b"\xe4\xb8"), 1);
    assert_eq!(metadata.get_string("text").unwrap(), "kept");
    assert!(metadata.get(b"\xe4\xb8").is_err());
}

/// A length reaching past the buffer ends the walk and the unreadable tail
/// is discarded. Only a blob that failed [`Metadata::check_buffer`] gets here.
#[test]
fn retain_entries_stops_at_an_entry_that_leaves_the_buffer() {
    let mut metadata = compaction_subject(&COMPACTION_ENTRIES[..2]);
    let first_block = Metadata::ITEM_SIZE + "first".len() + 1;
    let forged = Metadata::HEADER_SIZE + first_block + std::mem::size_of::<u32>();
    metadata.buffer[forged..forged + std::mem::size_of::<u32>()]
        .copy_from_slice(&u32::MAX.to_ne_bytes());

    assert_eq!(metadata.retain_entries(|_| true), 0);
    assert_eq!(metadata.get_binary("first").unwrap(), b"1");
    assert!(metadata.get_binary("k").is_err());
}

/// A compacted buffer is still in the state [`Metadata::set`] appends against.
#[test]
fn retain_entries_leaves_the_buffer_appendable() {
    let mut metadata = compaction_subject(&COMPACTION_ENTRIES);
    metadata.retain_entries(|key| key == b"third-key-longer");
    metadata.set_string("added", "after").unwrap();

    metadata
        .check_buffer()
        .expect("a compacted buffer must still be well formed");
    assert_eq!(
        metadata.get_binary("third-key-longer").unwrap(),
        b"three three three"
    );
    assert_eq!(metadata.get_string("added").unwrap(), "after");

    let expected = {
        let mut expected = Metadata::new();
        expected
            .set_binary("third-key-longer", b"three three three")
            .unwrap();
        expected.set_string("added", "after").unwrap();
        expected
    };
    assert_eq!(metadata.buffer.as_ref(), expected.buffer.as_ref());
}

/// The reserved sets bound every list, including the sentinel.
#[test]
fn no_inherit_list_can_carry_a_reserved_key() {
    let reserved: Vec<&str> = RESERVED_STAMP
        .iter()
        .chain(RESERVED_ERASE.iter())
        .copied()
        .collect();

    for key in &reserved {
        for inherit in [
            MetadataInherit::All,
            MetadataInherit::from_keys([*key]),
            MetadataInherit::from_keys([MetadataInherit::ALL]),
        ] {
            assert!(
                !inherit.permits(key),
                "{key} is reserved and must not be inheritable via {inherit:?}"
            );
        }
    }
}

/// Naming a key is what carries it, so the default carries nothing.
#[test]
fn the_default_inherit_list_carries_nothing() {
    let inherit = MetadataInherit::default();
    assert!(inherit.is_empty());
    for key in [
        CHANGE_REQUEST,
        REVIEWED_BY,
        CREATED_BY,
        "crowd-status-checks",
    ] {
        assert!(!inherit.permits(key), "{key} must not survive by default");
    }
}

/// Keys lore does not know are governed by the same list as its own.
#[test]
fn only_the_named_keys_are_carried() {
    let inherit = MetadataInherit::from_keys([CHANGE_REQUEST, "crowd-status-checks"]);

    assert!(inherit.permits(CHANGE_REQUEST));
    assert!(inherit.permits("crowd-status-checks"));
    assert!(!inherit.permits(REVIEWED_BY));
    assert!(!inherit.permits(CREATED_BY));
    assert!(!inherit.permits("crowd-review-state"));
}

/// The sentinel selects everything wherever it appears in the list.
#[test]
fn the_sentinel_selects_all_wherever_it_appears() {
    for keys in [
        vec![MetadataInherit::ALL],
        vec![CHANGE_REQUEST, MetadataInherit::ALL],
        vec![MetadataInherit::ALL, CHANGE_REQUEST],
    ] {
        let inherit = MetadataInherit::from_keys(keys.clone());
        assert!(
            matches!(inherit, MetadataInherit::All),
            "{keys:?} must resolve to All"
        );
        assert!(inherit.permits("anything-at-all"));
        assert!(!inherit.permits(MERGED_BY), "All is still bounded");
    }
}

/// Only permitted keys survive; the values of those that do are unchanged.
#[test]
fn retain_inherited_drops_everything_not_named() {
    let mut metadata = Metadata::new();
    metadata.set_string(MESSAGE, "source message").unwrap();
    metadata.set_string(COMMITTED_BY, "source.user").unwrap();
    metadata.set_string(MERGED_BY, "source.merger").unwrap();
    metadata.set_string(CHANGE_REQUEST, "CR-1234").unwrap();
    metadata.set_string(REVIEWED_BY, "source.reviewer").unwrap();
    metadata.set_string("crowd-status-checks", "green").unwrap();
    metadata.set_u64(FAST_FORWARD_MERGE, 1).unwrap();

    metadata.retain_inherited(&MetadataInherit::from_keys([CHANGE_REQUEST]));

    assert_eq!(metadata.get_string(CHANGE_REQUEST).unwrap(), "CR-1234");
    for dropped in [
        MESSAGE,
        COMMITTED_BY,
        MERGED_BY,
        REVIEWED_BY,
        "crowd-status-checks",
        FAST_FORWARD_MERGE,
    ] {
        assert!(
            metadata.get_typed(dropped).is_err(),
            "{dropped} must not survive an inherit list that does not name it"
        );
    }
}

/// Carrying nothing empties the buffer, which is what serializes to the
/// zero hash a revision without metadata holds.
#[test]
fn retain_inherited_can_empty_the_buffer() {
    let mut metadata = Metadata::new();
    metadata.set_string(MESSAGE, "source message").unwrap();
    metadata.set_string(CHANGE_REQUEST, "CR-1234").unwrap();

    metadata.retain_inherited(&MetadataInherit::default());

    assert!(metadata.is_empty(), "carrying nothing must leave nothing");
}

/// A key that is not text cannot be named in an inherit list, so it is
/// never permitted, not even by the sentinel.
#[test]
fn a_key_that_is_not_text_is_never_inherited() {
    let mut metadata = Metadata::new();
    metadata
        .set(b"\xe4\xb8", b"value", MetadataType::String)
        .unwrap();
    metadata.set_string(CHANGE_REQUEST, "CR-1234").unwrap();

    metadata.retain_inherited(&MetadataInherit::All);

    assert_eq!(metadata.get_string(CHANGE_REQUEST).unwrap(), "CR-1234");
    assert!(
        metadata.get(b"\xe4\xb8").is_err(),
        "a key that cannot be named cannot be inherited"
    );
}

#[test]
fn decode_to_value_numeric() {
    let result = Metadata::decode_to_value("42", &MetadataType::Numeric).unwrap();
    assert_eq!(result, 42u64.to_le_bytes().to_vec());
}

#[test]
fn decode_to_value_numeric_zero() {
    let result = Metadata::decode_to_value("0", &MetadataType::Numeric).unwrap();
    assert_eq!(result, 0u64.to_le_bytes().to_vec());
}

#[test]
fn decode_to_value_numeric_max() {
    let max = u64::MAX.to_string();
    let result = Metadata::decode_to_value(&max, &MetadataType::Numeric).unwrap();
    assert_eq!(result, u64::MAX.to_le_bytes().to_vec());
}

#[test]
fn decode_to_value_numeric_invalid() {
    let result = Metadata::decode_to_value("not_a_number", &MetadataType::Numeric);
    assert!(result.is_err());
}

#[test]
fn decode_to_value_numeric_negative() {
    let result = Metadata::decode_to_value("-1", &MetadataType::Numeric);
    assert!(result.is_err());
}

#[test]
fn decode_to_value_numeric_overflow() {
    let overflow = format!("{}0", u64::MAX);
    let result = Metadata::decode_to_value(&overflow, &MetadataType::Numeric);
    assert!(result.is_err());
}

#[test]
fn decode_to_value_string() {
    let result = Metadata::decode_to_value("hello", &MetadataType::String).unwrap();
    assert_eq!(result, b"hello");
}

#[test]
fn decode_to_value_binary() {
    let result = Metadata::decode_to_value("raw data", &MetadataType::Binary).unwrap();
    assert_eq!(result, b"raw data");
}
