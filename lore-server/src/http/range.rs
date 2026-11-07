// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! The HTTP `Range` request header for the content endpoints (RFC 9110 §14).
//!
//! A single `bytes` range is served as `206 Partial Content`. Any other value,
//! including several ranges, several `Range` headers or another unit, is ignored
//! and the whole content is served with `200`.

use std::ops::Range;

use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::header::ACCEPT_RANGES;
use axum::http::header::CONTENT_RANGE;
use axum::http::header::InvalidHeaderValue;
use axum::http::header::RANGE;

/// The response a request's `Range` header asks for, for content `total` bytes long.
#[derive(Debug, PartialEq, Eq)]
pub enum RangeRequest {
    /// The whole content, with `200`.
    Full,
    /// These bytes, with `206`. The range is non-empty and within the content.
    Partial(Range<u64>),
    /// `416`: the range starts at or past the end of the content.
    Unsatisfiable,
}

/// Reads the `Range` header from `headers` for content `total` bytes long.
///
/// More than one `Range` header is ignored: a cache in front of the server
/// might honour a different one than the server would.
pub fn range_request(headers: &HeaderMap, total: u64) -> RangeRequest {
    let mut values = headers.get_all(RANGE).iter();
    match (values.next().map(HeaderValue::to_str), values.next()) {
        (Some(Ok(value)), None) => parse_range(value, total),
        _ => RangeRequest::Full,
    }
}

/// Parses a `Range` header value: `bytes=a-b`, `bytes=a-` or `bytes=-n`.
pub fn parse_range(value: &str, total: u64) -> RangeRequest {
    let Some(spec) = value.trim().strip_prefix("bytes=") else {
        return RangeRequest::Full;
    };
    let spec = spec.trim();
    if spec.contains(',') {
        return RangeRequest::Full;
    }
    let Some((first, last)) = spec.split_once('-') else {
        return RangeRequest::Full;
    };
    let (first, last) = (first.trim(), last.trim());
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());

    if first.is_empty() {
        if !digits(last) {
            return RangeRequest::Full;
        }
        let Ok(n) = last.parse::<u64>() else {
            return RangeRequest::Full;
        };
        if n == 0 || total == 0 {
            return RangeRequest::Unsatisfiable;
        }
        return RangeRequest::Partial(total.saturating_sub(n)..total);
    }

    if !digits(first) || !(last.is_empty() || digits(last)) {
        return RangeRequest::Full;
    }
    let Ok(start) = first.parse::<u64>() else {
        return RangeRequest::Full;
    };
    let end_inclusive = if last.is_empty() {
        u64::MAX
    } else {
        last.parse::<u64>().unwrap_or(u64::MAX)
    };
    if end_inclusive < start {
        return RangeRequest::Full;
    }
    if start >= total {
        return RangeRequest::Unsatisfiable;
    }
    RangeRequest::Partial(start..end_inclusive.saturating_add(1).min(total))
}

/// The `Content-Range` value `bytes start-end/total` of a `206`.
pub fn content_range(range: &Range<u64>, total: u64) -> Result<HeaderValue, InvalidHeaderValue> {
    HeaderValue::from_str(&format!("bytes {}-{}/{total}", range.start, range.end - 1))
}

/// The `Content-Range` value `bytes */total` of a `416`.
pub fn unsatisfied_range(total: u64) -> Result<HeaderValue, InvalidHeaderValue> {
    HeaderValue::from_str(&format!("bytes */{total}"))
}

/// Sets `Accept-Ranges: bytes`.
pub fn advertise_ranges(headers: &mut HeaderMap) {
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
}

/// Sets `Content-Range`.
pub fn set_content_range(headers: &mut HeaderMap, value: HeaderValue) {
    headers.insert(CONTENT_RANGE, value);
}
