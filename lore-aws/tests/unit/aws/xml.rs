// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::borrow::Cow;

use lore_aws::aws::xml::*;

#[test]
fn walks_a_document() {
    let mut reader = XmlReader::new("<a><b>text</b></a>");

    assert_eq!(reader.next_event().unwrap(), Some(XmlEvent::Open("a")));
    assert_eq!(reader.next_event().unwrap(), Some(XmlEvent::Open("b")));
    assert_eq!(
        reader.next_event().unwrap(),
        Some(XmlEvent::Text(Cow::Borrowed("text")))
    );
    assert_eq!(reader.next_event().unwrap(), Some(XmlEvent::Close("b")));
    assert_eq!(reader.next_event().unwrap(), Some(XmlEvent::Close("a")));
    assert_eq!(reader.next_event().unwrap(), None);
}

#[test]
fn skips_declarations_comments_and_attributes() {
    let mut reader = XmlReader::new(
        r#"<?xml version="1.0" encoding="UTF-8"?><!-- note --><Root xmlns="http://s3">x</Root>"#,
    );

    assert_eq!(reader.next_event().unwrap(), Some(XmlEvent::Open("Root")));
    assert_eq!(
        reader.next_event().unwrap(),
        Some(XmlEvent::Text(Cow::Borrowed("x")))
    );
    assert_eq!(reader.next_event().unwrap(), Some(XmlEvent::Close("Root")));
}

#[test]
fn a_self_closing_tag_opens_and_closes() {
    let mut reader = XmlReader::new("<Root><Empty/></Root>");

    assert_eq!(reader.next_event().unwrap(), Some(XmlEvent::Open("Root")));
    assert_eq!(reader.next_event().unwrap(), Some(XmlEvent::Open("Empty")));
    assert_eq!(reader.next_event().unwrap(), Some(XmlEvent::Close("Empty")));
    assert_eq!(reader.next_event().unwrap(), Some(XmlEvent::Close("Root")));
    assert_eq!(reader.next_event().unwrap(), None);
}

#[test]
fn an_empty_element_reads_as_empty_text() {
    let mut reader = XmlReader::new("<NextKeyMarker></NextKeyMarker>");
    assert_eq!(
        reader.next_event().unwrap(),
        Some(XmlEvent::Open("NextKeyMarker"))
    );

    assert_eq!(reader.text("NextKeyMarker").unwrap(), "");
}

#[test]
fn entities_resolve_and_plain_text_stays_borrowed() {
    assert!(matches!(unescape("plain"), Cow::Borrowed("plain")));
    assert_eq!(unescape("a&amp;b"), "a&b");
    assert_eq!(unescape("&lt;tag&gt;"), "<tag>");
    assert_eq!(unescape("&quot;q&apos;"), "\"q'");
    assert_eq!(unescape("&#65;&#x42;"), "AB");
}

/// A response holding an ampersand that is not an entity should not become a parse failure; the
/// character is unambiguous.
#[test]
fn a_bare_ampersand_survives() {
    assert_eq!(unescape("a & b"), "a & b");
    assert_eq!(unescape("&notanentity"), "&notanentity");
}

#[test]
fn an_unresolvable_numeric_entity_keeps_its_source_text() {
    assert_eq!(
        unescape("&#xD800;"),
        "&#xD800;",
        "a surrogate is not a char"
    );
    assert_eq!(unescape("&#99999999;"), "&#99999999;");
}

#[test]
fn parses_an_s3_error_body() {
    let body = concat!(
        r#"<?xml version="1.0" encoding="UTF-8"?>"#,
        "<Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message>",
        "<Key>abc</Key><RequestId>REQ123</RequestId><HostId>host</HostId></Error>",
    );

    let parsed = parse_error_body(body);

    assert_eq!(parsed.code.as_deref(), Some("NoSuchKey"));
    assert_eq!(
        parsed.message.as_deref(),
        Some("The specified key does not exist.")
    );
    assert_eq!(parsed.request_id.as_deref(), Some("REQ123"));
}

#[test]
fn parses_an_sts_error_body_through_its_wrapper() {
    let body = concat!(
        "<ErrorResponse><Error><Type>Sender</Type>",
        "<Code>InvalidIdentityToken</Code><Message>bad token</Message></Error>",
        "<RequestId>R1</RequestId></ErrorResponse>",
    );

    let parsed = parse_error_body(body);

    assert_eq!(parsed.code.as_deref(), Some("InvalidIdentityToken"));
    assert_eq!(parsed.message.as_deref(), Some("bad token"));
}

#[test]
fn a_body_that_is_not_xml_yields_nothing_rather_than_failing() {
    assert_eq!(parse_error_body(""), XmlErrorBody::default());
    assert_eq!(
        parse_error_body("<html><body>502 Bad Gateway</body></html>"),
        XmlErrorBody::default()
    );
}

/// The first error a reader meets walking `body`, or `None` when it reaches the end cleanly.
fn first_error(body: &str) -> Option<XmlError> {
    let mut reader = XmlReader::new(body);

    loop {
        match reader.next_event() {
            Ok(Some(_)) => {}
            Ok(None) => return None,
            Err(error) => return Some(error),
        }
    }
}

/// Some inputs yield events before failing; every one of them fails, and none panics.
#[test]
fn malformed_documents_are_errors_not_panics() {
    for (body, expected) in [
        ("<unterminated", XmlError::UnterminatedTag),
        ("<a><b", XmlError::UnterminatedTag),
        ("<!-- never closed", XmlError::UnterminatedDirective),
        ("<![CDATA[never closed", XmlError::UnterminatedDirective),
        ("<>", XmlError::EmptyTagName),
        ("</>", XmlError::EmptyTagName),
        ("<?xml", XmlError::UnterminatedDirective),
    ] {
        assert_eq!(first_error(body), Some(expected), "{body:?}");
    }
}

/// A CDATA section is text taken as written, `>` and all; it ends only at `]]>`.
#[test]
fn a_cdata_section_is_text_up_to_its_own_close() {
    let mut reader = XmlReader::new("<Key><![CDATA[a>b&amp;c]]></Key>");
    assert_eq!(reader.next_event().unwrap(), Some(XmlEvent::Open("Key")));

    assert_eq!(reader.text("Key").unwrap(), "a>b&amp;c");
    assert_eq!(reader.next_event().unwrap(), None);
}

/// Text interrupted by a comment, a CDATA section or a nested element is one value, not its last
/// piece.
#[test]
fn text_split_by_markup_is_joined() {
    for body in [
        "<Key>ab<!-- note -->cd</Key>",
        "<Key>ab<![CDATA[cd]]></Key>",
        "<Key>a<Nested>x</Nested>b<!-- note -->cd</Key>",
    ] {
        let mut reader = XmlReader::new(body);
        assert_eq!(reader.next_event().unwrap(), Some(XmlEvent::Open("Key")));

        assert_eq!(reader.text("Key").unwrap(), "abcd", "{body:?}");
    }
}

/// Text that arrives in one piece is lent from the document rather than copied.
#[test]
fn text_in_one_piece_stays_borrowed() {
    let mut reader = XmlReader::new("<Key>abcd</Key>");
    assert_eq!(reader.next_event().unwrap(), Some(XmlEvent::Open("Key")));

    assert!(matches!(reader.text("Key").unwrap(), Cow::Borrowed("abcd")));
}

/// A processing instruction ends at `?>`, so a `>` inside one does not end it early.
#[test]
fn a_processing_instruction_ends_at_its_own_close() {
    let mut reader = XmlReader::new("<?note a > b?><Root/>");

    assert_eq!(reader.next_event().unwrap(), Some(XmlEvent::Open("Root")));
}

#[test]
fn skip_element_steps_over_nesting() {
    let mut reader = XmlReader::new("<a><skip><deep><deeper/></deep></skip><keep>k</keep></a>");
    assert_eq!(reader.next_event().unwrap(), Some(XmlEvent::Open("a")));
    assert_eq!(reader.next_event().unwrap(), Some(XmlEvent::Open("skip")));

    reader.skip_element("skip").unwrap();

    assert_eq!(reader.next_event().unwrap(), Some(XmlEvent::Open("keep")));
    assert_eq!(reader.text("keep").unwrap(), "k");
}

/// A leaf that unexpectedly contains an element must not have that element's text read as its
/// own — that is how a `VersionId` could come back holding a nested value.
#[test]
fn text_ignores_a_nested_element() {
    let mut reader = XmlReader::new("<VersionId><Nested>wrong</Nested>right</VersionId>");
    assert_eq!(
        reader.next_event().unwrap(),
        Some(XmlEvent::Open("VersionId"))
    );

    assert_eq!(reader.text("VersionId").unwrap(), "right");
}

/// Deep nesting must not recurse; the reader is iterative, so this is a bound on time only.
#[test]
fn deep_nesting_does_not_overflow_the_stack() {
    let depth = 50_000;
    let body = format!("{}{}", "<a>".repeat(depth), "</a>".repeat(depth));

    let mut reader = XmlReader::new(&body);
    let mut events = 0usize;
    while let Ok(Some(_)) = reader.next_event() {
        events += 1;
    }

    assert_eq!(events, depth * 2);
}
