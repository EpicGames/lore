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

    fn node(name: &str, flags: NodeFlags, address: Address) -> Node {
        Node {
            flags: flags.bits(),
            mode: 0o644,
            size: 10,
            address,
            name_hash: hash_string(name),
            ..Default::default()
        }
    }

    fn file_id() -> Context {
        Context::from(uuid::Uuid::now_v7())
    }

    fn address(hash: u64, context: Context) -> Address {
        Address {
            hash: Hash::from_u64(hash),
            context,
        }
    }

    /// The dirty action bits a node carries, which every staged change has to
    /// record alongside the staged action.
    fn dirty_action(node: &Node) -> u16 {
        node.flags & NodeFlags::DirtyBits.bits()
    }

    /// Staging a deletion leaves the node exactly where it was — the commit that
    /// freezes the tree is what removes it — so the name, the parent and the place
    /// in the sibling chain all have to survive.
    #[tokio::test]
    async fn node_delete_stages_the_node_and_leaves_it_in_the_tree() {
        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable_store).await;
                let state = Arc::new(State::new());

                let node_id = state
                    .node_add(
                        repository.clone(),
                        ROOT_NODE,
                        node("data.bin", NodeFlags::File, address(1, file_id())),
                        "data.bin",
                    )
                    .await
                    .expect("adding the file must succeed");

                let staged = state
                    .node_delete(repository.clone(), node_id)
                    .await
                    .expect("staging the deletion must succeed");
                assert!(staged, "the node must report that it took the tag");

                let deleted = state
                    .node(repository.clone(), node_id)
                    .await
                    .expect("a staged deletion must still read back");
                assert!(
                    deleted.is_staged_delete(),
                    "the node must carry the staged deletion"
                );
                assert_eq!(
                    dirty_action(&deleted),
                    NodeFlags::DirtyDelete.bits(),
                    "the dirty deletion must be recorded alongside the staged one"
                );
                assert_eq!(
                    deleted.name_hash,
                    hash_string("data.bin"),
                    "the name must survive, for history weaving"
                );
                assert_eq!(deleted.parent, ROOT_NODE, "the parent must be untouched");
                assert!(
                    !deleted.is_discarded(),
                    "staging must not discard the slot; commit does that"
                );

                let root = state
                    .node(repository.clone(), ROOT_NODE)
                    .await
                    .expect("the root must read back");
                assert_eq!(
                    root.child, node_id,
                    "the node must keep its place in the parent's chain"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Tagging twice writes nothing the second time, which is what lets a subtree
    /// walk cross a node an earlier deletion already reached without counting it
    /// again.
    #[tokio::test]
    async fn node_delete_reports_a_node_it_had_already_staged() {
        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable_store).await;
                let state = Arc::new(State::new());

                let node_id = state
                    .node_add(
                        repository.clone(),
                        ROOT_NODE,
                        node("data.bin", NodeFlags::File, address(1, file_id())),
                        "data.bin",
                    )
                    .await
                    .expect("adding the file must succeed");

                assert!(
                    state
                        .node_delete(repository.clone(), node_id)
                        .await
                        .expect("the first deletion must succeed"),
                    "the first tag must be reported as written"
                );
                assert!(
                    !state
                        .node_delete(repository.clone(), node_id)
                        .await
                        .expect("the second deletion must succeed"),
                    "the second tag must report that nothing was written"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A discarded slot is on the block's free list and carries neither the file
    /// nor the link flag, so it reads back as an ordinary empty directory. Without
    /// its own check it would be tagged as though it were a live node.
    #[tokio::test]
    async fn node_delete_refuses_a_discarded_slot() {
        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable_store).await;
                let state = Arc::new(State::new());

                let node_id = state
                    .node_add(
                        repository.clone(),
                        ROOT_NODE,
                        node("data.bin", NodeFlags::File, address(1, file_id())),
                        "data.bin",
                    )
                    .await
                    .expect("adding the file must succeed");

                let block_index = NodeBlock::index(node_id);
                let block = state
                    .block(repository.clone(), block_index)
                    .await
                    .expect("the block must be readable");
                block
                    .write()
                    .discard_node(block_index, Node::index(node_id));

                state
                    .node_delete(repository.clone(), node_id)
                    .await
                    .expect_err("a discarded slot must not be deletable");
            }))
            .await
            .expect("Test task failed");
    }

    /// The root is the revision itself, and the sentinel names no node, so neither
    /// can be tagged.
    #[tokio::test]
    async fn node_delete_refuses_an_id_that_names_no_deletable_node() {
        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable_store).await;
                let state = Arc::new(State::new());

                state
                    .node_delete(repository.clone(), INVALID_NODE)
                    .await
                    .expect_err("the invalid sentinel must not be deletable");
            }))
            .await
            .expect("Test task failed");
    }

    /// A restored node comes back as a modification, not an addition: it is in the
    /// revision the handle was loaded from, so what is staged is a change to it.
    /// A zero context preserves the identity it already had.
    #[tokio::test]
    async fn node_undelete_returns_a_node_as_a_modification() {
        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable_store).await;
                let state = Arc::new(State::new());

                let original_file_id = file_id();
                let node_id = state
                    .node_add(
                        repository.clone(),
                        ROOT_NODE,
                        node("data.bin", NodeFlags::File, address(1, original_file_id)),
                        "data.bin",
                    )
                    .await
                    .expect("adding the file must succeed");
                state
                    .node_delete(repository.clone(), node_id)
                    .await
                    .expect("staging the deletion must succeed");

                state
                    .node_undelete(
                        repository.clone(),
                        node_id,
                        0o600,
                        4096,
                        address(2, Context::default()),
                    )
                    .await
                    .expect("restoring the node must succeed");

                let restored = state
                    .node(repository.clone(), node_id)
                    .await
                    .expect("the restored node must read back");
                assert!(
                    !restored.is_staged_delete(),
                    "the deletion must be gone from the node"
                );
                assert!(
                    restored.is_staged_modify(),
                    "a restored node must be staged as a modification"
                );
                assert_eq!(
                    dirty_action(&restored),
                    NodeFlags::DirtyModify.bits(),
                    "the dirty modification must be recorded alongside the staged one"
                );
                assert_eq!(restored.mode, 0o600, "mode must take the new value");
                assert_eq!(restored.size, 4096, "size must take the new value");
                assert_eq!(
                    restored.address.hash,
                    Hash::from_u64(2),
                    "content hash must take the new value"
                );
                assert_eq!(
                    restored.address.context, original_file_id,
                    "a zero context must preserve the file id the node already had"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Restoring is only meaningful for a node on its way out. Letting it run on a
    /// live node would rewrite its content while calling the edit a restore.
    #[tokio::test]
    async fn node_undelete_refuses_a_node_that_is_not_staged_for_deletion() {
        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable_store).await;
                let state = Arc::new(State::new());

                let node_id = state
                    .node_add(
                        repository.clone(),
                        ROOT_NODE,
                        node("data.bin", NodeFlags::File, address(1, file_id())),
                        "data.bin",
                    )
                    .await
                    .expect("adding the file must succeed");

                state
                    .node_undelete(
                        repository.clone(),
                        node_id,
                        0o600,
                        4096,
                        address(2, Context::default()),
                    )
                    .await
                    .expect_err("a live node must not be restorable");
            }))
            .await
            .expect("Test task failed");
    }

    /// A directory carries no content of its own, so a restore drops the size and
    /// address it is handed rather than storing values a commit would recompute.
    #[tokio::test]
    async fn node_undelete_drops_the_fields_a_directory_does_not_carry() {
        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable_store).await;
                let state = Arc::new(State::new());

                let node_id = state
                    .node_add(
                        repository.clone(),
                        ROOT_NODE,
                        node("dir", NodeFlags::NoFlags, Address::default()),
                        "dir",
                    )
                    .await
                    .expect("adding the directory must succeed");
                state
                    .node_delete(repository.clone(), node_id)
                    .await
                    .expect("staging the deletion must succeed");

                state
                    .node_undelete(
                        repository.clone(),
                        node_id,
                        0o700,
                        4096,
                        address(2, file_id()),
                    )
                    .await
                    .expect("restoring the directory must succeed");

                let restored = state
                    .node(repository.clone(), node_id)
                    .await
                    .expect("the restored directory must read back");
                assert_eq!(restored.mode, 0o700, "mode must take the new value");
                assert_eq!(restored.size, 0, "a directory must store no size");
                assert_eq!(
                    restored.address,
                    Address::default(),
                    "a directory's address is derived at commit, so none may be stored"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// The staged and the dirty change are recorded as a pair on the node, and
    /// every ancestor is marked so a commit walking down from the root reaches it.
    /// The root node itself is never flagged — the walk stops above it.
    #[tokio::test]
    async fn node_mark_staged_records_the_pair_and_marks_the_ancestors() {
        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable_store).await;
                let state = Arc::new(State::new());

                let directory = state
                    .node_add(
                        repository.clone(),
                        ROOT_NODE,
                        node("dir", NodeFlags::NoFlags, Address::default()),
                        "dir",
                    )
                    .await
                    .expect("adding the directory must succeed");
                let node_id = state
                    .node_add(
                        repository.clone(),
                        directory,
                        node("data.bin", NodeFlags::File, address(1, file_id())),
                        "data.bin",
                    )
                    .await
                    .expect("adding the file must succeed");

                state
                    .node_mark_staged(
                        repository.clone(),
                        node_id,
                        NodeFlags::StagedAdd,
                        NodeFlags::DirtyAdd,
                    )
                    .await
                    .expect("marking must succeed");

                let marked = state
                    .node(repository.clone(), node_id)
                    .await
                    .expect("the marked node must read back");
                assert!(marked.is_staged_add(), "the staged action must be recorded");
                assert_eq!(
                    dirty_action(&marked),
                    NodeFlags::DirtyAdd.bits(),
                    "the dirty action must be recorded with it"
                );

                let parent = state
                    .node(repository.clone(), directory)
                    .await
                    .expect("the parent must read back");
                assert!(
                    parent.is_staged() && parent.is_dirty(),
                    "the ancestor must be marked staged and dirty"
                );
                assert!(
                    !parent.is_staged_add(),
                    "an ancestor carries no action of its own, only that it is staged"
                );
            }))
            .await
            .expect("Test task failed");
    }
}
