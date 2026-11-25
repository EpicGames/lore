// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use lore_aws::aws::item::AttributeValue;
use lore_aws::aws::item::Blob;
use lore_aws::aws::item::ByteStr;
use lore_aws::aws::item::Item;
use lore_aws::aws::item::NumStr;
use lore_aws::aws::json::*;
use serde::de::DeserializeSeed;

fn take_body(writer: &mut BodyWriter) -> String {
    String::from_utf8(writer.split().to_vec()).unwrap()
}

fn write(value: &AttributeValue) -> String {
    let mut writer = BodyWriter::new();
    writer.attribute_value(value);
    take_body(&mut writer)
}

fn write_item(item: &Item) -> String {
    let mut writer = BodyWriter::new();
    writer.item(item);
    take_body(&mut writer)
}

/// A reused writer must hand out bodies that are independent of each other.
///
/// `split` keeps the remainder of the chunk, so a second body is written into the same
/// allocation as the first one it handed out. The two must still read back separately.
#[test]
fn a_reused_writer_hands_out_independent_bodies() {
    let mut writer = BodyWriter::new();
    let mut bodies = Vec::new();

    for index in 0..64u32 {
        writer.reserve_for(64);
        writer.raw("{");
        writer.number_field("n", i64::from(index));
        writer.raw("}");
        bodies.push(writer.split());
    }

    for (index, body) in bodies.iter().enumerate() {
        assert_eq!(
            body.as_ref(),
            format!("{{\"n\":{index}}}").as_bytes(),
            "body {index} was overwritten by a later one"
        );
    }
}

/// A body that grows past the chunk still comes back whole.
#[test]
fn a_reused_writer_serves_a_body_larger_than_its_chunk() {
    let mut writer = BodyWriter::new();

    writer.reserve_for(16);
    writer.raw("{");
    writer.number_field("small", 1);
    writer.raw("}");
    let small = writer.split();

    let large_text = "x".repeat(64 * 1024);
    writer.reserve_for(large_text.len() + 32);
    writer.raw("{");
    writer.string_field("large", &large_text);
    writer.raw("}");
    let large = writer.split();

    assert_eq!(small.as_ref(), br#"{"small":1}"#);
    assert_eq!(
        large.len(),
        large_text.len() + r#"{"large":""}"#.len(),
        "the oversized body must be complete"
    );
    assert!(large.ends_with(br#""}"#));
    assert_eq!(
        small.as_ref(),
        br#"{"small":1}"#,
        "growing for a later body must not disturb one already handed out"
    );
}

/// A body that needed more than a chunk must not leave its allocation behind for the bodies after
/// it, whether the estimate was larger than the body or the body grew past a small estimate.
///
/// A body that has been handed out keeps alive the whole allocation it was carved from, so a small
/// body carved out of a large allocation would pin all of it for as long as the small body lives.
#[test]
fn a_large_body_does_not_leave_its_allocation_behind() {
    let large_text = "x".repeat(16 * 1024);

    for (estimate, text) in [(16 * 1024 + 32, "small"), (64, large_text.as_str())] {
        let mut writer = BodyWriter::new();

        writer.reserve_for(estimate);
        writer.raw("{");
        writer.string_field("body", text);
        writer.raw("}");
        let large = writer.split();

        writer.reserve_for(64);
        writer.raw("{}");
        let next = writer.split();

        let start = large.as_ptr() as usize;
        let span = estimate.max(large.len() + 1);
        assert!(
            !(start..start + span).contains(&(next.as_ptr() as usize)),
            "the body after a {}-byte body estimated at {estimate} bytes was carved out of its \
             allocation",
            large.len()
        );
        assert_eq!(next.as_ref(), b"{}");
    }
}

fn parse_value(json: &str) -> Result<AttributeValue, ParseError> {
    parse_response(
        Bytes::copy_from_slice(json.as_bytes()),
        |deserializer, arena| AttributeValueSeed { arena, depth: 0 }.deserialize(deserializer),
    )
}

fn parse_item(json: &str) -> Result<Item, ParseError> {
    parse_response(
        Bytes::copy_from_slice(json.as_bytes()),
        |deserializer, arena| ItemSeed::new(arena).deserialize(deserializer),
    )
}

#[test]
fn writes_each_attribute_type_as_dynamodb_spells_it() {
    assert_eq!(
        write(&AttributeValue::blob([0xde, 0xad])),
        r#"{"B":"3q0="}"#
    );
    assert_eq!(write(&AttributeValue::string("text")), r#"{"S":"text"}"#);
    assert_eq!(write(&AttributeValue::number(42u32)), r#"{"N":"42"}"#);
    assert_eq!(write(&AttributeValue::Bool(true)), r#"{"BOOL":true}"#);
    assert_eq!(write(&AttributeValue::Null), r#"{"NULL":true}"#);
}

/// Base64 is encoded in place at the end of the body, so every length around a group boundary must
/// come out exactly as a standalone encoder writes it, padding included.
#[test]
fn binary_of_every_length_encodes_as_standard_base64() {
    for length in 0..=40usize {
        let bytes: Vec<u8> = (0..length).map(|byte| byte as u8).collect();

        assert_eq!(
            write(&AttributeValue::blob(&bytes)),
            format!(r#"{{"B":"{}"}}"#, BASE64.encode(&bytes)),
            "length {length}"
        );
    }
}

/// A number is a *string* on the wire. Writing it as a JSON number is the classic way to make
/// `DynamoDB` reject a body.
#[test]
fn numbers_are_written_as_strings() {
    assert!(write(&AttributeValue::number(7u64)).contains(r#""N":"7""#));
}

#[test]
fn writes_an_item_in_insertion_order() {
    let mut item = Item::new();
    item.insert(ByteStr::from_static("hash"), AttributeValue::blob([1u8]));
    item.insert(ByteStr::from_static("state"), AttributeValue::number(2u32));

    assert_eq!(
        write_item(&item),
        r#"{"hash":{"B":"AQ=="},"state":{"N":"2"}}"#
    );
}

#[test]
fn writes_nested_maps_and_lists() {
    let mut inner = Item::new();
    inner.insert(ByteStr::from_static("k"), AttributeValue::Bool(false));

    assert_eq!(
        write(&AttributeValue::M(Box::new(inner))),
        r#"{"M":{"k":{"BOOL":false}}}"#
    );
    assert_eq!(
        write(&AttributeValue::L(vec![
            AttributeValue::number(1u32),
            AttributeValue::string("a")
        ])),
        r#"{"L":[{"N":"1"},{"S":"a"}]}"#
    );
}

#[test]
fn writes_sets() {
    assert_eq!(
        write(&AttributeValue::Ss(vec![
            ByteStr::from_static("a"),
            ByteStr::from_static("b")
        ])),
        r#"{"SS":["a","b"]}"#
    );
    assert_eq!(
        write(&AttributeValue::Ns(vec![
            NumStr::from(1u32),
            NumStr::from(2u32)
        ])),
        r#"{"NS":["1","2"]}"#
    );
    assert_eq!(
        write(&AttributeValue::Bs(vec![Blob::from_slice(&[0xff])])),
        r#"{"BS":["/w=="]}"#
    );
}

/// A lock description is user text. It reaches a JSON string, so every character JSON reserves
/// has to come back out the way it went in.
#[test]
fn escapes_everything_json_reserves() {
    let mut writer = BodyWriter::new();
    writer.string("quote\" backslash\\ newline\n tab\t return\r bell\u{7} nul\u{0}");
    let written = take_body(&mut writer);

    assert_eq!(
        written,
        "\"quote\\\" backslash\\\\ newline\\n tab\\t return\\r bell\\u0007 nul\\u0000\""
    );

    let parsed: String = serde_json::from_str(&written).unwrap();
    assert_eq!(
        parsed,
        "quote\" backslash\\ newline\n tab\t return\r bell\u{7} nul\u{0}"
    );
}

#[test]
fn escapes_the_short_forms_for_backspace_and_form_feed() {
    let mut writer = BodyWriter::new();
    writer.string("\u{8}\u{c}");

    assert_eq!(take_body(&mut writer), r#""\b\f""#);
}

#[test]
fn passes_non_ascii_text_through_unchanged() {
    let mut writer = BodyWriter::new();
    writer.string("naïve 日本語 🎮");
    let written = take_body(&mut writer);

    assert_eq!(written, "\"naïve 日本語 🎮\"");
    assert_eq!(
        serde_json::from_str::<String>(&written).unwrap(),
        "naïve 日本語 🎮"
    );
}

#[test]
fn reads_each_attribute_type() {
    assert_eq!(
        parse_value(r#"{"B":"3q0="}"#).unwrap(),
        AttributeValue::blob([0xde, 0xad])
    );
    assert_eq!(
        parse_value(r#"{"S":"text"}"#).unwrap(),
        AttributeValue::string("text")
    );
    assert_eq!(
        parse_value(r#"{"N":"42"}"#).unwrap(),
        AttributeValue::number(42u32)
    );
    assert_eq!(
        parse_value(r#"{"BOOL":false}"#).unwrap(),
        AttributeValue::Bool(false)
    );
    assert_eq!(
        parse_value(r#"{"NULL":true}"#).unwrap(),
        AttributeValue::Null
    );
}

#[test]
fn reads_nested_maps_lists_and_sets() {
    let mut inner = Item::new();
    inner.insert(ByteStr::from_static("k"), AttributeValue::Bool(true));

    assert_eq!(
        parse_value(r#"{"M":{"k":{"BOOL":true}}}"#).unwrap(),
        AttributeValue::M(Box::new(inner))
    );
    assert_eq!(
        parse_value(r#"{"L":[{"N":"1"}]}"#).unwrap(),
        AttributeValue::L(vec![AttributeValue::number(1u32)])
    );
    assert_eq!(
        parse_value(r#"{"SS":["a"]}"#).unwrap(),
        AttributeValue::Ss(vec![ByteStr::from_static("a")])
    );
    assert_eq!(
        parse_value(r#"{"NS":["3"]}"#).unwrap(),
        AttributeValue::Ns(vec![NumStr::from(3u32)])
    );
    assert_eq!(
        parse_value(r#"{"BS":["/w=="]}"#).unwrap(),
        AttributeValue::Bs(vec![Blob::from_slice(&[0xff])])
    );
}

#[test]
fn every_attribute_type_round_trips() {
    let mut nested = Item::new();
    nested.insert(ByteStr::from_static("n"), AttributeValue::number(9u32));

    for value in [
        AttributeValue::blob([1u8, 2, 3]),
        AttributeValue::string("with \"escape\" and \n newline"),
        AttributeValue::number(u64::MAX),
        AttributeValue::Bool(true),
        AttributeValue::Null,
        AttributeValue::M(Box::new(nested)),
        AttributeValue::L(vec![AttributeValue::Null]),
        AttributeValue::Ss(vec![ByteStr::from_static("x")]),
        AttributeValue::Ns(vec![NumStr::from(7u32)]),
        AttributeValue::Bs(vec![Blob::from_slice(b"ab")]),
    ] {
        let written = write(&value);
        let read = parse_value(&written)
            .unwrap_or_else(|error| panic!("{written} failed to parse: {error}"));

        assert_eq!(read, value, "{written} did not round-trip");
    }
}

/// The allocation goal in one assertion: a string attribute is a slice of the response, not a
/// copy of it.
#[test]
fn text_read_from_a_response_shares_its_buffer() {
    let json = r#"{"owner":{"S":"a-long-owner-identifier"}}"#;
    let body = Bytes::copy_from_slice(json.as_bytes());
    let arena = Arena::new(body.clone());
    let mut deserializer = serde_json::Deserializer::from_slice(&body);

    let item = ItemSeed::new(&arena)
        .deserialize(&mut deserializer)
        .unwrap();

    let AttributeValue::S(text) = item.get("owner").unwrap() else {
        panic!("expected a string attribute");
    };
    let offset = json.find("a-long-owner").unwrap();
    assert_eq!(
        text.as_bytes().as_ptr(),
        body[offset..].as_ptr(),
        "the attribute must point into the response buffer"
    );
}

/// Attribute *names* are borrowed too, which is what removes the per-name allocation.
#[test]
fn attribute_names_share_the_response_buffer() {
    let json = r#"{"repository_context":{"N":"1"}}"#;
    let body = Bytes::copy_from_slice(json.as_bytes());
    let arena = Arena::new(body.clone());
    let mut deserializer = serde_json::Deserializer::from_slice(&body);

    let item = ItemSeed::new(&arena)
        .deserialize(&mut deserializer)
        .unwrap();

    let (name, _) = item.iter().next().unwrap();
    let offset = json.find("repository_context").unwrap();
    assert_eq!(name.as_bytes().as_ptr(), body[offset..].as_ptr());
}

/// A blob of the sizes Lore writes decodes without allocating.
#[test]
fn key_sized_blobs_decode_inline() {
    for encoded in [
        "AQEBAQEBAQEBAQEBAQEBAQ==",
        "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=",
    ] {
        let decoded = decode_blob(encoded).unwrap();

        assert!(
            decoded.is_inline(),
            "{} bytes must decode inline",
            decoded.len()
        );
    }
}

/// A blob longer than the inline bytes still decodes, on the heap.
#[test]
fn an_oversized_blob_spills() {
    let encoded = BASE64.encode([7u8; 64]);

    let decoded = decode_blob(&encoded).unwrap();

    assert_eq!(decoded.as_slice(), [7u8; 64]);
    assert!(!decoded.is_inline());
}

/// Decoding is exact for every size around the inline boundary, where the two paths meet.
#[test]
fn decoding_is_exact_across_the_inline_boundary() {
    for length in 0..=BLOB_SCRATCH + 8 {
        let original: Vec<u8> = (0..length).map(|byte| byte as u8).collect();
        let encoded = BASE64.encode(&original);

        let decoded = decode_blob(&encoded).unwrap();

        assert_eq!(decoded.as_slice(), original.as_slice(), "length {length}");
    }
}

/// An escaped string cannot be borrowed, so it is copied. It must still be correct.
#[test]
fn an_escaped_string_is_copied_and_correct() {
    let value = parse_value(r#"{"S":"line\nbreak"}"#).unwrap();

    assert_eq!(value, AttributeValue::string("line\nbreak"));
}

/// JSON may escape any character, `/` included, so every string the parser reads has to be taken
/// whether `serde_json` can lend it from the body or has to unescape it first.
#[test]
fn escaped_tags_and_values_read_like_plain_ones() {
    assert_eq!(
        parse_value(r#"{"\u0053":"text"}"#).unwrap(),
        AttributeValue::string("text"),
        "an escaped tag"
    );
    assert_eq!(
        parse_value(r#"{"B":"\/w=="}"#).unwrap(),
        AttributeValue::blob([0xff]),
        "an escaped `/` inside base64"
    );
    assert_eq!(
        parse_value(r#"{"N":"4\u0032"}"#).unwrap(),
        AttributeValue::number(42u32),
        "an escaped digit"
    );
    assert_eq!(
        parse_value(r#"{"BS":["\/w=="]}"#).unwrap(),
        AttributeValue::Bs(vec![Blob::from_slice(&[0xff])])
    );
    assert_eq!(
        parse_value(r#"{"NS":["4\u0032"]}"#).unwrap(),
        AttributeValue::Ns(vec![NumStr::from(42u32)])
    );
    assert_eq!(
        parse_value(r#"{"S":"first","\u0042OOL":true}"#).unwrap(),
        AttributeValue::string("first"),
        "an escaped surplus key"
    );
}

#[test]
fn a_missing_type_tag_is_an_error() {
    assert_eq!(parse_value("{}"), Err(ParseError::UntaggedAttribute));
}

#[test]
fn an_unknown_type_tag_is_an_error() {
    assert_eq!(
        parse_value(r#"{"Z":"x"}"#),
        Err(ParseError::UnknownAttributeType("Z".to_owned()))
    );
}

#[test]
fn bad_base64_is_an_error_not_a_panic() {
    for encoded in [
        r#"{"B":"not base64!!"}"#,
        r#"{"B":"AAA"}"#,
        r#"{"B":"====" }"#,
        r#"{"BS":["/w==","not base64!!"]}"#,
    ] {
        assert_eq!(
            parse_value(encoded),
            Err(ParseError::BadBase64),
            "{encoded} must be refused"
        );
    }
}

#[test]
fn a_wrongly_typed_value_is_an_error() {
    for json in [
        r#"{"S":7}"#,
        r#"{"N":7}"#,
        r#"{"BOOL":"true"}"#,
        r#"{"B":[1,2]}"#,
        r#"{"M":[]}"#,
        r#"{"L":{}}"#,
    ] {
        assert!(
            matches!(parse_value(json), Err(ParseError::Malformed(_))),
            "{json} must be refused"
        );
    }
}

#[test]
fn truncated_and_malformed_bodies_are_errors() {
    for json in [
        "",
        "{",
        r#"{"S""#,
        r#"{"S":"unterminated"#,
        "null",
        "[]",
        "7",
    ] {
        assert!(
            matches!(parse_value(json), Err(ParseError::Malformed(_))),
            "{json:?} must be refused"
        );
    }
}

/// A body is one document. Anything after it is a body that is not what the operation answers with.
#[test]
fn bytes_after_the_document_are_an_error() {
    for json in [r#"{"NULL":true}x"#, r#"{"NULL":true}{"NULL":true}"#] {
        assert!(
            matches!(parse_value(json), Err(ParseError::Malformed(_))),
            "{json:?} must be refused"
        );
    }

    assert_eq!(
        parse_value("{\"NULL\":true}\n"),
        Ok(AttributeValue::Null),
        "trailing whitespace is still one document"
    );
}

/// An empty body is not an empty answer: a read whose body did not arrive must not read as a miss.
#[test]
fn an_empty_body_is_malformed() {
    let parsed = parse_response(Bytes::new(), |deserializer, arena| {
        ItemSeed::new(arena).deserialize(deserializer)
    });

    assert!(matches!(parsed, Err(ParseError::Malformed(_))));
}

fn nested_lists(depth: usize) -> String {
    format!(
        "{}{}{}",
        r#"{"L":["#.repeat(depth),
        r#"{"NULL":true}"#,
        "]}".repeat(depth)
    )
}

fn nested_maps(depth: usize) -> String {
    format!(
        "{}{}{}",
        r#"{"M":{"a":"#.repeat(depth),
        r#"{"NULL":true}"#,
        "}}".repeat(depth)
    )
}

/// A hostile body must not turn into unbounded recursion, and the bound is this parser's own: a
/// value inside `MAX_DEPTH` lists or maps is read, and one level more is refused well inside
/// `serde_json`'s recursion limit of 128, which these documents stay below.
#[test]
fn nesting_past_the_bound_is_refused_rather_than_recursed() {
    for nested in [nested_lists, nested_maps] {
        assert!(parse_value(&nested(MAX_DEPTH)).is_ok());
        assert_eq!(
            parse_value(&nested(MAX_DEPTH + 1)),
            Err(ParseError::TooDeep)
        );
    }
}

#[test]
fn an_item_reads_its_attributes_in_wire_order() {
    let item = parse_item(r#"{"z":{"N":"1"},"a":{"N":"2"}}"#).unwrap();

    let names: Vec<&str> = item.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["z", "a"], "wire order is preserved");
}

#[test]
fn an_empty_item_reads_as_empty() {
    assert!(parse_item("{}").unwrap().is_empty());
}

/// Over-specified but unambiguous: the first tag wins, the rest is drained.
#[test]
fn a_surplus_tag_on_an_attribute_is_tolerated() {
    let value = parse_value(r#"{"S":"first","BOOL":true}"#).unwrap();

    assert_eq!(value, AttributeValue::string("first"));
}
