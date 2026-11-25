// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_aws::aws::item::AttributeValue;
use lore_aws::aws::item::ByteStr;
use lore_aws::aws::item::Item;
use lore_aws::aws::item::NumStr;
use lore_aws::aws::serde::*;
use serde::Deserialize;
use serde::Serialize;

/// The exception in `lore-base` that keeps identifiers binary matches this module by name. If the
/// module ever moves, every identifier silently becomes hex text and every table's key type
/// changes, so the name is asserted rather than trusted.
#[test]
fn the_serializer_is_the_one_lore_base_makes_an_exception_for() {
    assert!(
        std::any::type_name::<ValueSerializer>()
            .starts_with(lore_base::types::LORE_DYNAMO_SERIALIZER),
        "lore-base looks for {:?}, this is {:?}",
        lore_base::types::LORE_DYNAMO_SERIALIZER,
        std::any::type_name::<ValueSerializer>()
    );
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Row {
    text: String,
    number: i64,
    flag: bool,
    #[serde(with = "serde_bytes")]
    raw: Vec<u8>,
    nested: Nested,
    list: Vec<u32>,
    absent: Option<u32>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Nested {
    inner: String,
}

#[test]
fn every_shape_round_trips() {
    let row = Row {
        text: "hello".to_owned(),
        number: -7,
        flag: true,
        raw: vec![1, 2, 3],
        nested: Nested {
            inner: "deep".to_owned(),
        },
        list: vec![4, 5],
        absent: None,
    };

    let item = to_item(&row).expect("encodes");

    assert_eq!(item.get("text"), Some(&AttributeValue::string("hello")));
    assert_eq!(
        item.get("number"),
        Some(&AttributeValue::N(NumStr::from(-7i64)))
    );
    assert_eq!(item.get("flag"), Some(&AttributeValue::Bool(true)));
    assert_eq!(item.get("raw"), Some(&AttributeValue::blob([1u8, 2, 3])));
    assert_eq!(item.get("absent"), Some(&AttributeValue::Null));
    assert!(matches!(item.get("list"), Some(AttributeValue::L(values)) if values.len() == 2));
    assert!(matches!(item.get("nested"), Some(AttributeValue::M(_))));

    assert_eq!(from_item::<Row>(item).expect("decodes"), row);
}

/// A field name is `&'static str`, so a row's names cost nothing to write. This is most of what the
/// bridge exists for, so it is asserted rather than assumed: two rows encoded separately point at
/// the same name, which they could not do if either had copied it.
#[test]
fn attribute_names_are_written_without_allocating() {
    let name_of = |value: &str| {
        let item = to_item(&Nested {
            inner: value.to_owned(),
        })
        .expect("encodes");
        let (name, _) = item.iter().next().expect("one attribute");

        name.as_bytes().as_ptr()
    };

    assert_eq!(
        name_of("first"),
        name_of("second"),
        "each row copied its attribute name instead of borrowing the struct's literal"
    );
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
enum Choice {
    Unit,
    Wrapped(u32),
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Chosen {
    choice: Choice,
}

#[test]
fn an_enum_round_trips_externally_tagged() {
    let unit = to_item(&Chosen {
        choice: Choice::Unit,
    })
    .expect("encodes");
    assert_eq!(unit.get("choice"), Some(&AttributeValue::string("Unit")));
    assert_eq!(
        from_item::<Chosen>(unit).expect("decodes"),
        Chosen {
            choice: Choice::Unit
        }
    );

    let wrapped = to_item(&Chosen {
        choice: Choice::Wrapped(3),
    })
    .expect("encodes");
    assert!(matches!(wrapped.get("choice"), Some(AttributeValue::M(item)) if item.len() == 1));
    assert_eq!(
        from_item::<Chosen>(wrapped).expect("decodes"),
        Chosen {
            choice: Choice::Wrapped(3)
        }
    );
}

/// A newtype variant at the top is the single-attribute row an externally tagged enum writes.
#[test]
fn an_enum_variant_row_is_its_one_attribute() {
    let item = to_item(Choice::Wrapped(4)).expect("encodes");

    assert_eq!(item.len(), 1);
    assert_eq!(item.get("Wrapped"), Some(&AttributeValue::number(4u32)));
}

/// An enum read from a map that is not a single entry is refused rather than read as its first
/// entry.
#[test]
fn an_enum_in_a_map_of_more_than_one_entry_is_refused() {
    let mut variant = Item::new();
    variant.insert(
        ByteStr::from_static("Wrapped"),
        AttributeValue::number(3u32),
    );
    variant.insert(ByteStr::from_static("Unit"), AttributeValue::Null);

    let mut item = Item::new();
    item.insert(
        ByteStr::from_static("choice"),
        AttributeValue::from(variant),
    );

    let error = from_item::<Chosen>(item).expect_err("two entries name no one variant");
    assert_eq!(error.kind(), ErrorKind::WrongType);
}

/// `DynamoDB` numbers are decimal text, so a number that has no decimal text is refused rather than
/// written as something the service would take for a string.
#[test]
fn a_number_that_is_not_finite_is_refused() {
    for error in [
        to_attribute_value(f64::NAN),
        to_attribute_value(f64::INFINITY),
        to_attribute_value(f32::NEG_INFINITY),
    ] {
        assert_eq!(
            error.expect_err("no decimal text").kind(),
            ErrorKind::NotFinite
        );
    }
    assert!(to_attribute_value(1.5f64).is_ok());
}

/// An `f32` is written in its own shortest digits, as `serde_dynamo` writes it, not in the digits
/// of the `f64` it widens to.
#[test]
fn an_f32_is_written_in_its_own_digits() {
    for (value, digits) in [(0.1f32, "0.1"), (1.5, "1.5"), (16_777_216.0, "16777216")] {
        assert_eq!(
            to_attribute_value(value).expect("encodes"),
            AttributeValue::N(NumStr::from_ascii(digits)),
            "{value}"
        );
    }
    assert_eq!(
        to_attribute_value(0.1f64).expect("encodes"),
        AttributeValue::N(NumStr::from_ascii("0.1"))
    );
}

#[test]
fn a_row_must_be_a_map() {
    for error in [to_item("just a string"), to_item(7u32), to_item([1u32, 2])] {
        assert_eq!(error.expect_err("not a map").kind(), ErrorKind::NotARow);
    }
}

#[test]
fn an_attribute_name_must_be_text() {
    let numeric_keys = std::collections::BTreeMap::from([(1u32, "one")]);

    let error = to_item(&numeric_keys).expect_err("a number is not an attribute name");
    assert_eq!(error.kind(), ErrorKind::NotAKey);
}

/// A newtype reads through to the value it wraps, whether it is a field's value, a map's key, or a
/// whole row.
#[test]
fn a_newtype_reads_and_writes_as_what_it_wraps() {
    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
    struct Name(String);

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Counter(u64);

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Counted {
        count: Counter,
        by_name: std::collections::BTreeMap<Name, u32>,
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Wrapped(Counted);

    let row = Wrapped(Counted {
        count: Counter(7),
        by_name: std::collections::BTreeMap::from([(Name("a".to_owned()), 1)]),
    });

    let item = to_item(&row).expect("encodes");
    assert_eq!(item.get("count"), Some(&AttributeValue::number(7u64)));

    assert_eq!(from_item::<Wrapped>(item).expect("decodes"), row);
}

/// A `serde_bytes` field asks for binary by name, and reading hex text into it would put the text's
/// own bytes where the value belongs.
#[test]
fn a_binary_field_refuses_text() {
    #[derive(Debug, Deserialize)]
    struct Binary {
        #[serde(with = "serde_bytes")]
        #[allow(dead_code, reason = "the read is what is under test, not the value")]
        raw: Vec<u8>,
    }

    let mut item = Item::new();
    item.insert(ByteStr::from_static("raw"), AttributeValue::string("00ff"));

    let error = from_item::<Binary>(item).expect_err("text is not binary");
    assert_eq!(error.kind(), ErrorKind::WrongType);
}

#[test]
fn a_missing_optional_field_reads_as_none() {
    #[derive(Debug, PartialEq, Deserialize)]
    struct Optional {
        present: u32,
        missing: Option<u32>,
    }

    let mut item = Item::new();
    item.insert(
        ByteStr::from_static("present"),
        AttributeValue::number(1u32),
    );

    assert_eq!(
        from_item::<Optional>(item).expect("decodes"),
        Optional {
            present: 1,
            missing: None,
        }
    );
}

#[test]
fn a_null_reads_as_none_and_a_value_as_some() {
    #[derive(Debug, PartialEq, Deserialize)]
    struct Optional {
        value: Option<u32>,
    }

    for (attribute, expected) in [
        (AttributeValue::Null, None),
        (AttributeValue::number(2u32), Some(2)),
    ] {
        let mut item = Item::new();
        item.insert(ByteStr::from_static("value"), attribute);

        assert_eq!(
            from_item::<Optional>(item).expect("decodes"),
            Optional { value: expected }
        );
    }
}

/// Nothing Lore stores has this shape, and a row that grew one silently would be worse than one
/// that refuses to be written.
#[test]
fn a_shape_this_client_does_not_write_is_refused_rather_than_guessed() {
    #[derive(Serialize)]
    enum Unsupported {
        Tuple(u32, u32),
        Struct { field: u32 },
    }

    for value in [Unsupported::Tuple(1, 2), Unsupported::Struct { field: 3 }] {
        let error = to_attribute_value(&value).expect_err("refused");
        assert_eq!(error.kind(), ErrorKind::Unsupported);

        let error = to_item(&value).expect_err("refused as a row too");
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }
}
