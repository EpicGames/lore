// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![allow(clippy::disallowed_methods)] // Test fixtures write the working tree directly.

//! The purge of a directory entry whose name folds to the name of a child the revision holds.

use std::sync::Arc;

use lore_base::test_util::TempDir;
use lore_revision::file::reset::HeldChild;
use lore_revision::file::reset::HeldChildren;
use lore_revision::file::reset::ResetStats;
use lore_revision::file::reset::purge_untracked_children;
use lore_revision::fs::filesystem_provider::test_util::TestIdentity;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::State;
use lore_revision::util::path::RelativePath;
use lore_storage::hash::hash_string;
use lore_storage::hash::hash_string_bytes;

use crate::fs::filesystem_provider::TestFilesystemProvider;
use crate::fs::filesystem_provider::setup_test_execution;
use crate::fs::filesystem_provider::test_repository_over;
use crate::fs::filesystem_provider::test_store_create;
use crate::helper::in_memory::file;

/// A node the tree holds none at, whose name cannot be read.
const UNREAD: NodeID = 0x00ff_ffff;

/// The paths a purge of a working tree holding `foo.txt` removes, where the revision holds
/// `Foo.txt` at the node `child` names, or at the node it adds where `child` names none, and the
/// provider tells the two entries apart as `identity` says.
async fn purged(identity: TestIdentity, child: Option<NodeID>) -> Vec<String> {
    let dir = TempDir::new("lore-purge-test-");
    std::fs::write(dir.child("foo.txt"), b"untracked").expect("write the entry");
    let (immutable_store, mutable_store, _) =
        test_store_create().await.expect("Making test stores");
    let filesystem = Arc::new(TestFilesystemProvider::reading(dir.path()).with_identity(identity));
    let repository = test_repository_over(immutable_store, mutable_store, filesystem.clone());
    let state = State::new();
    let added = state
        .node_add(repository.clone(), ROOT_NODE, file("Foo.txt"), "Foo.txt")
        .await
        .expect("the node adds");
    let held = HeldChildren::new(
        state,
        repository.clone(),
        vec![HeldChild {
            folded: hash_string("Foo.txt"),
            exact: hash_string_bytes(b"Foo.txt"),
            node: child.unwrap_or(added),
        }],
    );
    let operation = repository
        .file_system()
        .begin_operation()
        .await
        .expect("an operation");

    purge_untracked_children(
        &operation,
        &Arc::new(ResetStats::default()),
        dir.path(),
        &RelativePath::new(),
        Some(held),
    )
    .await
    .expect("the purge runs");
    filesystem.removed.lock().clone()
}

/// An entry whose name folds to a child's is removed only where it is there and another entry
/// than the child's. One that names the child's entry is held, as on a file system that folds
/// case, and so is one gone at the check, as a directory the child's reset replaces is for a
/// moment, and one that cannot be told apart from the child.
#[tokio::test]
async fn a_folded_match_is_purged_only_where_it_is_another_entry() {
    lore_base::runtime::LORE_CONTEXT
        .scope(setup_test_execution(), async {
            for (identity, removed) in [
                (TestIdentity::Exact, &["foo.txt"][..]),
                (TestIdentity::Folded, &[][..]),
                (TestIdentity::NamesNothing, &[][..]),
                (TestIdentity::Fails, &[][..]),
            ] {
                assert_eq!(
                    purged(identity, None).await,
                    removed,
                    "answered {identity:?}"
                );
            }
        })
        .await;
}

/// An entry whose name folds to that of a child whose name the tree cannot read is held.
#[tokio::test]
async fn a_folded_match_to_a_child_whose_name_cannot_be_read_is_held() {
    lore_base::runtime::LORE_CONTEXT
        .scope(setup_test_execution(), async {
            assert!(purged(TestIdentity::Exact, Some(UNREAD)).await.is_empty());
        })
        .await;
}
