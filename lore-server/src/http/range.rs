// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! The HTTP `Range` request header and the `Content-Range` response header (RFC 9110 §14).

use std::ops::Range;

use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::Method;
use axum::http::StatusCode;
use axum::http::header::ACCEPT_RANGES;
use axum::http::header::CONTENT_RANGE;
use axum::http::header::IF_RANGE;
use axum::http::header::InvalidHeaderValue;
use axum::http::header::RANGE;

/// The one byte range a request's `Range` header asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteRange {
    /// Bytes `first` through `last`: `first-last`, or `first-` with `last` at `u64::MAX`.
    Bounded { first: u64, last: u64 },
    /// The last `length` bytes: `-length`.
    Suffix { length: u64 },
}

/// The part of the content a response serves.
#[derive(Debug, PartialEq, Eq)]
pub enum Selection {
    /// The whole content.
    Full,
    /// These bytes of the content. The range is non-empty and within the content.
    Partial(Range<u64>),
    /// No byte of the content: the range starts at or past the end of the content, or is a suffix
    /// of zero bytes.
    Unsatisfiable,
}

/// The byte range the `Range` header in `headers` asks for; `None` for the whole content.
///
/// `Range` is ignored on a method other than `GET`, and with `If-Range`, since the response
/// carries no validator for it to match. More than one `Range` header is ignored: a cache in
/// front of the server might honour a different one than the server would.
pub fn requested_range(method: &Method, headers: &HeaderMap) -> Option<ByteRange> {
    if method != Method::GET || headers.contains_key(IF_RANGE) {
        return None;
    }
    let mut values = headers.get_all(RANGE).iter();
    match (values.next(), values.next()) {
        (Some(value), None) => parse_range(value.to_str().ok()?),
        _ => None,
    }
}

/// Parses a `Range` value of one `bytes` range, `first-last`, `first-` or `-length`, with no
/// whitespace inside it.
///
/// The unit is case-insensitive. A number too large for `u64` reads as `u64::MAX`. Any other
/// value, including several ranges, is `None`. [`select`] handles a `last` before `first`.
#[lore_macro::test_pub]
fn parse_range(value: &str) -> Option<ByteRange> {
    let (unit, spec) = value.trim().split_once('=')?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let (first, last) = spec.split_once('-')?;
    if first.is_empty() {
        return Some(ByteRange::Suffix {
            length: decimal(last)?,
        });
    }
    let first = decimal(first)?;
    let last = if last.is_empty() {
        u64::MAX
    } else {
        decimal(last)?
    };
    Some(ByteRange::Bounded { first, last })
}

/// The value of `digits`, one or more ASCII digits, saturated at `u64::MAX`; `None` for any other
/// string.
fn decimal(digits: &str) -> Option<u64> {
    (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| digits.parse().unwrap_or(u64::MAX))
}

/// The part of content `total` bytes long that `range` selects. Any range of empty content, and a
/// `Bounded` range whose `last` is before its `first`, select the whole content.
pub fn select(range: Option<ByteRange>, total: u64) -> Selection {
    let Some(range) = range.filter(|_| total > 0) else {
        return Selection::Full;
    };
    match range {
        ByteRange::Suffix { length: 0 } => Selection::Unsatisfiable,
        ByteRange::Suffix { length } => Selection::Partial(total.saturating_sub(length)..total),
        ByteRange::Bounded { first, last } if last < first => Selection::Full,
        ByteRange::Bounded { first, .. } if first >= total => Selection::Unsatisfiable,
        ByteRange::Bounded { first, last } => {
            Selection::Partial(first..last.saturating_add(1).min(total))
        }
    }
}

/// The range of the content to read for `selection`: `None` for the whole content, and an empty
/// range for no byte. A bound past `usize::MAX` reads as `usize::MAX`.
pub fn read_range(selection: &Selection) -> Option<Range<usize>> {
    let bound = |offset: u64| usize::try_from(offset).unwrap_or(usize::MAX);
    match selection {
        Selection::Full => None,
        Selection::Partial(range) => Some(bound(range.start)..bound(range.end)),
        Selection::Unsatisfiable => Some(0..0),
    }
}

/// Sets `Accept-Ranges` in `headers`, and the `Content-Range` of a partial or unsatisfiable
/// `selection` of content `total` bytes long, returning the response's status.
pub fn range_headers(
    selection: &Selection,
    total: u64,
    headers: &mut HeaderMap,
) -> Result<StatusCode, InvalidHeaderValue> {
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    let (status, content_range) = match selection {
        Selection::Full => return Ok(StatusCode::OK),
        Selection::Partial(range) => (
            StatusCode::PARTIAL_CONTENT,
            format!(
                "bytes {}-{}/{total}",
                range.start,
                range.end.saturating_sub(1)
            ),
        ),
        Selection::Unsatisfiable => (
            StatusCode::RANGE_NOT_SATISFIABLE,
            format!("bytes */{total}"),
        ),
    };
    headers.insert(CONTENT_RANGE, HeaderValue::try_from(content_range)?);
    Ok(status)
}
