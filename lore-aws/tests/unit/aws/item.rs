// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::borrow::Cow;

use bytes::Bytes;
use lore_aws::aws::item::*;
use serde::Deserialize;
use serde::Serialize;

use crate::aws::differential::from_serde_dynamo;
use crate::aws::differential::item_from_serde_dynamo;
use crate::aws::differential::item_to_serde_dynamo;
use crate::aws::differential::to_serde_dynamo;

/// `from_static` shares the program's own bytes, so the pointer is the literal's.
#[test]
fn a_literal_name_does_not_allocate() {
    let name = ByteStr::from_static("repository_context");

    assert_eq!(
        name.as_bytes().as_ptr(),
        "repository_context".as_ptr(),
        "a literal must not be copied"
    );
}

#[test]
fn text_read_off_the_wire_shares_the_response_buffer() {
    let response = Bytes::from_static(b"....hello....");
    let text = ByteStr::from_utf8(response.slice(4..9)).unwrap();

    assert_eq!(text.as_str(), "hello");
    assert_eq!(
        text.as_bytes().as_ptr(),
        response[4..].as_ptr(),
        "a wire string must be a slice of the response, not a copy"
    );
}

#[test]
fn non_text_bytes_are_refused_rather_than_carried() {
    assert!(ByteStr::from_utf8(Bytes::from_static(&[0xff, 0xfe])).is_err());
}

/// Text a reader had to build is taken as it is; only borrowed text is copied.
#[test]
fn owned_text_converts_without_a_copy() {
    let owned = String::from("unescaped text");
    let pointer = owned.as_ptr();

    let text = ByteStr::from(Cow::<str>::Owned(owned));

    assert_eq!(text.as_bytes().as_ptr(), pointer);
    assert_eq!(ByteStr::from(Cow::Borrowed("borrowed")), "borrowed");
}

#[test]
fn numbers_format_without_allocating_and_parse_back() {
    for value in [0u64, 7, 4096, u64::MAX] {
        let number = NumStr::from(value);

        assert_eq!(number.as_str(), value.to_string());
        assert_eq!(number.parse::<u64>().unwrap(), value);
    }

    assert_eq!(NumStr::from(-42i64).as_str(), "-42");
}

/// The widest number `DynamoDB` accepts is 38 significant digits, which does not fit inline. It
/// must round-trip anyway rather than truncate.
#[test]
fn an_oversized_number_spills_instead_of_truncating() {
    let wide = "1".repeat(38);
    let number = NumStr::from_ascii(&wide);

    assert_eq!(number.as_str(), wide);
}

#[test]
fn the_hot_rows_fit_inline() {
    let mut fragments = Item::new();
    fragments.insert(
        ByteStr::from_static("hash"),
        AttributeValue::blob([7u8; 32]),
    );
    fragments.insert(
        ByteStr::from_static("repository_context"),
        AttributeValue::blob([8u8; 32]),
    );

    assert!(
        !fragments.spilled(),
        "a fragment association must not allocate a table"
    );
}

#[test]
fn insert_replaces_and_keeps_first_insertion_order() {
    let mut item = Item::new();
    item.insert(ByteStr::from_static("a"), AttributeValue::number(1u32));
    item.insert(ByteStr::from_static("b"), AttributeValue::number(2u32));
    let replaced = item.insert(ByteStr::from_static("a"), AttributeValue::number(3u32));

    assert_eq!(replaced, Some(AttributeValue::number(1u32)));
    assert_eq!(item.len(), 2);
    let names: Vec<&str> = item.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["a", "b"], "replacing must not reorder");
    assert_eq!(item.get("a"), Some(&AttributeValue::number(3u32)));
}

#[test]
fn remove_takes_the_value_and_preserves_the_rest() {
    let mut item = Item::new();
    item.insert(ByteStr::from_static("a"), AttributeValue::number(1u32));
    item.insert(ByteStr::from_static("b"), AttributeValue::number(2u32));
    item.insert(ByteStr::from_static("c"), AttributeValue::number(3u32));

    assert_eq!(item.remove("b"), Some(AttributeValue::number(2u32)));
    assert_eq!(item.remove("b"), None);
    let names: Vec<&str> = item.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["a", "c"]);
}

#[test]
fn a_missing_name_reads_as_absent_rather_than_panicking() {
    let item = Item::new();

    assert_eq!(item.get("nothing"), None);
    assert!(item.is_empty());
}

/// Equality ignores attribute order, and gives the same answer from either side even when one item
/// repeats a name.
#[test]
fn item_equality_ignores_order_and_is_symmetric() {
    let entry = |name: &'static str, value: u32| {
        (ByteStr::from_static(name), AttributeValue::number(value))
    };

    let distinct = Item::from([entry("a", 1), entry("b", 2)]);
    let reordered = Item::from([entry("b", 2), entry("a", 1)]);
    let repeated = Item::from([entry("a", 1), entry("a", 1)]);

    assert_eq!(distinct, reordered);
    assert_eq!(reordered, distinct);

    assert_ne!(repeated, distinct);
    assert_ne!(distinct, repeated);
    assert_eq!(repeated, repeated.clone());
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Row {
    #[serde(with = "serde_bytes")]
    key: [u8; 4],
    name: String,
    count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    missing: Option<u32>,
}

#[test]
fn a_row_round_trips_through_the_bridge() {
    let row = Row {
        key: [1, 2, 3, 4],
        name: "lock".to_owned(),
        count: 9,
        missing: None,
    };

    let item: Item = to_item(&row).unwrap();

    assert_eq!(
        item.get("key"),
        Some(&AttributeValue::blob([1u8, 2, 3, 4])),
        "bytes must reach DynamoDB as binary, not as text"
    );
    assert_eq!(item.get("name"), Some(&AttributeValue::string("lock")));
    assert_eq!(item.get("count"), Some(&AttributeValue::number(9u64)));
    assert_eq!(
        item.get("missing"),
        None,
        "a skipped field writes no attribute"
    );

    assert_eq!(from_item::<Row>(item).unwrap(), row);
}

#[test]
fn every_attribute_kind_survives_the_serde_dynamo_bridge() {
    let mut nested = Item::new();
    nested.insert(ByteStr::from_static("inner"), AttributeValue::Bool(true));

    let values = [
        AttributeValue::blob([9u8; 3]),
        AttributeValue::string("text"),
        AttributeValue::number(11u32),
        AttributeValue::Bool(false),
        AttributeValue::Null,
        AttributeValue::M(Box::new(nested)),
        AttributeValue::L(vec![AttributeValue::number(1u32)]),
        AttributeValue::Bs(vec![Blob::from_slice(b"ab")]),
        AttributeValue::Ss(vec![ByteStr::from_static("s")]),
        AttributeValue::Ns(vec![NumStr::from(5u32)]),
    ];

    for value in values {
        let back = from_serde_dynamo(to_serde_dynamo(value.clone()));

        assert_eq!(back, value, "{value:?} must survive the round trip");
    }
}

/// `serde_dynamo` holds an item in a `HashMap`, so the order does not survive, and item equality
/// does not ask it to.
#[test]
fn an_item_round_trips_through_serde_dynamos_representation() {
    let mut item = Item::new();
    item.insert(
        ByteStr::from_static("hash"),
        AttributeValue::blob([3u8; 32]),
    );
    item.insert(ByteStr::from_static("state"), AttributeValue::number(2u32));

    let round_tripped = item_from_serde_dynamo(item_to_serde_dynamo(item.clone()));

    assert_eq!(round_tripped, item);
}
