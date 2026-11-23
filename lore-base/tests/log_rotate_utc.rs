// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Log rotation in a process whose local time zone is east of UTC. A binary of its own: it sets
//! the process's time zone.
use std::fs::File;
use std::time::Duration;
use std::time::SystemTime;

use lore_base::log::rotate::RotatingLogFile;
use lore_base::test_util::TempDir;

/// 2025-06-15T12:00:00Z, which is 2025-06-16 in the time zone the test sets.
const MODIFIED: u64 = 1_749_988_800;

/// A log file rotates by the UTC date, whatever the local time zone: a stale file takes the UTC
/// date it was written on, and the next rotation falls on a UTC midnight.
#[test]
fn a_log_file_rotates_by_the_utc_date_in_a_zone_east_of_utc() {
    // Safety: single-threaded, before anything reads the time zone, and nothing else in this
    // target reads the environment concurrently.
    unsafe { std::env::set_var("TZ", "LINT-14") };

    let dir = TempDir::new("lore-base-log-rotate-utc-");
    File::create(dir.child("lore.log"))
        .and_then(|log| log.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(MODIFIED)))
        .expect("a stale log file");

    let writer = RotatingLogFile::new(dir.path().to_path_buf(), "lore".to_string(), 8)
        .expect("the log file opens");

    assert!(
        dir.child("lore.log.2025-06-15").exists(),
        "the stale file takes the UTC date it was written on"
    );
    assert_eq!(
        writer.next_rotation_ts % 86_400,
        0,
        "the next rotation falls on a UTC midnight"
    );
}
