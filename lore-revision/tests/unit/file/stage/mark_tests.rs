// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// A fixture builds the working tree directly; what these test is how the stage marks it.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::file::stage::*;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreString;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;

use crate::fs::filesystem_provider::TestFilesystemProvider;
use crate::fs::filesystem_provider::test_store_create;
use crate::helper::RepositoryContextCreationArgsExt;
use crate::helper::default_repository_creation_args;

/// **A marking walk that fails still lets the others' stale entries go**, as a status scan does.
/// The discards are held until every walk has drained, so a failure returned before applying them
/// would leave a reverted add, and the marks it carried up the tree, in a state the walks that did
/// finish had reconciled. The staged tree holds an add the walk below `a` finds reverted, and the
/// walk below `b` fails.
#[tokio::test]
async fn a_failed_marking_walk_still_discards_what_the_other_walks_found_stale() {
    let dir = lore_base::test_util::TempDir::new("lore-stage-mark-test-");
    std::fs::create_dir(dir.path().join("a")).expect("create directory");
    std::fs::create_dir(dir.path().join("b")).expect("create directory");
    let filesystem = Arc::new(TestFilesystemProvider::holding_every_path());
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    let repository = Arc::new(RepositoryContext::new(
        default_repository_creation_args(immutable_store, mutable_store)
            .with_path(dir.path())
            .with_filesystem_provider(filesystem.clone()),
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            let staged = State::new();
            let stale = staged
                .node_add(
                    repository.clone(),
                    ROOT_NODE,
                    Node {
                        name_hash: lore_storage::hash::hash_string("stale.txt"),
                        flags: NodeFlags::DirtyAdd.bits(),
                        ..Default::default()
                    },
                    "stale.txt",
                )
                .await
                .expect("Adding the add to the staged tree");
            assert!(
                staged
                    .find_node_link(repository.clone(), "stale.txt")
                    .await
                    .is_ok(),
                "the fixture holds the add"
            );
            filesystem
                .stale_on_scan
                .lock()
                .push(("a".to_string(), stale));
            filesystem.failing_scans.lock().push("b".to_string());
            let operation = repository
                .file_system()
                .begin_operation()
                .await
                .expect("An operation");

            let marked = mark_through_provider_diff(
                &operation,
                &repository,
                &State::new(),
                &staged,
                &[],
                &[],
                &LoreArray::from_vec(vec![
                    LoreString::from(&dir.path().join("a")),
                    LoreString::from(&dir.path().join("b")),
                ]),
            )
            .await;

            assert!(marked.is_err(), "the failing walk fails the marking");
            assert!(
                staged
                    .find_node_link(repository.clone(), "stale.txt")
                    .await
                    .is_err(),
                "the reverted add the other walk found is discarded all the same"
            );
        })
        .await;
}
