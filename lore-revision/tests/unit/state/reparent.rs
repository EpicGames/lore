// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::types::Hash;
use lore_revision::state::State;

fn current() -> Hash {
    Hash::from_u64(1)
}

fn remote_latest() -> Hash {
    Hash::from_u64(2)
}

fn previous() -> Hash {
    Hash::from_u64(6)
}

fn other_branch_latest() -> Hash {
    Hash::from_u64(3)
}

fn metadata() -> Hash {
    Hash::from_u64(4)
}

/// A staged merge with its own signature, as `merge start` leaves it, on `parent_self` and
/// `parent_other`.
fn staged_merge(parent_self: Hash, parent_other: Hash) -> Arc<State> {
    let state = State::new();
    state.set_revision(Hash::from_u64(5));
    state.set_parent_self(parent_self);
    state.set_parent_other(parent_other);
    state.set_metadata_hash(metadata());
    state
}

/// The merge a sync stages for divergent history has the remote latest first and the current
/// revision second. Staging on top of it keeps both, or the revision it commits does not descend
/// from the remote latest.
#[test]
fn reparenting_a_merge_of_divergent_history_keeps_the_remote_latest_first() {
    let state = staged_merge(remote_latest(), current());

    state.reparent_onto(current());

    assert_eq!(state.parent_self(), remote_latest());
    assert_eq!(state.parent_other(), current());
    assert!(state.is_merge());
    assert_eq!(state.metadata_hash(), metadata());
}

/// Only the current revision as the second parent marks a merge of divergent history. A merge of
/// another branch staged on a revision the branch has since moved past moves to the current
/// revision, as any staged state does.
#[test]
fn reparenting_a_merge_of_another_branch_moves_its_first_parent_to_the_current_revision() {
    let state = staged_merge(previous(), other_branch_latest());

    state.reparent_onto(current());

    assert_eq!(state.parent_self(), current());
    assert_eq!(state.parent_other(), other_branch_latest());
}

/// The current revision staged on top of itself is a new revision: the merge it records is not
/// the new revision's, and neither is its metadata.
#[test]
fn reparenting_the_current_revision_drops_its_second_parent_and_metadata() {
    let state = State::new();
    state.set_revision(current());
    state.set_revision_number(7);
    state.set_parent_self(previous());
    state.set_parent_other(other_branch_latest());
    state.set_metadata_hash(metadata());

    state.reparent_onto(current());

    assert_eq!(state.parent_self(), current());
    assert!(state.parent_other().is_zero());
    assert!(!state.is_merge());
    assert!(state.metadata_hash().is_zero());
    assert_eq!(state.revision_number(), 0);
}

/// A staged state reparented onto its own signature, as a nested link is onto the staged pin its
/// parent holds, is left as it is: the signature is no revision, and the parents and metadata are
/// the state's own.
#[test]
fn reparenting_a_staged_state_onto_its_own_signature_leaves_it_as_it_is() {
    let state = staged_merge(previous(), other_branch_latest());

    state.reparent_onto(state.revision());

    assert_eq!(state.parent_self(), previous());
    assert_eq!(state.parent_other(), other_branch_latest());
    assert_eq!(state.metadata_hash(), metadata());
}

fn new_branch_head() -> Hash {
    Hash::from_u64(8)
}

/// On a new branch, a merge of divergent history staged on the revision the branch replaces
/// merges the remote latest into the branch: the new head first, so the branch's first-parent line
/// reaches its branch point, and the remote latest second.
#[test]
fn reparenting_a_merge_of_divergent_history_onto_a_new_branch_merges_the_remote_latest_into_it() {
    let state = staged_merge(remote_latest(), current());

    state.reparent_replacing(current(), new_branch_head());

    assert_eq!(state.parent_self(), new_branch_head());
    assert_eq!(state.parent_other(), remote_latest());
    assert!(state.is_merge());
    assert_eq!(state.metadata_hash(), metadata());
}

/// A merge of another branch moves onto the new branch's head and keeps the branch it merged.
#[test]
fn reparenting_a_merge_of_another_branch_onto_a_new_branch_keeps_its_second_parent() {
    let state = staged_merge(current(), other_branch_latest());

    state.reparent_replacing(current(), new_branch_head());

    assert_eq!(state.parent_self(), new_branch_head());
    assert_eq!(state.parent_other(), other_branch_latest());
}
