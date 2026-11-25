// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_aws::aws::error::CancellationReason;
use lore_aws::aws::item::ByteStr;
use lore_aws::dynamodb::cancellation_reason::*;

fn reason(code: Option<&'static str>) -> CancellationReason {
    CancellationReason {
        code: code.map(ByteStr::from_static),
        message: None,
        item: None,
    }
}

#[test]
fn no_code_is_not_interesting() {
    assert!(!interesting_cancellation_reason_filter(&&reason(None)));
}

#[test]
fn none_code_is_not_interesting() {
    assert!(!interesting_cancellation_reason_filter(&&reason(Some(
        "None"
    ))));
}

#[test]
fn a_code_is_interesting() {
    assert!(interesting_cancellation_reason_filter(&&reason(Some(
        "Hello"
    ))));
}
