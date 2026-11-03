// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::header::RANGE;
use lore_server::http::range::RangeRequest;
use lore_server::http::range::content_range;
use lore_server::http::range::parse_range;
use lore_server::http::range::range_request;
use lore_server::http::range::unsatisfied_range;
use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;

const TOTAL: u64 = 1000;

#[test]
fn a_closed_range_is_those_bytes() {
    assert_eq!(
        parse_range("bytes=0-99", TOTAL),
        RangeRequest::Partial(0..100)
    );
    assert_eq!(
        parse_range("bytes=500-599", TOTAL),
        RangeRequest::Partial(500..600)
    );
    assert_eq!(
        parse_range("bytes=999-999", TOTAL),
        RangeRequest::Partial(999..1000)
    );
}

#[test]
fn an_open_range_runs_to_the_end() {
    assert_eq!(
        parse_range("bytes=900-", TOTAL),
        RangeRequest::Partial(900..1000)
    );
}

#[test]
fn a_suffix_is_the_last_n_bytes() {
    assert_eq!(
        parse_range("bytes=-100", TOTAL),
        RangeRequest::Partial(900..1000)
    );
    assert_eq!(
        parse_range("bytes=-5000", TOTAL),
        RangeRequest::Partial(0..1000)
    );
}

#[test]
fn an_end_past_the_content_is_trimmed() {
    assert_eq!(
        parse_range("bytes=990-5000", TOTAL),
        RangeRequest::Partial(990..1000)
    );
    assert_eq!(
        parse_range("bytes=0-99999999999999999999999", TOTAL),
        RangeRequest::Partial(0..1000)
    );
}

#[test]
fn a_start_at_or_past_the_end_is_unsatisfiable() {
    assert_eq!(
        parse_range("bytes=1000-1010", TOTAL),
        RangeRequest::Unsatisfiable
    );
    assert_eq!(
        parse_range("bytes=2000-", TOTAL),
        RangeRequest::Unsatisfiable
    );
    assert_eq!(parse_range("bytes=-0", TOTAL), RangeRequest::Unsatisfiable);
    assert_eq!(parse_range("bytes=0-0", 0), RangeRequest::Unsatisfiable);
    assert_eq!(parse_range("bytes=-10", 0), RangeRequest::Unsatisfiable);
}

#[test]
fn anything_else_is_the_whole_content() {
    for value in [
        "bytes=0-9,100-109",
        "bytes=abc",
        "bytes=9-0",
        "bytes=-",
        "bytes=1-x",
        "bytes= -",
        "items=0-9",
        "0-9",
        "",
    ] {
        assert_eq!(parse_range(value, TOTAL), RangeRequest::Full, "{value:?}");
    }
}

#[test]
fn whitespace_around_the_spec_is_tolerated() {
    assert_eq!(
        parse_range("bytes= 10 - 19 ", TOTAL),
        RangeRequest::Partial(10..20)
    );
}

#[test]
fn no_header_is_the_whole_content() {
    assert_eq!(range_request(&HeaderMap::new(), TOTAL), RangeRequest::Full);
}

#[test]
fn content_range_values() {
    assert_eq!(content_range(&(0..100), TOTAL).unwrap(), "bytes 0-99/1000");
    assert_eq!(unsatisfied_range(TOTAL).unwrap(), "bytes */1000");
}

#[test]
fn numbers_at_the_limit_of_u64_do_not_overflow() {
    assert_eq!(
        parse_range("bytes=0-18446744073709551615", TOTAL),
        RangeRequest::Partial(0..1000)
    );
    assert_eq!(
        parse_range("bytes=-18446744073709551615", TOTAL),
        RangeRequest::Partial(0..1000)
    );
    assert_eq!(
        parse_range("bytes=18446744073709551615-", TOTAL),
        RangeRequest::Unsatisfiable
    );
    assert_eq!(
        parse_range("bytes=18446744073709551615-18446744073709551615", u64::MAX),
        RangeRequest::Unsatisfiable
    );
    assert_eq!(
        parse_range("bytes=18446744073709551614-", u64::MAX),
        RangeRequest::Partial(u64::MAX - 1..u64::MAX)
    );
}

#[test]
fn a_start_or_suffix_too_large_for_u64_is_the_whole_content() {
    for value in ["bytes=99999999999999999999-", "bytes=-99999999999999999999"] {
        assert_eq!(parse_range(value, TOTAL), RangeRequest::Full, "{value:?}");
    }
}

#[test]
fn signs_and_other_number_forms_are_the_whole_content() {
    for value in [
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
        "bytes=0-9 junk",
        "bytes=0-9;",
        "bytes =0-9",
        "bytes:0-9",
        "bytes=,0-9",
        "bytes=0-9,",
    ] {
        assert_eq!(parse_range(value, TOTAL), RangeRequest::Full, "{value:?}");
    }
}

#[test]
fn leading_zeros_are_numbers() {
    assert_eq!(
        parse_range("bytes=0010-0019", TOTAL),
        RangeRequest::Partial(10..20)
    );
}

#[test]
fn many_ranges_are_one_whole_response() {
    let overlapping = format!("bytes={}", vec!["0-999"; 10_000].join(","));
    let tiny = format!("bytes={}", vec!["0-0"; 10_000].join(","));
    for value in [overlapping, tiny] {
        assert_eq!(parse_range(&value, TOTAL), RangeRequest::Full);
    }
}

#[test]
fn a_huge_number_is_the_whole_content() {
    let value = format!("bytes={}-", "9".repeat(100_000));
    assert_eq!(parse_range(&value, TOTAL), RangeRequest::Full);
}

#[test]
fn a_header_that_is_not_text_is_the_whole_content() {
    let mut headers = HeaderMap::new();
    headers.insert(RANGE, HeaderValue::from_bytes(b"bytes=0-9\xff").unwrap());
    assert_eq!(range_request(&headers, TOTAL), RangeRequest::Full);
}

#[test]
fn several_range_headers_are_the_whole_content() {
    let mut headers = HeaderMap::new();
    headers.append(RANGE, HeaderValue::from_static("bytes=0-9"));
    headers.append(RANGE, HeaderValue::from_static("bytes=10-19"));
    assert_eq!(range_request(&headers, TOTAL), RangeRequest::Full);
}

#[test]
fn content_range_of_one_byte() {
    assert_eq!(
        content_range(&(999..1000), TOTAL).unwrap(),
        "bytes 999-999/1000"
    );
    assert_eq!(
        content_range(&(u64::MAX - 1..u64::MAX), u64::MAX).unwrap(),
        "bytes 18446744073709551614-18446744073709551614/18446744073709551615"
    );
}

#[test]
fn negative_numbers_are_the_whole_content() {
    for value in [
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
    ] {
        assert_eq!(parse_range(value, TOTAL), RangeRequest::Full, "{value:?}");
    }
}

#[test]
fn junk_is_the_whole_content() {
    for value in [
        "bytes=",
        "bytes",
        "=",
        "-",
        "bytes==0-9",
        "bytes=0-9-19",
        "bytes=0--",
        "bytes=0-9=",
        "bytes=0-9\u{0}",
        "bytes=0-9'; DROP TABLE content; --",
        "bytes=../../etc/passwd",
        "bytes=%30-%39",
        "bytes=\u{1f4a5}-\u{1f4a5}",
        "bytes=\u{7f}0-9",
        "bytes=\u{e9}-9",
    ] {
        assert_eq!(parse_range(value, TOTAL), RangeRequest::Full, "{value:?}");
    }
}

#[test]
fn random_values_never_yield_a_range_outside_the_content() {
    const ALPHABET: &[u8] = b"bytes=-,0123456789 +xX\t";
    let mut rng = StdRng::seed_from_u64(0x0052_414e_4745);
    for _ in 0..200_000 {
        let len = rng.random_range(0..24);
        let value: String = (0..len)
            .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
            .collect();
        let total = rng.random_range(0..4096);
        if let RangeRequest::Partial(range) = parse_range(&value, total) {
            assert!(range.start < range.end, "{value:?} of {total}");
            assert!(range.end <= total, "{value:?} of {total}");
        }
    }
}
