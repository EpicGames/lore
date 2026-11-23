// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_aws::store::lock_store::LockEntry;
use lore_base::types::LockData;

fn locked_at(timestamp: &str) -> Option<u64> {
    LockData::try_from(LockEntry {
        timestamp: timestamp.to_string(),
        ..LockEntry::default()
    })
    .ok()
    .map(|lock| lock.locked_at)
}

/// Lock records are shared by servers of every version, so a record's time is read with 0, 3, 6 or
/// 9 fractional digits.
#[test]
fn a_lock_record_time_is_read_as_any_version_writes_it() {
    for timestamp in [
        "2026-11-22T12:45:20.303+00:00",
        "2026-11-22T12:45:20.303350+00:00",
        "2026-11-22T12:45:20.303350755+00:00",
    ] {
        assert_eq!(locked_at(timestamp), Some(1_795_351_520_303), "{timestamp}");
    }
    assert_eq!(
        locked_at("2026-11-22T12:45:20+00:00"),
        Some(1_795_351_520_000)
    );
}

#[test]
fn a_lock_record_time_that_is_not_rfc3339_is_refused() {
    assert_eq!(locked_at("Sun, 22 Nov 2026 12:45:20 +0000"), None);
    assert_eq!(locked_at(""), None);
}
