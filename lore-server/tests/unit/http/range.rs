// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::Method;
use axum::http::StatusCode;
use axum::http::header::ACCEPT_RANGES;
use axum::http::header::CONTENT_RANGE;
use axum::http::header::IF_RANGE;
use axum::http::header::RANGE;
use lore_server::http::range::ByteRange;
use lore_server::http::range::Selection;
use lore_server::http::range::parse_range;
use lore_server::http::range::range_headers;
use lore_server::http::range::requested_range;
use lore_server::http::range::select;
use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;

const TOTAL: u64 = 1000;

/// The part of content `total` bytes long that the `Range` value `value` selects.
fn selected(value: &str, total: u64) -> Selection {
    select(parse_range(value), total)
}

#[test]
fn each_form_parses_to_its_byte_range() {
    assert_eq!(
        parse_range("bytes=10-19"),
        Some(ByteRange::Bounded {
            first: 10,
            last: 19
        })
    );
    assert_eq!(
        parse_range("bytes=10-"),
        Some(ByteRange::Bounded {
            first: 10,
            last: u64::MAX
        })
    );
    assert_eq!(
        parse_range("bytes=-5"),
        Some(ByteRange::Suffix { length: 5 })
    );
}

#[test]
fn a_closed_range_is_those_bytes() {
    assert_eq!(selected("bytes=0-99", TOTAL), Selection::Partial(0..100));
    assert_eq!(
        selected("bytes=500-599", TOTAL),
        Selection::Partial(500..600)
    );
    assert_eq!(
        selected("bytes=999-999", TOTAL),
        Selection::Partial(999..1000)
    );
}

#[test]
fn an_open_range_runs_to_the_end() {
    assert_eq!(selected("bytes=900-", TOTAL), Selection::Partial(900..1000));
}

#[test]
fn a_suffix_is_the_last_n_bytes() {
    assert_eq!(selected("bytes=-100", TOTAL), Selection::Partial(900..1000));
    assert_eq!(selected("bytes=-5000", TOTAL), Selection::Partial(0..1000));
}

#[test]
fn an_end_past_the_content_is_trimmed() {
    assert_eq!(
        selected("bytes=990-5000", TOTAL),
        Selection::Partial(990..1000)
    );
    assert_eq!(
        selected("bytes=0-99999999999999999999999", TOTAL),
        Selection::Partial(0..1000)
    );
}

#[test]
fn a_range_selecting_no_byte_is_unsatisfiable() {
    assert_eq!(selected("bytes=1000-1010", TOTAL), Selection::Unsatisfiable);
    assert_eq!(selected("bytes=2000-", TOTAL), Selection::Unsatisfiable);
    assert_eq!(selected("bytes=-0", TOTAL), Selection::Unsatisfiable);
}

#[test]
fn an_inverted_range_is_the_whole_content() {
    assert_eq!(selected("bytes=9-0", TOTAL), Selection::Full);
    assert_eq!(
        select(Some(ByteRange::Bounded { first: 5, last: 2 }), TOTAL),
        Selection::Full
    );
}

#[test]
fn any_range_of_empty_content_is_the_whole_content() {
    for value in ["bytes=0-0", "bytes=0-", "bytes=-10", "bytes=-0"] {
        assert_eq!(selected(value, 0), Selection::Full, "{value:?}");
    }
}

#[test]
fn the_unit_is_case_insensitive() {
    for value in ["Bytes=0-9", "BYTES=0-9"] {
        assert_eq!(
            selected(value, TOTAL),
            Selection::Partial(0..10),
            "{value:?}"
        );
    }
}

#[test]
fn anything_else_is_the_whole_content() {
    for value in [
        "bytes=0-9,100-109",
        "bytes=,0-9",
        "bytes=0-9,",
        "bytes=-5,",
        "bytes=abc",
        "bytes=-",
        "bytes=1-x",
        "bytes= -",
        "items=0-9",
        "0-9",
        "",
        "bytes=+5-10",
        "bytes=5-+10",
        "bytes=-+5",
        "bytes=--5",
        "bytes=-5-10",
        "bytes=5--10",
        "bytes=0x10-0x20",
        "bytes=1e3-",
        "bytes=1.5-2",
        "bytes=\u{ff11}-\u{ff12}",
        "bytes=\u{0661}-\u{0669}",
        "bytes=-100-",
        "bytes=-100-200",
        "bytes=10--1",
        "bytes=-10--1",
        "bytes=-0-5",
        "bytes=0--9223372036854775808",
        "bytes=-9223372036854775808-",
        "bytes=\u{2212}5",
        "bytes=0\u{2212}9",
        "bytes=\u{2013}5",
        "bytes=",
        "bytes",
        "bytes=0-9 junk",
        "bytes=0-9;",
        "bytes =0-9",
        "bytes:0-9",
        "=",
        "-",
        "bytes==0-9",
        "bytes=0-9-19",
        "bytes=0--",
        "bytes=0-9=",
        "bytes=0-9\u{0}",
        "bytes=%30-%39",
        "bytes=\u{1f4a5}-\u{1f4a5}",
        "bytes=\u{7f}0-9",
        "bytes=\u{e9}-9",
    ] {
        assert_eq!(selected(value, TOTAL), Selection::Full, "{value:?}");
    }
}

#[test]
fn whitespace_is_skipped_only_around_the_value() {
    assert_eq!(selected(" bytes=10-19 ", TOTAL), Selection::Partial(10..20));
    for value in [
        "bytes= 10-19",
        "bytes=10 -19",
        "bytes=10- 19",
        "bytes=10-19 junk",
        "bytes=- 5",
        "bytes= 10 - 19",
        "bytes=10\t-19",
    ] {
        assert_eq!(selected(value, TOTAL), Selection::Full, "{value:?}");
    }
}

#[test]
fn no_header_is_the_whole_content() {
    assert_eq!(requested_range(&Method::GET, &HeaderMap::new()), None);
}

#[test]
fn a_range_on_a_method_other_than_get_is_the_whole_content() {
    let mut headers = HeaderMap::new();
    headers.insert(RANGE, HeaderValue::from_static("bytes=0-9"));
    assert_eq!(
        requested_range(&Method::GET, &headers),
        Some(ByteRange::Bounded { first: 0, last: 9 })
    );
    assert_eq!(requested_range(&Method::HEAD, &headers), None);
}

#[test]
fn a_range_with_if_range_is_the_whole_content() {
    let mut headers = HeaderMap::new();
    headers.insert(RANGE, HeaderValue::from_static("bytes=0-9"));
    headers.insert(IF_RANGE, HeaderValue::from_static("\"etag\""));
    assert_eq!(requested_range(&Method::GET, &headers), None);
}

#[test]
fn range_headers_of_each_selection() {
    let cases = [
        (Selection::Full, TOTAL, StatusCode::OK, None),
        (
            Selection::Partial(0..100),
            TOTAL,
            StatusCode::PARTIAL_CONTENT,
            Some("bytes 0-99/1000"),
        ),
        (
            Selection::Partial(999..1000),
            TOTAL,
            StatusCode::PARTIAL_CONTENT,
            Some("bytes 999-999/1000"),
        ),
        (
            Selection::Partial(u64::MAX - 1..u64::MAX),
            u64::MAX,
            StatusCode::PARTIAL_CONTENT,
            Some("bytes 18446744073709551614-18446744073709551614/18446744073709551615"),
        ),
        (
            Selection::Unsatisfiable,
            TOTAL,
            StatusCode::RANGE_NOT_SATISFIABLE,
            Some("bytes */1000"),
        ),
    ];
    for (selection, total, status, content_range) in cases {
        let mut headers = HeaderMap::new();
        assert_eq!(
            range_headers(&selection, total, &mut headers).unwrap(),
            status,
            "{selection:?}"
        );
        assert_eq!(headers[ACCEPT_RANGES], "bytes", "{selection:?}");
        assert_eq!(
            headers
                .get(CONTENT_RANGE)
                .map(|value| value.to_str().unwrap()),
            content_range,
            "{selection:?}"
        );
    }
}

#[test]
fn numbers_at_the_limit_of_u64_do_not_overflow() {
    assert_eq!(
        selected("bytes=0-18446744073709551615", TOTAL),
        Selection::Partial(0..1000)
    );
    assert_eq!(
        selected("bytes=-18446744073709551615", TOTAL),
        Selection::Partial(0..1000)
    );
    assert_eq!(
        selected("bytes=18446744073709551615-", TOTAL),
        Selection::Unsatisfiable
    );
    assert_eq!(
        selected("bytes=18446744073709551615-18446744073709551615", u64::MAX),
        Selection::Unsatisfiable
    );
    assert_eq!(
        selected("bytes=18446744073709551614-", u64::MAX),
        Selection::Partial(u64::MAX - 1..u64::MAX)
    );
}

#[test]
fn a_start_too_large_for_u64_is_unsatisfiable() {
    let huge = format!("bytes={}-", "9".repeat(100_000));
    for value in ["bytes=99999999999999999999-", huge.as_str()] {
        assert_eq!(
            selected(value, TOTAL),
            Selection::Unsatisfiable,
            "{} bytes",
            value.len()
        );
    }
}

#[test]
fn a_suffix_too_large_for_u64_is_all_bytes() {
    assert_eq!(
        selected("bytes=-99999999999999999999", TOTAL),
        Selection::Partial(0..TOTAL)
    );
}

#[test]
fn leading_zeros_are_numbers() {
    assert_eq!(
        selected("bytes=0010-0019", TOTAL),
        Selection::Partial(10..20)
    );
}

#[test]
fn many_ranges_are_one_whole_response() {
    let overlapping = format!("bytes={}", vec!["0-999"; 10_000].join(","));
    let tiny = format!("bytes={}", vec!["0-0"; 10_000].join(","));
    for value in [overlapping, tiny] {
        assert_eq!(selected(&value, TOTAL), Selection::Full);
    }
}

#[test]
fn a_header_that_is_not_text_is_the_whole_content() {
    let mut headers = HeaderMap::new();
    headers.insert(RANGE, HeaderValue::from_bytes(b"bytes=0-9\xff").unwrap());
    assert_eq!(requested_range(&Method::GET, &headers), None);
}

#[test]
fn several_range_headers_are_the_whole_content() {
    let mut headers = HeaderMap::new();
    headers.append(RANGE, HeaderValue::from_static("bytes=0-9"));
    headers.append(RANGE, HeaderValue::from_static("bytes=10-19"));
    assert_eq!(requested_range(&Method::GET, &headers), None);
}

#[test]
fn random_ranges_stay_within_the_content() {
    const ALPHABET: &[u8] = b"-,0123456789 +xX\t";
    let mut rng = StdRng::seed_from_u64(0x0052_414e_4745);
    let mut partial = 0;
    for _ in 0..200_000 {
        let len = rng.random_range(0..24);
        let value: String = "bytes="
            .chars()
            .chain((0..len).map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char))
            .collect();
        let total = rng.random_range(0..4096);
        if let Selection::Partial(range) = selected(&value, total) {
            assert!(range.start < range.end, "{value:?} of {total}");
            assert!(range.end <= total, "{value:?} of {total}");
            partial += 1;
        }
    }
    assert!(partial > 0);
}
