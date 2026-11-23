// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Timestamps written and read as text, in UTC.
//!
//! The forms are written digit by digit: a `time` format description costs several times as
//! much per timestamp.
use std::fmt::Write;

use time::OffsetDateTime;
use time::UtcDateTime;
use time::format_description::well_known::Rfc3339;

const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// The time `millis` milliseconds from the Unix epoch, or `None` outside the years -9999 to 9999.
pub fn from_unix_millis(millis: i64) -> Option<UtcDateTime> {
    UtcDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000).ok()
}

/// Milliseconds from the Unix epoch to `time`, rounded down.
pub fn unix_millis(time: UtcDateTime) -> i64 {
    time.unix_timestamp() * 1000 + i64::from(time.millisecond())
}

/// The date of `time` as `YYYY-MM-DD`, such as `2026-11-22`. A year outside 0 to 9999 takes a
/// sign, as in `-0001-12-31`.
pub fn date(time: UtcDateTime) -> String {
    let mut text = String::with_capacity(10);
    push_date(&mut text, time);
    text
}

/// `time` as an RFC 2822 date and time, such as `Sun, 1 Nov 2026 12:45:20 +0000`, or `None`
/// outside the years 0 to 9999, which the form can't hold.
pub fn rfc2822(time: UtcDateTime) -> Option<String> {
    let year = time.year();
    if !(0..=9999).contains(&year) {
        return None;
    }
    let mut text = String::with_capacity(31);
    text.push_str(WEEKDAYS[usize::from(time.weekday().number_days_from_monday())]);
    text.push_str(", ");
    let day = time.day();
    if day < 10 {
        push_digits::<1>(&mut text, u32::from(day));
    } else {
        push_digits::<2>(&mut text, u32::from(day));
    }
    text.push(' ');
    text.push_str(MONTHS[usize::from(u8::from(time.month()) - 1)]);
    text.push(' ');
    push_digits::<4>(&mut text, year.unsigned_abs());
    text.push(' ');
    push_time(&mut text, time);
    text.push_str(" +0000");
    Some(text)
}

/// `time` as an RFC 3339 date and time with the fewest of 0, 3, 6 or 9 fractional digits that
/// hold it, such as `2026-11-22T12:45:20.303350755+00:00`.
pub fn rfc3339(time: UtcDateTime) -> String {
    let mut text = String::with_capacity(35);
    push_date(&mut text, time);
    text.push('T');
    push_time(&mut text, time);
    let nanosecond = time.nanosecond();
    if nanosecond != 0 {
        text.push('.');
        if nanosecond.is_multiple_of(1_000_000) {
            push_digits::<3>(&mut text, nanosecond / 1_000_000);
        } else if nanosecond.is_multiple_of(1_000) {
            push_digits::<6>(&mut text, nanosecond / 1_000);
        } else {
            push_digits::<9>(&mut text, nanosecond);
        }
    }
    text.push_str("+00:00");
    text
}

/// Writes `time` as an RFC 3339 date and time with milliseconds, such as
/// `2026-11-22T12:45:20.303Z`.
pub fn push_rfc3339_millis(text: &mut String, time: UtcDateTime) {
    push_date(text, time);
    text.push('T');
    push_time(text, time);
    text.push('.');
    push_digits::<3>(text, u32::from(time.millisecond()));
    text.push('Z');
}

/// Milliseconds from the Unix epoch to the RFC 3339 date and time `text`, rounded down.
///
/// A leap second, `:60`, is read only at the last second of a month in UTC, and as the last
/// millisecond before the next minute, whatever its fraction. Anywhere else it is refused, and so
/// is an offset signed with U+2212 MINUS SIGN.
pub fn parse_rfc3339_millis(text: &str) -> Result<i64, time::error::Parse> {
    let time = OffsetDateTime::parse(text, &Rfc3339)?;
    Ok(time.unix_timestamp() * 1000 + i64::from(time.millisecond()))
}

fn push_date(text: &mut String, time: UtcDateTime) {
    let year = time.year();
    if (0..=9999).contains(&year) {
        push_digits::<4>(text, year.unsigned_abs());
    } else {
        let _ = write!(text, "{year:+05}");
    }
    text.push('-');
    push_digits::<2>(text, u32::from(u8::from(time.month())));
    text.push('-');
    push_digits::<2>(text, u32::from(time.day()));
}

fn push_time(text: &mut String, time: UtcDateTime) {
    push_digits::<2>(text, u32::from(time.hour()));
    text.push(':');
    push_digits::<2>(text, u32::from(time.minute()));
    text.push(':');
    push_digits::<2>(text, u32::from(time.second()));
}

/// Writes the `N` lowest decimal digits of `value`, padded with zeros.
#[inline]
fn push_digits<const N: usize>(text: &mut String, mut value: u32) {
    let mut digits = [b'0'; N];
    for digit in digits.iter_mut().rev() {
        *digit += (value % 10) as u8;
        value /= 10;
    }
    for digit in digits {
        text.push(char::from(digit));
    }
}
