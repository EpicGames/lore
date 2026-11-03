// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_revision::change::NodeChange;
use lore_revision::filter::FilterMode;
use lore_revision::link;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision::diff::*;
use lore_revision::state::State;
use lore_revision::util::path::RelativePath;

use crate::fs::filesystem_provider::setup_test_execution;
use crate::fs::filesystem_provider::test_store_create;

fn paths(values: &[&str]) -> Vec<RelativePath> {
    values
        .iter()
        .map(|value| RelativePath::from_str(value).expect("valid path"))
        .collect()
}

#[test]
fn unfiltered_diff_includes_every_link() {
    assert!(link_path_in_scope("libs/shared", None));
    assert!(link_path_in_scope("libs/shared", Some(&[])));
}

#[test]
fn link_at_the_requested_path_is_in_scope() {
    assert!(link_path_in_scope(
        "libs/shared",
        Some(&paths(&["libs/shared"]))
    ));
}

#[test]
fn link_below_the_requested_path_is_in_scope() {
    assert!(link_path_in_scope("libs/shared", Some(&paths(&["libs"]))));
}

/// A request scoped inside a link asks for that subtree, not for the
/// link's own entry.
#[test]
fn request_inside_a_link_excludes_the_link_itself() {
    assert!(!link_path_in_scope(
        "libs/shared",
        Some(&paths(&["libs/shared/sub"]))
    ));
}

#[test]
fn unrelated_requested_path_excludes_the_link() {
    assert!(!link_path_in_scope("libs/shared", Some(&paths(&["docs"]))));
}

/// A sibling sharing a name prefix is not a parent directory.
#[test]
fn sibling_prefix_does_not_put_a_link_in_scope() {
    assert!(!link_path_in_scope(
        "libs/shared",
        Some(&paths(&["libs/sha"]))
    ));
}

#[test]
fn any_matching_requested_path_puts_the_link_in_scope() {
    assert!(link_path_in_scope(
        "libs/shared",
        Some(&paths(&["docs", "libs"]))
    ));
}

/// The changes are sent by reference, so sending them holds no change beside the node reads.
#[test]
fn sending_the_changes_holds_no_change() {
    let send = send_file_changes(Vec::new());

    assert!(
        size_of_val(&send) < size_of::<NodeChange>(),
        "sending the changes holds {} bytes",
        size_of_val(&send)
    );
}

/// The changes are sent after the link pins are diffed, so the diff holds no change beside
/// the link pin diff.
#[tokio::test]
async fn the_link_pin_diff_is_held_without_a_change() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable_store, mutable_store, _execution) =
                test_store_create().await.expect("Failed to create stores");
            let repository = Arc::new(RepositoryContext::new_null_context(
                immutable_store,
                mutable_store,
            ));
            let state = State::new();

            let revision_diff = diff(
                repository.clone(),
                Hash::default(),
                Hash::default(),
                None,
                FilterMode::View,
            );
            let pin_diff = link::diff_link_pins(repository, &state, &state);

            assert!(
                size_of_val(&revision_diff) < size_of_val(&pin_diff) + size_of::<NodeChange>(),
                "the diff holds {} bytes, the link pin diff {}",
                size_of_val(&revision_diff),
                size_of_val(&pin_diff)
            );
        })
        .await;
}
