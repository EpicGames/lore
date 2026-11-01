// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_revision::change::FileAction;
use lore_revision::change::Flags;
use lore_revision::change::NodeChange;
use lore_revision::change::NodeChangeState;
use lore_revision::node::NodeFlags;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::NodeMapping;
use lore_revision::state::State;
use lore_revision::state::stream::ChangeStream;
use lore_revision::util::path::RelativePath;

use crate::fs::filesystem_provider::setup_test_execution;
use crate::fs::filesystem_provider::test_store_create;

mod diff;
mod last_merged;
mod merge_moves;
mod resolve;
mod sync;
mod sync_hold;

use lore_revision::revision::*;

fn path(text: &str) -> RelativePath {
    RelativePath::new_from_initial_path(text).expect("valid path")
}

#[test]
fn is_below_accepts_only_paths_inside_the_mount() {
    assert!(is_below(&path("shared"), &path("shared/a.txt")));
    assert!(is_below(&path("shared"), &path("shared/deep/a.txt")));
    assert!(!is_below(&path("shared"), &path("shared")));
    assert!(!is_below(&path("shared"), &path("shared-other/a.txt")));
    assert!(!is_below(&path("shared"), &path("root.txt")));
}

#[test]
fn push_unique_path_holds_one_entry_per_path() {
    let mut paths = Vec::new();
    push_unique_path(&mut paths, &path("shared"));
    push_unique_path(&mut paths, &path("shared"));
    push_unique_path(&mut paths, &path("other"));
    assert_eq!(paths.len(), 2);
}

/// The target's walk holds each change only until the join takes it, so while it joins a change
/// it holds less than another change beside the join and what the join builds.
#[tokio::test]
async fn the_target_walk_holds_no_change_beside_the_join() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable_store, mutable_store, _execution) =
                test_store_create().await.expect("Failed to create stores");
            let repository = Arc::new(RepositoryContext::new_null_context(
                immutable_store,
                mutable_store,
            ));
            let state = State::new();
            let side = NodeChangeState {
                mapping: NodeMapping::root(repository, state.clone()),
                observed: None,
                flags: NodeFlags::NoFlags,
                address: Address::default(),
                mode: 0,
            };
            let change = NodeChange {
                action: FileAction::Keep,
                flags: Flags::None,
                from: side.clone(),
                to: side,
            };
            let (mut target_mounts, mut conflicting_mounts) = (Vec::new(), Vec::new());

            let walk = join_target_walk(
                ChangeStream::<()>::nothing(),
                &[],
                &[],
                &mut target_mounts,
                &mut conflicting_mounts,
                (&state, &state),
                false,
            );
            let mut joined = Joined::new(0);
            let join = joined.join(&[], change, Err(0), false);

            assert!(
                size_of_val(&walk)
                    < size_of_val(&join) + size_of::<Joined>() + size_of::<NodeChange>(),
                "the walk holds {} bytes beside a join of {}",
                size_of_val(&walk),
                size_of_val(&join)
            );
        })
        .await;
}
