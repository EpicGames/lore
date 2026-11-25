// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The in-house serde bridge against `serde_dynamo`, for every row shape Lore stores.
//!
//! This is what stands between a subtle encoding difference and a silent rewrite of production
//! data. `serde_dynamo` is a dev-dependency so the comparison runs with every test run rather than
//! once.
//!
//! Three directions, because compatibility is not one property:
//!
//! 1. **Encode the same.** Our item equals the one `serde_dynamo` produces, attribute for
//!    attribute.
//! 2. **Read what it wrote.** A row encoded by `serde_dynamo` deserializes through us to the same
//!    value.
//! 3. **It reads what we write.** A row we encoded deserializes through `serde_dynamo` to the same
//!    value — which is what makes a rollback safe.
//!
//! The identifiers are the part that matters: `lore-base` decides between raw bytes and hex text by
//! asking the serializer whether it is human-readable, and every table in production holds the
//! bytes.
use bytes::Bytes;
use lore_aws::aws::item::AttributeValue;
use lore_aws::aws::item::Blob;
use lore_aws::aws::item::ByteStr;
use lore_aws::aws::item::Item;
use lore_aws::aws::item::NumStr;
use lore_aws::aws::serde::from_item;
use lore_aws::aws::serde::to_attribute_value;
use lore_aws::aws::serde::to_item;
use lore_aws::store::immutable_store::FragmentStateEntry;
use lore_aws::store::immutable_store::FragmentsEntry;
use lore_aws::store::lock_store::LockEntry;
use lore_aws::store::lock_store::LockKey;
use lore_aws::store::mutable_store::MutableStoreEntry;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_base::types::Hash;
use lore_base::types::Partition;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// `serde_dynamo`'s attribute value in this crate's representation.
pub(crate) fn from_serde_dynamo(value: serde_dynamo::AttributeValue) -> AttributeValue {
    use serde_dynamo::AttributeValue as Sd;

    match value {
        Sd::N(number) => AttributeValue::N(NumStr::from_ascii(&number)),
        Sd::S(text) => AttributeValue::S(ByteStr::from(text)),
        Sd::Bool(flag) => AttributeValue::Bool(flag),
        Sd::B(bytes) => AttributeValue::B(Blob::from(bytes)),
        Sd::Null(_) => AttributeValue::Null,
        Sd::M(map) => AttributeValue::M(Box::new(item_from_serde_dynamo(
            serde_dynamo::Item::from(map),
        ))),
        Sd::L(list) => AttributeValue::L(list.into_iter().map(from_serde_dynamo).collect()),
        Sd::Ss(set) => AttributeValue::Ss(set.into_iter().map(ByteStr::from).collect()),
        Sd::Ns(set) => AttributeValue::Ns(
            set.iter()
                .map(|number| NumStr::from_ascii(number))
                .collect(),
        ),
        Sd::Bs(set) => AttributeValue::Bs(set.into_iter().map(Blob::from).collect()),
    }
}

/// This crate's attribute value in `serde_dynamo`'s representation.
pub(crate) fn to_serde_dynamo(value: AttributeValue) -> serde_dynamo::AttributeValue {
    use serde_dynamo::AttributeValue as Sd;

    match value {
        AttributeValue::N(number) => Sd::N(number.as_str().to_owned()),
        AttributeValue::S(text) => Sd::S(text.into()),
        AttributeValue::Bool(flag) => Sd::Bool(flag),
        AttributeValue::B(bytes) => Sd::B(bytes.to_vec()),
        AttributeValue::Null => Sd::Null(true),
        AttributeValue::M(item) => Sd::M(item_to_serde_dynamo(*item).into_inner()),
        AttributeValue::L(list) => Sd::L(list.into_iter().map(to_serde_dynamo).collect()),
        AttributeValue::Ss(set) => Sd::Ss(set.into_iter().map(String::from).collect()),
        AttributeValue::Ns(set) => Sd::Ns(
            set.iter()
                .map(|number| number.as_str().to_owned())
                .collect(),
        ),
        AttributeValue::Bs(set) => Sd::Bs(set.iter().map(Blob::to_vec).collect()),
    }
}

/// `serde_dynamo`'s item in this crate's representation, its attributes sorted by name.
///
/// `serde_dynamo` holds an item in a `HashMap`, whose order is a function of the hash seed; sorting
/// makes the converted item, and anything printed of it, the same in every run.
pub(crate) fn item_from_serde_dynamo(item: serde_dynamo::Item) -> Item {
    let mut entries: Vec<(String, serde_dynamo::AttributeValue)> =
        item.into_inner().into_iter().collect();
    entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));

    entries
        .into_iter()
        .map(|(name, value)| (ByteStr::from(name), from_serde_dynamo(value)))
        .collect()
}

/// This crate's item in `serde_dynamo`'s representation.
pub(crate) fn item_to_serde_dynamo(item: Item) -> serde_dynamo::Item {
    let map: std::collections::HashMap<String, serde_dynamo::AttributeValue> = item
        .into_iter()
        .map(|(name, value)| (String::from(name), to_serde_dynamo(value)))
        .collect();

    serde_dynamo::Item::from(map)
}

/// The item `serde_dynamo` produces for `value`, in our representation.
fn through_serde_dynamo<T: Serialize>(value: &T) -> Item {
    item_from_serde_dynamo(
        serde_dynamo::to_item::<_, serde_dynamo::Item>(value)
            .expect("serde_dynamo encodes every entry type"),
    )
}

/// Assert all three directions for one value.
fn agrees<T>(value: &T)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let ours = to_item(value).expect("our bridge encodes it");
    let theirs = through_serde_dynamo(value);

    assert_eq!(
        ours, theirs,
        "the two encodings differ, which would change what production holds"
    );

    let read_theirs: T = from_item(theirs.clone()).expect("we read what serde_dynamo wrote");
    assert_eq!(
        &read_theirs, value,
        "reading serde_dynamo's row lost something"
    );

    let read_ours: T = serde_dynamo::from_item(item_to_serde_dynamo(ours))
        .expect("serde_dynamo reads what we wrote");
    assert_eq!(
        &read_ours, value,
        "serde_dynamo could not read our row back"
    );
}

fn hash() -> Hash {
    Hash::from([7u8; 32])
}

fn context() -> Context {
    Context::from([8u8; 16])
}

#[test]
fn a_fragment_association_agrees() {
    // `partition_context` carries `#[serde(with = "serde_bytes")]`, so it asks for binary by name.
    let mut partition_context = [0u8; size_of::<Context>() * 2];
    partition_context[..size_of::<Context>()].copy_from_slice(&[1u8; 16]);
    partition_context[size_of::<Context>()..].copy_from_slice(&[2u8; 16]);

    agrees(&FragmentsEntry {
        hash: hash(),
        partition_context,
    });
}

#[test]
fn a_fragment_state_row_agrees() {
    agrees(&FragmentStateEntry::new(
        hash(),
        lore_aws::store::immutable_store::FragmentState::Stored,
    ));
}

/// A row whose optional attribute is absent, which is how a key-only row is written.
#[test]
fn a_state_key_agrees() {
    let key = FragmentStateEntry::key(hash());

    let ours = to_item(&key).expect("our bridge encodes it");
    assert_eq!(ours, through_serde_dynamo(&key));
    assert_eq!(
        ours.len(),
        1,
        "a skipped attribute must not be written: {ours:?}"
    );
}

#[test]
fn a_mutable_store_row_agrees() {
    agrees(&MutableStoreEntry::new(context(), hash()).with_value(Hash::from([9u8; 32])));
    agrees(&MutableStoreEntry::new(context(), hash()));
}

/// `LockEntry` has no `PartialEq`, so the round trips are asserted on the row it encodes to rather
/// than on the value — which is the property that matters anyway.
#[test]
fn a_lock_row_agrees() {
    let entry = LockEntry {
        description: "/my/test/file.txt".to_owned(),
        branch: context(),
        hash: hash(),
        owner_id: "someone".to_owned(),
        repository: Partition::from([3u8; 16]),
        repository_branch: Bytes::from_static(&[4u8; 32]),
        timestamp: "2026-08-12T00:00:00+00:00".to_owned(),
    };

    let ours = to_item(&entry).expect("our bridge encodes it");
    let theirs = through_serde_dynamo(&entry);
    assert_eq!(ours, theirs);

    let read_theirs: LockEntry = from_item(theirs).expect("we read what serde_dynamo wrote");
    assert_eq!(to_item(&read_theirs).unwrap(), ours);

    let read_ours: LockEntry = serde_dynamo::from_item(item_to_serde_dynamo(ours.clone()))
        .expect("serde_dynamo reads what we wrote");
    assert_eq!(through_serde_dynamo(&read_ours), ours);
}

#[test]
fn a_lock_key_agrees() {
    agrees(&LockKey {
        hash: hash(),
        repository_branch: Bytes::from_static(&[5u8; 32]),
    });
}

/// The legacy row: a whole fragment flattened alongside the hash, and `#[serde(flatten)]` is the
/// one serde feature this bridge has to support that nothing else in the tree uses.
#[test]
fn a_flattened_legacy_row_reads_the_same() {
    let fragment = Fragment {
        flags: FragmentFlags::PayloadCompressedZstd.bits(),
        size_payload: 4096,
        size_content: 16384,
    };

    // Written the way that era wrote it: through `serde_dynamo`, from a shape with no `state`.
    #[derive(Serialize)]
    struct LegacyRow {
        hash: Hash,
        #[serde(flatten)]
        fragment: Fragment,
    }

    let row = LegacyRow {
        hash: hash(),
        fragment,
    };
    let theirs = through_serde_dynamo(&row);
    let ours = to_item(&row).expect("our bridge flattens it");
    assert_eq!(ours, theirs);

    let read: lore_aws::store::immutable_store::FragmentMetadataEntry =
        from_item(theirs).expect("we read a flattened row");
    assert_eq!(read.hash, hash());
    assert_eq!(read.fragment, Some(fragment));
}

/// An identifier must reach the wire as binary. The bridge reports itself human-readable, so this
/// rests on `lore-base` recognising the bridge's serializer by name, as it recognises
/// `serde_dynamo`'s.
#[test]
fn identifiers_are_written_as_binary_not_hex() {
    let item = to_item(&LockKey {
        hash: hash(),
        repository_branch: Bytes::from_static(&[5u8; 32]),
    })
    .expect("encodes");

    assert_eq!(
        item.get("hash").and_then(AttributeValue::as_blob),
        Some(&[7u8; 32][..]),
        "a hash written as hex text would change the table's key type: {item:?}"
    );
}

/// And a row that holds one as hex text still reads, because rows in production hold both
/// spellings.
#[test]
fn an_identifier_stored_as_hex_text_still_reads() {
    let mut item = Item::new();
    item.insert(
        lore_aws::aws::item::ByteStr::from_static("hash"),
        AttributeValue::string("0707070707070707070707070707070707070707070707070707070707070707"),
    );
    item.insert(
        // `LockKey` is `rename_all = "camelCase"`.
        lore_aws::aws::item::ByteStr::from_static("repositoryBranch"),
        AttributeValue::blob([5u8; 32]),
    );

    let key: LockKey = from_item(item).expect("hex text reads back as an identifier");

    assert_eq!(key.hash, hash());
}

/// A float is written in the digits its own type displays, which is what `serde_dynamo` writes: an
/// `f32` widened to `f64` first would carry the widened value's digits instead.
#[test]
fn floats_agree() {
    for value in [0.1f32, 1.5, 16_777_216.0] {
        let theirs = serde_dynamo::to_attribute_value::<_, serde_dynamo::AttributeValue>(value)
            .expect("serde_dynamo encodes a finite f32");

        assert_eq!(
            to_attribute_value(value).expect("our bridge encodes it"),
            from_serde_dynamo(theirs),
            "{value}"
        );
    }

    for value in [0.1f64, -2.5, 1e-7] {
        let theirs = serde_dynamo::to_attribute_value::<_, serde_dynamo::AttributeValue>(value)
            .expect("serde_dynamo encodes a finite f64");

        assert_eq!(
            to_attribute_value(value).expect("our bridge encodes it"),
            from_serde_dynamo(theirs),
            "{value}"
        );
    }
}
