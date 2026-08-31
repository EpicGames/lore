// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_revision::node::*;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::verify::verify_state_for_commit;
    use lore_revision::state::State;
    use lore_storage::hash::hash_string;
    use lore_storage::local::immutable_store::LocalImmutableStore;

    include!("helper.rs");

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

    fn file(name: &str) -> Node {
        Node {
            flags: NodeFlags::File.bits(),
            mode: 0o644,
            size: 10,
            address: Address {
                hash: Hash::from_u64(1),
                context: Context::from(uuid::Uuid::now_v7()),
            },
            name_hash: hash_string(name),
            ..Default::default()
        }
    }

    fn directory(name: &str) -> Node {
        Node {
            flags: NodeFlags::NoFlags.bits(),
            mode: 0o755,
            name_hash: hash_string(name),
            ..Default::default()
        }
    }

    /// Add a node the way a write verb does: `node_add` records nothing on its own,
    /// and the validator only walks what carries a staged action.
    async fn add(
        state: &State,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        node: Node,
        name: &str,
    ) -> NodeID {
        let node_id = state
            .node_add(repository.clone(), parent, node, name)
            .await
            .expect("adding the node must succeed");
        state
            .node_mark_staged(
                repository,
                node_id,
                NodeFlags::StagedAdd,
                NodeFlags::DirtyAdd,
            )
            .await
            .expect("marking the addition must succeed");
        node_id
    }

    /// Clear a node's change flags, as the commit's freeze does — the node is then
    /// indistinguishable from one the loaded revision holds.
    async fn settle(state: &State, repository: Arc<RepositoryContext>, node_id: NodeID) {
        let block_index = NodeBlock::index(node_id);
        let block = state
            .block(repository, block_index)
            .await
            .expect("the block must read back");
        {
            let mut writer = block.write();
            writer.node(Node::index(node_id)).clear_all_change_flags();
            writer.mark_dirty();
        }
        state.block_modified(block, block_index);
    }

    /// The walk has to accept the ordinary case, or nothing commits.
    #[tokio::test]
    async fn verify_state_for_commit_accepts_a_tree_with_distinct_names() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable).await;
                let state = Arc::new(State::new());

                let dir = add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    directory("dir"),
                    "dir",
                )
                .await;
                add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    file("a.bin"),
                    "a.bin",
                )
                .await;
                add(&state, repository.clone(), dir, file("a.bin"), "a.bin").await;
                add(&state, repository.clone(), dir, file("b.bin"), "b.bin").await;

                verify_state_for_commit(repository, state)
                    .await
                    .expect("a tree with distinct names per directory must verify");
            }))
            .await
            .expect("Task failed");
    }

    /// Two concurrent calls adding the same name both validate and both apply, which
    /// is documented and accepted; this is the net that stops the result being
    /// published.
    #[tokio::test]
    async fn verify_state_for_commit_rejects_two_siblings_with_the_same_name() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable).await;
                let state = Arc::new(State::new());

                add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    file("a.bin"),
                    "a.bin",
                )
                .await;
                add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    file("a.bin"),
                    "a.bin",
                )
                .await;

                let error = verify_state_for_commit(repository, state)
                    .await
                    .expect_err("a duplicate child name must not reach a revision");
                assert!(
                    error.to_string().contains("shares a name with node"),
                    "Expected a duplicate-name rejection, got {error}"
                );
            }))
            .await
            .expect("Task failed");
    }

    /// The duplicate has to be found wherever it is, not only under the root.
    #[tokio::test]
    async fn verify_state_for_commit_rejects_a_duplicate_name_inside_a_subdirectory() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable).await;
                let state = Arc::new(State::new());

                let dir = add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    directory("dir"),
                    "dir",
                )
                .await;
                let nested = add(
                    &state,
                    repository.clone(),
                    dir,
                    directory("nested"),
                    "nested",
                )
                .await;
                add(&state, repository.clone(), nested, file("a.bin"), "a.bin").await;
                add(&state, repository.clone(), nested, file("a.bin"), "a.bin").await;

                let error = verify_state_for_commit(repository, state)
                    .await
                    .expect_err("a duplicate below the root must not reach a revision");
                assert!(
                    error.to_string().contains("shares a name with node"),
                    "Expected a duplicate-name rejection, got {error}"
                );
            }))
            .await
            .expect("Task failed");
    }

    /// Deleting a name and adding it back with a different kind leaves both nodes in
    /// the tree until the commit freezes it, so the namesake on its way out must not
    /// be read as a collision.
    #[tokio::test]
    async fn verify_state_for_commit_allows_a_live_namesake_beside_one_staged_for_deletion() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable).await;
                let state = Arc::new(State::new());

                let going = add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    file("a.bin"),
                    "a.bin",
                )
                .await;
                state
                    .node_delete(repository.clone(), going)
                    .await
                    .expect("staging the deletion must succeed");
                add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    directory("a.bin"),
                    "a.bin",
                )
                .await;

                verify_state_for_commit(repository, state)
                    .await
                    .expect("a replacement beside a staged deletion must verify");
            }))
            .await
            .expect("Task failed");
    }

    /// A stored name that does not hash to the `name_hash` beside it is caught on its
    /// own, which the collision check cannot do — that compares hashes to each other
    /// and sees nothing wrong with a single node.
    #[tokio::test]
    async fn verify_state_for_commit_rejects_a_name_hash_that_does_not_match_the_name() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable).await;
                let state = Arc::new(State::new());

                let node_id = add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    file("a.bin"),
                    "a.bin",
                )
                .await;

                let block_index = NodeBlock::index(node_id);
                let block = state
                    .block(repository.clone(), block_index)
                    .await
                    .expect("the block must read back");
                {
                    let mut writer = block.write();
                    writer.node(Node::index(node_id)).name_hash = hash_string("b.bin");
                    writer.mark_dirty();
                }
                state.block_modified(block, block_index);

                let error = verify_state_for_commit(repository, state)
                    .await
                    .expect_err("a name hash that does not match its name must be rejected");
                assert!(
                    error.to_string().contains("invalid name hash"),
                    "Expected a name-hash rejection, got {error}"
                );
            }))
            .await
            .expect("Task failed");
    }

    /// The collision the walk exists to catch: two concurrent calls raced one name,
    /// the loser's node is already in the loaded revision, and the winner's is staged
    /// beside it. Only one of the pair is staged, so comparing staged names to each
    /// other would miss it.
    #[tokio::test]
    async fn verify_state_for_commit_rejects_a_staged_name_a_settled_sibling_holds() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable).await;
                let state = Arc::new(State::new());

                let settled = add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    file("a.bin"),
                    "a.bin",
                )
                .await;
                settle(&state, repository.clone(), settled).await;
                add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    file("a.bin"),
                    "a.bin",
                )
                .await;

                let error = verify_state_for_commit(repository, state)
                    .await
                    .expect_err("a staged name a sibling already holds must not be published");
                assert!(
                    error.to_string().contains("shares a name with node"),
                    "Expected a duplicate-name rejection, got {error}"
                );
            }))
            .await
            .expect("Task failed");
    }

    /// A settled pair of namesakes is left alone even where the walk does read it:
    /// this directory is staged and holds a staged file, so both namesakes are
    /// compared, and only the rule that one side of a pair must be staged keeps
    /// them from being reported.
    #[tokio::test]
    async fn verify_state_for_commit_allows_two_settled_namesakes_in_a_staged_directory() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable).await;
                let state = Arc::new(State::new());

                let dir = add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    directory("dir"),
                    "dir",
                )
                .await;
                let first = add(&state, repository.clone(), dir, file("dup"), "dup").await;
                let second = add(&state, repository.clone(), dir, file("dup"), "dup").await;
                for node_id in [first, second] {
                    settle(&state, repository.clone(), node_id).await;
                }

                add(&state, repository.clone(), dir, file("new.bin"), "new.bin").await;

                verify_state_for_commit(repository, state)
                    .await
                    .expect("a settled pair of namesakes must not be reported");
            }))
            .await
            .expect("Task failed");
    }

    /// The walk descends only into staged directories, so a settled subtree costs
    /// nothing to commit past — and a duplicate inside one is not reported, because
    /// it came from a revision that was validated when it was published. Something
    /// outside the subtree is staged, or the walk would have no work and the test
    /// would pass without exercising the descent rule at all.
    #[tokio::test]
    async fn verify_state_for_commit_does_not_read_a_settled_subtree() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable).await;
                let state = Arc::new(State::new());

                let settled_dir = add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    directory("settled"),
                    "settled",
                )
                .await;
                let first = add(&state, repository.clone(), settled_dir, file("dup"), "dup").await;
                let second = add(&state, repository.clone(), settled_dir, file("dup"), "dup").await;
                for node_id in [settled_dir, first, second] {
                    settle(&state, repository.clone(), node_id).await;
                }

                add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    file("staged.bin"),
                    "staged.bin",
                )
                .await;

                verify_state_for_commit(repository, state)
                    .await
                    .expect("a settled subtree must not be walked, duplicate or not");
            }))
            .await
            .expect("Task failed");
    }
}
