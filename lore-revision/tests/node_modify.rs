// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::sync::Arc;

    use lore_base::error::NoRemote;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_revision::node::*;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryFormat;
    use lore_revision::state::State;
    use lore_storage::hash::hash_string;
    use lore_storage::local::immutable_store::LocalImmutableStore;
    use lore_transport::ProtocolError;

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
                Some(path.to_path_buf()),
                immutable_store,
                mutable_store,
                Context::from(uuid::Uuid::now_v7()).into(),
                lore_revision::instance::InstanceId::default(),
                Err(ProtocolError::from(NoRemote)),
                Arc::default(),
                RepositoryFormat::Lore,
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

    /// The common edit: new content at the same path. Mode, size and content
    /// hash take the new values while the file id stays put, because a caller
    /// replacing bytes supplies no context.
    #[tokio::test]
    async fn node_modify_rewrites_content_and_keeps_the_file_id() {
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
                    .node_modify(
                        repository.clone(),
                        node_id,
                        0o600,
                        4096,
                        address(2, Context::default()),
                    )
                    .await
                    .expect("modifying a file must succeed");

                let modified = state
                    .node(repository.clone(), node_id)
                    .await
                    .expect("the modified node must read back");
                assert_eq!(modified.mode, 0o600, "mode must take the new value");
                assert_eq!(modified.size, 4096, "size must take the new value");
                assert_eq!(
                    modified.address.hash,
                    Hash::from_u64(2),
                    "content hash must take the new value"
                );
                assert_eq!(
                    modified.address.context, original_file_id,
                    "a zero context must preserve the file id rather than clear it"
                );
                assert_eq!(
                    modified.name_hash,
                    hash_string("data.bin"),
                    "modify must not disturb the node's name"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A caller that does supply a file id is recording a different identity for
    /// the path, and gets it.
    #[tokio::test]
    async fn node_modify_takes_a_supplied_file_id() {
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

                let replacement = file_id();
                state
                    .node_modify(
                        repository.clone(),
                        node_id,
                        0o644,
                        10,
                        address(2, replacement),
                    )
                    .await
                    .expect("modifying a file must succeed");

                let modified = state
                    .node(repository.clone(), node_id)
                    .await
                    .expect("the modified node must read back");
                assert_eq!(
                    modified.address.context, replacement,
                    "a supplied file id must replace the existing one"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A directory's size and address are derived at commit and a link's address
    /// is its target, so neither holds content a caller can rewrite.
    #[tokio::test]
    async fn node_modify_refuses_a_directory_and_a_link() {
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
                let link = state
                    .node_add(
                        repository.clone(),
                        ROOT_NODE,
                        node("link", NodeFlags::Link, address(3, file_id())),
                        "link",
                    )
                    .await
                    .expect("adding the link must succeed");

                for (node_id, kind) in [(directory, "directory"), (link, "link")] {
                    let before = state
                        .node(repository.clone(), node_id)
                        .await
                        .expect("the node must read back before the attempt");
                    let refused = state
                        .node_modify(
                            repository.clone(),
                            node_id,
                            0o600,
                            4096,
                            address(9, Context::default()),
                        )
                        .await;
                    assert!(
                        refused.is_err(),
                        "a {kind} must not be modifiable, got {refused:?}"
                    );
                    let after = state
                        .node(repository.clone(), node_id)
                        .await
                        .expect("the node must read back after the attempt");
                    assert_eq!(
                        (after.mode, after.size, after.address),
                        (before.mode, before.size, before.address),
                        "a refused modify must leave the {kind} untouched"
                    );
                }
            }))
            .await
            .expect("Test task failed");
    }

    /// A discarded slot carries neither the file nor the link flag, so it reads
    /// back as an ordinary directory. Writing content into one would be lost the
    /// moment the allocator hands the slot out again.
    #[tokio::test]
    async fn node_modify_refuses_a_deleted_node() {
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

                let refused = state
                    .node_modify(
                        repository.clone(),
                        node_id,
                        0o600,
                        4096,
                        address(2, Context::default()),
                    )
                    .await;
                let reason = refused
                    .expect_err("a deleted node must not be modifiable")
                    .to_string();
                // Discarding clears the file flag, so the kind check would refuse
                // this too — but as "not a file", which would send a caller
                // looking for a type error instead of a deleted node.
                assert!(
                    reason.contains("deleted"),
                    "a deleted node must be refused as deleted, got {reason:?}"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// The root is the tree itself, not a leaf, and the invalid sentinel names
    /// no node at all. Both are refused before any block is read.
    #[tokio::test]
    async fn node_modify_refuses_the_root_and_the_invalid_sentinel() {
        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let tempdir = generate_tempdir();
                let repository = test_repository(tempdir.path(), mutable_store).await;
                let state = Arc::new(State::new());

                for (node_id, label) in [(ROOT_NODE, "root"), (INVALID_NODE, "invalid sentinel")] {
                    let refused = state
                        .node_modify(
                            repository.clone(),
                            node_id,
                            0o600,
                            4096,
                            address(2, Context::default()),
                        )
                        .await;
                    let Err(error) = refused else {
                        panic!("the {label} must not be modifiable");
                    };
                    let reason = error.to_string();
                    // Both are refused before a block is read: the root would
                    // otherwise fall through to the kind check, and the sentinel
                    // to a block index that names nothing.
                    assert!(
                        reason.contains("does not name a modifiable node"),
                        "the {label} must be refused as unmodifiable, got {reason:?}"
                    );
                }
            }))
            .await
            .expect("Test task failed");
    }
}
