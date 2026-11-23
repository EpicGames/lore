// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::time::Duration;
use std::time::SystemTime;

use lore_base::log::rotate::date_label;
use lore_base::log::rotate::next_midnight_ts;

/// 2026-11-22T12:45:20Z.
const NOON: i64 = 1_795_351_520;
/// 2026-11-23T00:00:00Z.
const MIDNIGHT: i64 = 1_795_392_000;

#[test]
fn the_next_rotation_is_the_next_utc_midnight() {
    assert_eq!(next_midnight_ts(NOON), MIDNIGHT);
    assert_eq!(next_midnight_ts(MIDNIGHT - 1), MIDNIGHT);
    assert_eq!(
        next_midnight_ts(MIDNIGHT),
        MIDNIGHT + 86_400,
        "a time on midnight rotates at the next one"
    );
    assert_eq!(next_midnight_ts(-1), 0);
}

#[test]
fn a_file_is_labelled_by_the_utc_date_it_was_modified_on() {
    let at = |seconds: i64| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds as u64);
    assert_eq!(date_label(at(NOON)).as_deref(), Some("2026-11-22"));
    assert_eq!(date_label(at(MIDNIGHT - 1)).as_deref(), Some("2026-11-22"));
    assert_eq!(date_label(at(MIDNIGHT)).as_deref(), Some("2026-11-23"));
    assert_eq!(
        date_label(SystemTime::UNIX_EPOCH - Duration::from_secs(1)),
        None,
        "a time before the epoch"
    );
}
