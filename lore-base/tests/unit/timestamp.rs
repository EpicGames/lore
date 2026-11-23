// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::timestamp::*;
use time::UtcDateTime;

/// 2026-11-22T12:45:20.303Z.
const MILLIS: i64 = 1_795_351_520_303;

fn at_nanos(nanos: i128) -> UtcDateTime {
    UtcDateTime::from_unix_timestamp_nanos(nanos).expect("the time is in range")
}

fn at_millis(millis: i64) -> UtcDateTime {
    from_unix_millis(millis).expect("the time is in range")
}

#[test]
fn from_unix_millis_covers_the_years_time_represents() {
    assert_eq!(
        from_unix_millis(-1).map(unix_millis),
        Some(-1),
        "a time before the epoch keeps its millisecond"
    );
    assert_eq!(from_unix_millis(MILLIS).map(unix_millis), Some(MILLIS));
    assert!(
        from_unix_millis(253_402_300_800_000).is_none(),
        "the year 10000"
    );
    assert!(from_unix_millis(i64::MIN).is_none());
}

#[test]
fn unix_millis_rounds_down() {
    assert_eq!(unix_millis(at_nanos(1_795_351_520_303_999_999)), MILLIS);
    assert_eq!(unix_millis(at_nanos(-1)), -1);
}

#[test]
fn date_is_year_month_day() {
    assert_eq!(date(at_millis(MILLIS)), "2026-11-22");
    assert_eq!(date(at_millis(-1)), "1969-12-31");
    assert_eq!(date(at_millis(-62_167_219_200_000)), "0000-01-01");
}

#[test]
fn date_signs_a_year_before_zero() {
    assert_eq!(date(at_millis(-62_167_219_200_001)), "-0001-12-31");
}

#[test]
fn rfc2822_writes_the_day_without_padding() {
    assert_eq!(
        rfc2822(at_millis(MILLIS)).as_deref(),
        Some("Sun, 22 Nov 2026 12:45:20 +0000")
    );
    assert_eq!(
        rfc2822(at_millis(1_793_491_200_000)).as_deref(),
        Some("Sun, 1 Nov 2026 00:00:00 +0000")
    );
}

#[test]
fn rfc2822_covers_the_years_zero_to_9999() {
    assert_eq!(
        rfc2822(at_millis(-62_167_219_200_000)).as_deref(),
        Some("Sat, 1 Jan 0000 00:00:00 +0000")
    );
    assert_eq!(
        rfc2822(at_millis(253_402_300_799_999)).as_deref(),
        Some("Fri, 31 Dec 9999 23:59:59 +0000")
    );
    assert_eq!(
        rfc2822(at_millis(-1)).as_deref(),
        Some("Wed, 31 Dec 1969 23:59:59 +0000")
    );
    assert_eq!(rfc2822(at_millis(-62_167_219_200_001)), None, "the year -1");
}

#[test]
fn rfc3339_writes_the_fewest_fractional_digits_that_hold_the_time() {
    let second = 1_795_351_520_000_000_000;
    for (nanos, expected) in [
        (0, "2026-11-22T12:45:20+00:00"),
        (303_000_000, "2026-11-22T12:45:20.303+00:00"),
        (5_000_000, "2026-11-22T12:45:20.005+00:00"),
        (303_350_000, "2026-11-22T12:45:20.303350+00:00"),
        (50_000, "2026-11-22T12:45:20.000050+00:00"),
        (303_350_755, "2026-11-22T12:45:20.303350755+00:00"),
    ] {
        assert_eq!(rfc3339(at_nanos(second + nanos)), expected);
    }
}

#[test]
fn push_rfc3339_millis_writes_three_fractional_digits() {
    let mut text = String::from("[");
    push_rfc3339_millis(&mut text, at_nanos(1_795_351_520_303_350_755));
    assert_eq!(text, "[2026-11-22T12:45:20.303Z");

    let mut text = String::new();
    push_rfc3339_millis(&mut text, at_millis(1_795_351_520_000));
    assert_eq!(text, "2026-11-22T12:45:20.000Z");
}

#[test]
fn parse_rfc3339_millis_reads_what_rfc3339_writes() {
    for written in [
        "2026-11-22T12:45:20.303+00:00",
        "2026-11-22T12:45:20.303350+00:00",
        "2026-11-22T12:45:20.303350755+00:00",
    ] {
        assert_eq!(
            parse_rfc3339_millis(written).ok(),
            Some(MILLIS),
            "{written}"
        );
    }
    assert_eq!(
        parse_rfc3339_millis("2026-11-22T12:45:20+00:00").ok(),
        Some(1_795_351_520_000)
    );
    let now = UtcDateTime::now();
    assert_eq!(
        parse_rfc3339_millis(&rfc3339(now)).ok(),
        Some(unix_millis(now))
    );
}

#[test]
fn parse_rfc3339_millis_reads_other_rfc3339_forms() {
    for text in [
        "2026-11-22T12:45:20.303Z",
        "2026-11-22t12:45:20.303z",
        "2026-11-22 12:45:20.303Z",
        "2026-11-22T18:15:20.303+05:30",
        "2026-11-22T12:45:20.3033507559999Z",
    ] {
        assert_eq!(parse_rfc3339_millis(text).ok(), Some(MILLIS), "{text}");
    }
    assert_eq!(
        parse_rfc3339_millis("1969-12-31T23:59:59.9995Z").ok(),
        Some(-1),
        "a time before the epoch rounds down"
    );
    assert_eq!(
        parse_rfc3339_millis("9999-12-31T23:59:59-01:00").ok(),
        Some(253_402_304_399_000),
        "the offset takes the time past the year 9999"
    );
    assert_eq!(
        parse_rfc3339_millis("0000-01-01T00:00:00Z").ok(),
        Some(-62_167_219_200_000)
    );
}

#[test]
fn parse_rfc3339_millis_refuses_what_is_not_rfc3339() {
    for text in [
        "2026-11-22T12:45:20",
        "2026-11-22T12:45:20+0000",
        "2026-11-22T12:45:20Z ",
        "2026-02-29T00:00:00Z",
        "",
    ] {
        assert!(parse_rfc3339_millis(text).is_err(), "{text:?}");
    }
}

#[test]
fn parse_rfc3339_millis_reads_a_leap_second_only_at_the_end_of_a_month() {
    for text in [
        "2026-11-30T23:59:60Z",
        "2026-11-30T23:59:60.5Z",
        "2026-12-01T00:59:60+01:00",
    ] {
        assert_eq!(
            parse_rfc3339_millis(text).ok(),
            Some(1_796_083_199_999),
            "{text}"
        );
    }
    assert!(parse_rfc3339_millis("2026-11-22T23:59:60Z").is_err());
    assert!(parse_rfc3339_millis("2026-11-30T23:59:60+01:00").is_err());
}

#[test]
fn parse_rfc3339_millis_refuses_an_offset_signed_with_a_minus_sign_character() {
    assert!(parse_rfc3339_millis("2026-11-22T12:45:20\u{2212}01:00").is_err());
    assert_eq!(
        parse_rfc3339_millis("2026-11-22T12:45:20-01:00").ok(),
        Some(1_795_355_120_000)
    );
}
