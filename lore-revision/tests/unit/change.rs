// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::change::*;

#[test]
fn dirty_flag_exists_and_is_independent() {
    let dirty = Flags::Dirty;
    let staged = Flags::Staged;
    // Dirty and Staged don't overlap
    assert_eq!(dirty & staged, Flags::None);
}

#[test]
fn is_dirty_method() {
    let flags = Flags::Dirty;
    assert!(flags.is_dirty());
    assert!(!flags.is_stage());

    let flags = Flags::Staged;
    assert!(!flags.is_dirty());
    assert!(flags.is_stage());

    let flags = Flags::Dirty | Flags::Staged;
    assert!(flags.is_dirty());
    assert!(flags.is_stage());
}

#[test]
fn dirty_and_modify_combine() {
    let flags = Flags::Dirty | Flags::Modify;
    assert!(flags.is_dirty());
    assert!(flags.contains(Flags::Modify));
    assert!(!flags.is_stage());
}

/// A conflict stays unresolved until it is resolved or one side is taken, as a merge takes the
/// incoming side at a path the view excludes.
#[test]
fn a_conflict_is_unresolved_until_resolved_or_a_side_is_taken() {
    assert!(Flags::Conflict.is_conflict_unresolved());
    for settled in [
        Flags::ConflictResolved,
        Flags::ConflictMine,
        Flags::ConflictTheirs,
        Flags::ConflictResolved | Flags::ConflictTheirs,
    ] {
        assert!(!settled.is_conflict_unresolved(), "{settled:?}");
    }
    assert!(!Flags::Merge.is_conflict_unresolved());
}
