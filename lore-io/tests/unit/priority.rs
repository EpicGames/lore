// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The background disk priority a thread holds while a `BackgroundIo` lives.

use lore_io::priority::BackgroundIo;
use lore_io::priority::test_util::background_disk_priority;
use lore_io::priority::test_util::restorable_disk_priority;
use lore_io::priority::test_util::thread_disk_priority;

/// The guard gives the thread background disk priority and gives back the priority it had. The
/// priority during the guard is compared with the one the guard sets rather than with the one
/// before it, which is the same for a thread that already runs at the lowest level.
#[test]
fn background_io_gives_the_thread_background_disk_priority_and_restores_it() {
    let before = thread_disk_priority().expect("the system reports the thread's disk priority");

    let background = BackgroundIo::begin();
    let during = thread_disk_priority();
    drop(background);

    assert_eq!(during, background_disk_priority());
    assert_eq!(
        thread_disk_priority(),
        Some(restorable_disk_priority(before))
    );
}

/// A thread read as the no-priority class with a level is given the class alone, which
/// `ioprio_set` takes, and a best-effort priority is given back as it was.
#[cfg(target_os = "linux")]
#[test]
fn an_unset_linux_disk_priority_is_restored_as_the_class_alone() {
    const NONE_AT_NORMAL_LEVEL: i32 = 4;
    const BEST_EFFORT_NORMAL: i32 = (2 << 13) | 4;
    const BEST_EFFORT_LOWEST: i32 = (2 << 13) | 7;

    assert_eq!(restorable_disk_priority(NONE_AT_NORMAL_LEVEL), 0);
    assert_eq!(restorable_disk_priority(0), 0);
    assert_eq!(
        restorable_disk_priority(BEST_EFFORT_NORMAL),
        BEST_EFFORT_NORMAL
    );
    assert_eq!(
        restorable_disk_priority(BEST_EFFORT_LOWEST),
        BEST_EFFORT_LOWEST
    );
}
