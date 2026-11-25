// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! The pass of `revision info --delta` that lists the changes inside the links a revision moved:
//! a cancelled walk fails it rather than reading as a link whose changes cannot be read.

use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision::info::send_moved_link_deltas;
use lore_revision::runtime::Cancellation;
use lore_revision::state::State;
use lore_revision::state::allow_all_repositories;
use lore_revision::util::path::RelativePath;
use lore_storage::hash::hash_string;

use crate::fs::filesystem_provider::setup_test_execution;
use crate::fs::filesystem_provider::test_store_create;

/// The walk from a revision holding nothing to one holding a few files fails as cancelled when
/// its caller is, and lists what it finds when it is not.
#[tokio::test]
async fn a_cancelled_walk_of_the_moved_links_fails_as_cancelled() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable_store, mutable_store, _execution) =
                test_store_create().await.expect("Failed to create stores");
            let repository = Arc::new(RepositoryContext::new_null_context(
                immutable_store,
                mutable_store,
                allow_all_repositories(),
            ));
            let parent = State::new();
            let state = State::new();
            for name in ["a", "b", "c"] {
                let node = Node {
                    flags: NodeFlags::File.bits(),
                    name_hash: hash_string(name),
                    ..Default::default()
                };
                state
                    .node_add(repository.clone(), ROOT_NODE, node, name)
                    .await
                    .expect("Failed to add node");
            }
            let mounts = [RelativePath::new_from_initial_path("a").expect("valid path")];

            let cancellation = Cancellation::new();
            cancellation.cancel();
            let cancelled = Arc::new(
                ExecutionContext::new_client(
                    LoreGlobalArgs::default(),
                    EventDispatcher::no_dispatch(),
                )
                .with_cancellation(cancellation),
            );
            let err = LORE_CONTEXT
                .scope(
                    cancelled,
                    send_moved_link_deltas(
                        &repository,
                        Some(parent.clone()),
                        &state,
                        &mounts,
                        None,
                    ),
                )
                .await
                .expect_err("a cancelled walk fails the pass");
            assert!(err.is_cancelled(), "{err:?}");

            send_moved_link_deltas(&repository, Some(parent), &state, &mounts, None)
                .await
                .expect("the same walk, not cancelled, succeeds");
        })
        .await;
}
