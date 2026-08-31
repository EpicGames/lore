// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_revision::node::*;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::state::State;
    use lore_storage::hash::hash_string;
    use lore_storage::local::immutable_store::LocalImmutableStore;

    include!("helper.rs");

    /// How many times a refused add is repeated before checking that none of
    /// them took a slot.
    const REFUSED_ADDS: usize = 64;

    /// A name the node name table refuses, so the add fails after the allocator
    /// has already handed out a slot.
    const REFUSED_NAME: &str = "..";

    async fn test_repository(
        path: &std::path::Path,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
    ) -> Arc<RepositoryContext> {
        let immutable_store = LocalImmutableStore::new(
            None,
            lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
        )
        .await
        .expect("Failed to create store");
        let write_token = lore_revision::repository::RepositoryWriteToken::acquire(path).await;
        Arc::new(
            RepositoryContext::new(
                default_repository_creation_args(immutable_store, mutable_store).with_path(path),
            )
            .with_write_token(write_token.share()),
        )
    }

    fn directory(name: &str) -> Node {
        Node {
            name_hash: hash_string(name),
            ..Default::default()
        }
    }

    /// Initialization runs after the allocator hands out a slot, so an add that
    /// fails there must put the slot back. A name the node name table refuses is
    /// the one such failure a caller can reach directly.
    #[tokio::test]
    async fn node_add_releases_the_slot_when_the_name_is_refused() {
        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable_store).await;
                let state = Arc::new(State::new());

                let first = state
                    .node_add(repository.clone(), ROOT_NODE, directory("first"), "first")
                    .await
                    .expect("the first add must succeed");

                for round in 0..REFUSED_ADDS {
                    let refused = state
                        .node_add(
                            repository.clone(),
                            ROOT_NODE,
                            directory(REFUSED_NAME),
                            REFUSED_NAME,
                        )
                        .await;
                    assert!(
                        refused.is_err(),
                        "round {round}: the name table must refuse {REFUSED_NAME:?}"
                    );
                }

                let second = state
                    .node_add(repository.clone(), ROOT_NODE, directory("second"), "second")
                    .await
                    .expect("an add after the refused ones must succeed");
                assert_eq!(
                    second,
                    first + 1,
                    "{REFUSED_ADDS} refused adds must consume no node slot"
                );

                let children = state
                    .node_children(repository.clone(), ROOT_NODE)
                    .await
                    .expect("listing the root must succeed");
                assert_eq!(
                    children.len(),
                    2,
                    "only the two accepted adds may reach the child chain"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A discarded slot keeps its name and reads back as a directory, so nothing
    /// about the node itself says it is gone. Attaching a child to one orphans
    /// that child as soon as the allocator hands the slot out again.
    #[tokio::test]
    async fn node_add_refuses_a_discarded_parent() {
        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable_store).await;
                let state = Arc::new(State::new());

                let parent = state
                    .node_add(repository.clone(), ROOT_NODE, directory("parent"), "parent")
                    .await
                    .expect("adding the parent must succeed");
                state
                    .node_add(repository.clone(), parent, directory("before"), "before")
                    .await
                    .expect("the parent must take a child while it is live");

                let block_index = NodeBlock::index(parent);
                let block = state
                    .block(repository.clone(), block_index)
                    .await
                    .expect("the parent block must be readable");
                block.write().discard_node(block_index, Node::index(parent));

                let node = state
                    .node(repository.clone(), parent)
                    .await
                    .expect("a discarded slot still reads back");
                assert!(
                    node.is_directory(),
                    "a discarded slot reads back as a directory, which is why it needs its own check"
                );

                let refused = state
                    .node_add(repository.clone(), parent, directory("after"), "after")
                    .await;
                assert!(
                    refused.is_err(),
                    "a discarded node must not take a child, got {refused:?}"
                );
            }))
            .await
            .expect("Test task failed");
    }
}
