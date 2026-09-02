// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Address;
    use lore_base::types::CloneHeapAlloc;
    use lore_base::types::ZeroHeapAlloc;
    use lore_revision::node::*;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::state::State;
    use lore_revision::state::StateData;
    use lore_revision::state::collect_new_fragments;
    use lore_storage::hash::hash_string;
    use lore_storage::local::immutable_store::LocalImmutableStore;
    use zerocopy::IntoBytes;

    include!("helper.rs");

    #[test]
    fn size() {
        assert_eq!(
            std::mem::size_of::<StateData>(),
            320,
            "State data size is invalid"
        );
    }

    #[test]
    fn clone_block_data() {
        // Create a node block data with some random data
        let mut block = NodeBlockData::new_from_heap_zeroed();
        block.flags = NodeBlockFlags::Dirty.as_u32();
        block.node[100].flags = NodeFlags::Discarded.as_u32() as u16;
        block.node_count = 150;

        // Clone it
        let cloned = block.clone_on_heap();
        assert_ne!(cloned.as_bytes().as_ptr(), block.as_bytes().as_ptr());
        assert_eq!(cloned.as_bytes(), block.as_bytes());
    }

    /// A state can be serialized more than once, and the second write carries the edits
    /// made between the two.
    ///
    /// Every link in that chain is asserted: `serialize` releases the blocks it wrote, so
    /// an edit after it finds them clean, registers itself and marks the state dirty
    /// again; the second `serialize` then has something to write and returns a signature
    /// of its own; and the edit is read back out of that signature, because a serialize
    /// that wrote nothing returns the first signature and is otherwise indistinguishable
    /// from one that worked.
    #[tokio::test]
    async fn serialize_twice_writes_the_edits_made_between() {
        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let tempdir = generate_tempdir();
                let path = tempdir.to_path_buf();
                let immutable_store = LocalImmutableStore::new(
                    None,
                    lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");
                let write_token =
                    lore_revision::repository::RepositoryWriteToken::acquire(path.as_path()).await;
                let repository = Arc::new(
                    RepositoryContext::new(
                        default_repository_creation_args(
                            immutable_store.clone(),
                            mutable_store.clone(),
                        )
                        .with_path(&path),
                    )
                    .with_write_token(write_token.share()),
                );

                let state = Arc::new(State::new());
                state
                    .node_add(
                        repository.clone(),
                        ROOT_NODE,
                        Node {
                            name_hash: hash_string("first"),
                            ..Default::default()
                        },
                        "first",
                    )
                    .await
                    .expect("Failed to add the first node");
                let first = state
                    .serialize(repository.clone(), &write_token)
                    .await
                    .expect("Failed to serialize");

                let second_node = state
                    .node_add(
                        repository.clone(),
                        ROOT_NODE,
                        Node {
                            name_hash: hash_string("second"),
                            ..Default::default()
                        },
                        "second",
                    )
                    .await
                    .expect("Failed to add the second node");
                assert!(
                    state.is_dirty(),
                    "an edit after a serialize must mark the state dirty again"
                );

                let second = state
                    .serialize(repository.clone(), &write_token)
                    .await
                    .expect("Failed to serialize again");
                assert_ne!(
                    first, second,
                    "a state with a new edit must serialize to a new signature"
                );

                let restored = State::deserialize(repository.clone(), second)
                    .await
                    .expect("Failed to deserialize");
                let node = restored
                    .node(repository, second_node)
                    .await
                    .expect("Failed to read the node back");
                assert_eq!(
                    node.name_hash,
                    hash_string("second"),
                    "the second serialize must carry the edit made after the first"
                );
            }))
            .await
            .expect("Task failed");
    }

    /// The block that goes into the store has to be the block that is in
    /// memory, byte for byte: `State::serialize` writes it straight from its
    /// lock, so anything the runtime keeps in the block - the dirty flag above
    /// all - would otherwise be written out with it.
    #[tokio::test]
    async fn a_serialized_block_is_the_block_in_memory() {
        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let tempdir = generate_tempdir();
                let path = tempdir.to_path_buf();
                let immutable_store = LocalImmutableStore::new(
                    None,
                    lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");
                let write_token =
                    lore_revision::repository::RepositoryWriteToken::acquire(path.as_path()).await;
                let repository = Arc::new(
                    RepositoryContext::new(
                        default_repository_creation_args(
                            immutable_store.clone(),
                            mutable_store.clone(),
                        )
                        .with_path(&path),
                    )
                    .with_write_token(write_token.share()),
                );

                let state = Arc::new(State::new());
                state
                    .node_add(
                        repository.clone(),
                        ROOT_NODE,
                        Node {
                            name_hash: hash_string("only"),
                            ..Default::default()
                        },
                        "only",
                    )
                    .await
                    .expect("Failed to add the node");
                let signature = state
                    .serialize(repository.clone(), &write_token)
                    .await
                    .expect("Failed to serialize");

                let live = state
                    .block(repository.clone(), 0)
                    .await
                    .expect("Failed to read the block back in memory");
                assert!(
                    !live.read().is_dirty(),
                    "serialize must release the blocks it wrote"
                );
                assert_eq!(
                    live.read().node_block().flags & NODE_BLOCK_RUNTIME_FLAGS,
                    0,
                    "the runtime flags must be held outside the block data"
                );

                let restored = State::deserialize(repository.clone(), signature)
                    .await
                    .expect("Failed to deserialize");
                let stored = restored
                    .block(repository.clone(), 0)
                    .await
                    .expect("Failed to read the block back from the store");

                assert_eq!(
                    stored.read().node_block().flags & NODE_BLOCK_RUNTIME_FLAGS,
                    0,
                    "the runtime flags must not reach the store"
                );
                assert_eq!(
                    live.read().node_block().as_bytes(),
                    stored.read().node_block().as_bytes(),
                    "the stored block must be the in-memory block byte for byte"
                );
            }))
            .await
            .expect("Task failed");
    }

    #[tokio::test]
    async fn collect_new_name_fragments() {
        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let tempdir = generate_tempdir();
                let path = tempdir.to_path_buf();

                // Create an explicit immutable store to access some test functions
                let immutable_store = LocalImmutableStore::new(
                    None,
                    lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");

                let write_token =
                    lore_revision::repository::RepositoryWriteToken::acquire(path.as_path()).await;
                let repository = Arc::new(
                    RepositoryContext::new(
                        default_repository_creation_args(
                            immutable_store.clone(),
                            mutable_store.clone(),
                        )
                        .with_path(&path),
                    )
                    .with_write_token(write_token.share()),
                );

                let state_from = Arc::new(State::new());

                let name = "test-node";
                let node = Node {
                    name_hash: hash_string(name),
                    ..Default::default()
                };
                state_from
                    .node_add(repository.clone(), ROOT_NODE, node, "test-node")
                    .await
                    .expect("Failed to add node");
                let signature_from = state_from
                    .serialize(repository.clone(), &write_token)
                    .await
                    .expect("Failed to serialize from state");

                let state_to = State::deserialize(repository.clone(), signature_from)
                    .await
                    .expect("Failed to deserialize state");

                let _signature_to = state_to
                    .serialize(repository.clone(), &write_token)
                    .await
                    .expect("Failed to serialize to state");

                let fragments = collect_new_fragments(
                    repository.clone(),
                    state_from.clone(),
                    state_to.clone(),
                    true,
                )
                .await
                .expect("Failed to collect fragments");

                assert!(
                    fragments.is_empty(),
                    "Unmodified state does not yield empty collection of new fragments"
                );

                let other_name = "other-test-node";
                let other_node = Node {
                    name_hash: hash_string(other_name),
                    ..Default::default()
                };
                state_to
                    .node_add(repository.clone(), ROOT_NODE, other_node, other_name)
                    .await
                    .expect("Failed to add node");

                let signature_to = state_to
                    .serialize(repository.clone(), &write_token)
                    .await
                    .expect("Failed to serialize to state");

                let state_to = State::deserialize(repository.clone(), signature_to)
                    .await
                    .expect("Failed to deserialize state");

                let fragments = collect_new_fragments(
                    repository.clone(),
                    state_from.clone(),
                    state_to.clone(),
                    true,
                )
                .await
                .expect("Failed to collect fragments");

                assert!(
                    !fragments.is_empty(),
                    "Modified state yielded empty collection of new fragments"
                );

                let name_fragment = state_to
                    .block(repository.clone(), 0)
                    .await
                    .expect("Failed to access node block")
                    .read()
                    .raw()
                    .name_table;

                // Hack to mark all data as durably stored
                immutable_store.mark_all_as_durably_stored().await;

                let fragments = collect_new_fragments(
                    repository.clone(),
                    state_from.clone(),
                    state_to.clone(),
                    true,
                )
                .await
                .expect("Failed to collect fragments");

                assert!(
                    fragments.is_empty(),
                    "New fragments not empty after all data marked as durably stored"
                );

                // Hack to mark all data as durably stored
                let name_address = Address::zero_context_hash(name_fragment);
                immutable_store
                    .mark_as_not_durably_stored(repository.id, name_address)
                    .await;

                let fragments = collect_new_fragments(
                    repository.clone(),
                    state_from.clone(),
                    state_to.clone(),
                    true,
                )
                .await
                .expect("Failed to collect fragments");

                assert!(
                    fragments.contains(&name_address),
                    "Name table not collected as new fragment after marked as not durably stored"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Helper to create a Node with specific flags for testing
    fn node_with_flags(flags: u16) -> Node {
        Node {
            flags,
            ..Default::default()
        }
    }

    use lore_revision::change;
    use lore_revision::change::FileAction;
    use lore_revision::state::compute_change_flags;

    #[test]
    fn returns_none_for_default_node_with_valid_to() {
        let node = Node::default();
        let flags = compute_change_flags(&node, FileAction::Keep, true);
        assert_eq!(flags, change::Flags::None);
    }

    #[test]
    fn sets_modify_flag_for_keep_action_with_invalid_to_node() {
        let node = Node::default();
        let flags = compute_change_flags(&node, FileAction::Keep, false);
        assert!(flags.contains(change::Flags::Modify));
    }

    #[test]
    fn does_not_set_modify_flag_for_add_action_with_invalid_to_node() {
        let node = Node::default();
        let flags = compute_change_flags(&node, FileAction::Add, false);
        assert!(!flags.contains(change::Flags::Modify));
    }

    #[test]
    fn does_not_set_modify_flag_for_delete_action_with_invalid_to_node() {
        let node = Node::default();
        let flags = compute_change_flags(&node, FileAction::Delete, false);
        assert!(!flags.contains(change::Flags::Modify));
    }

    #[test]
    fn sets_staged_flag_when_node_is_staged() {
        let node = node_with_flags(NodeFlags::Staged.bits());
        let flags = compute_change_flags(&node, FileAction::Keep, true);
        assert!(flags.contains(change::Flags::Staged));
    }

    #[test]
    fn sets_merge_flag_when_node_is_staged_merge() {
        let node = node_with_flags(NodeFlags::StagedMerge.bits());
        let flags = compute_change_flags(&node, FileAction::Keep, true);
        assert!(flags.contains(change::Flags::Merge));
    }

    #[test]
    fn sets_conflict_flag_when_node_is_merge_conflict() {
        let node = node_with_flags(NodeFlags::StagedMergeConflict.bits());
        let flags = compute_change_flags(&node, FileAction::Keep, true);
        assert!(flags.contains(change::Flags::Conflict));
    }

    #[test]
    fn sets_conflict_resolved_flag_when_node_is_merge_resolved() {
        let node = node_with_flags(NodeFlags::StagedMergeResolved.bits());
        let flags = compute_change_flags(&node, FileAction::Keep, true);
        assert!(flags.contains(change::Flags::ConflictResolved));
    }

    #[test]
    fn sets_conflict_mine_flag_when_node_is_merge_mine() {
        let node = node_with_flags(NodeFlags::StagedMergeMine.bits());
        let flags = compute_change_flags(&node, FileAction::Keep, true);
        assert!(flags.contains(change::Flags::ConflictMine));
    }

    #[test]
    fn sets_conflict_theirs_flag_when_node_is_merge_theirs() {
        let node = node_with_flags(NodeFlags::StagedMergeTheirs.bits());
        let flags = compute_change_flags(&node, FileAction::Keep, true);
        assert!(flags.contains(change::Flags::ConflictTheirs));
    }

    #[test]
    fn combines_multiple_flags() {
        // Node that is staged and also a merge conflict
        let node = node_with_flags(NodeFlags::StagedMergeConflict.bits());
        let flags = compute_change_flags(&node, FileAction::Keep, false);
        // Should have Modify (from invalid to), Staged, Merge, and Conflict
        assert!(flags.contains(change::Flags::Modify));
        assert!(flags.contains(change::Flags::Staged));
        assert!(flags.contains(change::Flags::Merge));
        assert!(flags.contains(change::Flags::Conflict));
    }

    #[tokio::test]
    async fn deserialize_nonexistent_hash_returns_not_found() {
        use lore_base::types::Hash;

        let (_, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let tempdir = generate_tempdir();
                let path = tempdir.to_path_buf();

                let immutable_store = LocalImmutableStore::new(
                    None,
                    lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create immutable store");

                let repository = Arc::new(RepositoryContext::new(
                    default_repository_creation_args(immutable_store, mutable_store)
                        .with_path(&path),
                ));

                // A non-zero hash that was never written to the store.
                let fake_hash = Hash::from([1u8; 32]);
                let result = State::deserialize(repository, fake_hash).await;

                assert!(result.is_err());
                assert!(
                    result.unwrap_err().is_not_found(),
                    "expected NotFound for a hash that does not exist in the store"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Run `body` against a fresh in-memory state, inside the execution scope the
    /// state edits read their globals from. Nothing here serializes, so the
    /// repository needs no path and no write token.
    async fn with_state<F, Fut>(body: F)
    where
        F: FnOnce(Arc<RepositoryContext>, Arc<State>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let (_immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let immutable_store = LocalImmutableStore::new(
                    None,
                    lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
                )
                .await
                .expect("Failed to create store");
                let repository = Arc::new(RepositoryContext::new(
                    default_repository_creation_args(immutable_store, mutable_store),
                ));
                body(repository, Arc::new(State::new())).await;
            }))
            .await
            .expect("Test task failed");
    }

    /// Add a node of `flags` under `parent`, carrying a file id of its own so a move
    /// can be shown to keep it.
    async fn add_node(
        repository: &Arc<RepositoryContext>,
        state: &Arc<State>,
        parent: NodeID,
        name: &str,
        flags: NodeFlags,
    ) -> NodeID {
        state
            .node_add(
                repository.clone(),
                parent,
                Node {
                    flags: flags.bits(),
                    name_hash: hash_string(name),
                    address: Address {
                        hash: lore_base::types::Hash::from_u64(7),
                        context: lore_base::types::Context::from(uuid::Uuid::now_v7()),
                    },
                    ..Default::default()
                },
                name,
            )
            .await
            .unwrap_or_else(|error| panic!("Failed to add node {name}: {error}"))
    }

    /// The hash comes back from the record the walk read for the child, so it has
    /// to be the hash of that child's name, for the same children in the same
    /// order as the walk that reports ids alone.
    #[tokio::test]
    async fn children_with_name_hash_report_the_hash_of_every_child_name() {
        with_state(|repository, state| async move {
            let names = ["first", "second", "third", "fourth"];
            for name in names {
                add_node(&repository, &state, ROOT_NODE, name, NodeFlags::File).await;
            }

            let children = state
                .node_children(repository.clone(), ROOT_NODE)
                .await
                .expect("Failed to walk the children");
            let with_name_hash = state
                .node_children_with_name_hash(repository.clone(), ROOT_NODE)
                .await
                .expect("Failed to walk the children with their name hash");

            assert_eq!(
                with_name_hash
                    .iter()
                    .map(|&(node, _)| node)
                    .collect::<Vec<_>>(),
                children,
                "the children and their order must be what the walk without the hash reports"
            );

            let mut reported: Vec<u64> = with_name_hash.iter().map(|&(_, hash)| hash).collect();
            reported.sort_unstable();
            let mut expected: Vec<u64> = names.iter().map(|name| hash_string(name)).collect();
            expected.sort_unstable();
            assert_eq!(
                reported, expected,
                "every child must report the hash of the name it was added under"
            );
        })
        .await;
    }

    /// A child is prepended, so the search for what was linked in since a head
    /// was seen covers everything ahead of that head and stops there: it finds
    /// the new children and never the ones the caller already holds.
    #[tokio::test]
    async fn a_search_since_a_head_covers_what_was_linked_in_ahead_of_it() {
        with_state(|repository, state| async move {
            let held = add_node(&repository, &state, ROOT_NODE, "held", NodeFlags::File).await;
            let added = add_node(&repository, &state, ROOT_NODE, "added", NodeFlags::File).await;

            let since_held = |name: &str| {
                let repository = repository.clone();
                let state = state.clone();
                let name_hash = hash_string(name);
                async move {
                    state
                        .find_subnode_added_since(repository, ROOT_NODE, Some(held), name_hash)
                        .await
                        .expect("Failed to search the children added since")
                }
            };

            assert_eq!(
                since_held("added").await,
                Some(added),
                "a child linked in after the head was seen is ahead of it"
            );
            assert_eq!(
                since_held("held").await,
                None,
                "the head itself is where the search stops, so what it holds is not reported"
            );
            assert_eq!(
                since_held("never").await,
                None,
                "a name no child carries is not reported"
            );
            assert_eq!(
                state
                    .find_subnode_added_since(
                        repository.clone(),
                        ROOT_NODE,
                        None,
                        hash_string("held")
                    )
                    .await
                    .expect("Failed to search the children added since"),
                Some(held),
                "a caller holding no children has the whole chain searched"
            );
        })
        .await;
    }

    /// Children linked in since a head was seen that do not all fit in the block
    /// that head sits in: the search crosses into the block it started from and
    /// only then reaches the head it stops at.
    #[tokio::test]
    async fn a_search_since_a_head_crosses_blocks_before_it_reaches_the_head() {
        with_state(|repository, state| async move {
            let mut head = ROOT_NODE;
            for index in 0..BLOCK_NODE_COUNT - 64 {
                head = add_node(
                    &repository,
                    &state,
                    ROOT_NODE,
                    &format!("held-{index:05}"),
                    NodeFlags::File,
                )
                .await;
            }
            let mut added = Vec::new();
            for index in 0..128 {
                added.push(
                    add_node(
                        &repository,
                        &state,
                        ROOT_NODE,
                        &format!("added-{index:05}"),
                        NodeFlags::File,
                    )
                    .await,
                );
            }
            assert!(
                state.block_count() > 1,
                "the children added since must reach past the block the head sits in"
            );

            let since_head = |name: String| {
                let repository = repository.clone();
                let state = state.clone();
                async move {
                    state
                        .find_subnode_added_since(
                            repository,
                            ROOT_NODE,
                            Some(head),
                            hash_string(&name),
                        )
                        .await
                        .expect("Failed to search the children added since")
                }
            };

            assert_eq!(
                since_head("added-00127".to_string()).await,
                added.last().copied(),
                "the child linked in last heads the chain"
            );
            assert_eq!(
                since_head("added-00000".to_string()).await,
                added.first().copied(),
                "a child linked in before the block boundary is still reached"
            );
            assert_eq!(
                since_head(format!("held-{:05}", BLOCK_NODE_COUNT - 65)).await,
                None,
                "the head is where the search stops"
            );
            assert_eq!(
                since_head("held-00000".to_string()).await,
                None,
                "nothing behind the head is reached"
            );
        })
        .await;
    }

    /// A directory holding more children than one node block takes: the walk
    /// reads a run of siblings under one lock on the block they share and takes
    /// the next block where the run ends, which a directory inside a single
    /// block never reaches.
    #[tokio::test]
    async fn children_spanning_node_blocks_are_all_reported_with_their_hash() {
        with_state(|repository, state| async move {
            let names: Vec<String> = (0..BLOCK_NODE_COUNT + 64)
                .map(|index| format!("child-{index}"))
                .collect();
            let mut added: Vec<(NodeID, u64)> = Vec::with_capacity(names.len());
            for name in &names {
                let node = add_node(&repository, &state, ROOT_NODE, name, NodeFlags::File).await;
                added.push((node, hash_string(name)));
            }
            assert!(
                state.block_count() > 1,
                "the children must span more than one node block for this to reach the walk \
                 that takes the next one"
            );

            let mut reported = state
                .node_children_with_name_hash(repository.clone(), ROOT_NODE)
                .await
                .expect("Failed to walk the children with their name hash");

            assert_eq!(
                reported.len(),
                added.len(),
                "the walk must report every child, including those past the first block"
            );
            reported.sort_unstable();
            added.sort_unstable();
            assert_eq!(
                reported, added,
                "every child must carry the hash of its own name, whichever block holds it"
            );
        })
        .await;
    }

    /// Reparenting rewrites the chain the node sat in, and the node it is unlinked from
    /// is whichever one points at it: this is the case where that is a sibling rather
    /// than the parent, which the parent's own `child` pointer never exercises.
    #[tokio::test]
    async fn move_node_unlinks_from_the_middle_of_a_sibling_chain() {
        with_state(|repository, state| async move {
            let source = add_node(&repository, &state, ROOT_NODE, "src", NodeFlags::NoFlags).await;
            let destination =
                add_node(&repository, &state, ROOT_NODE, "dst", NodeFlags::NoFlags).await;
            let first = add_node(&repository, &state, source, "first", NodeFlags::File).await;
            let middle = add_node(&repository, &state, source, "middle", NodeFlags::File).await;
            let last = add_node(&repository, &state, source, "last", NodeFlags::File).await;

            state
                .move_node(repository.clone(), middle, destination, "middle")
                .await
                .expect("Failed to move the middle child");

            assert_eq!(
                state
                    .node_children(repository.clone(), source)
                    .await
                    .expect("Failed to walk the source chain"),
                vec![last, first],
                "the chain the node left must close over it, keeping its order"
            );
            assert_eq!(
                state
                    .node_children(repository.clone(), destination)
                    .await
                    .expect("Failed to walk the destination chain"),
                vec![middle],
                "the node must be the destination's only child"
            );
            assert_eq!(
                state
                    .node(repository.clone(), middle)
                    .await
                    .expect("Failed to read the moved node")
                    .parent,
                destination,
                "the moved node must point at the parent it arrived at"
            );
        })
        .await;
    }

    /// A move keeps the node: the same slot, the same file id, the same children —
    /// which is what makes the record a move rather than a delete and an add. The
    /// subtree is recorded as moved with it, since its paths change.
    #[tokio::test]
    async fn move_node_renames_in_place_and_keeps_identity() {
        with_state(|repository, state| async move {
            let directory =
                add_node(&repository, &state, ROOT_NODE, "before", NodeFlags::NoFlags).await;
            let child =
                add_node(&repository, &state, directory, "child.bin", NodeFlags::File).await;
            let before = state
                .node(repository.clone(), directory)
                .await
                .expect("Failed to read the directory");
            state
                .node_mark_staged(
                    repository.clone(),
                    directory,
                    NodeFlags::NoFlags,
                    NodeFlags::NoFlags,
                )
                .await
                .expect("Failed to settle the directory");
            state
                .node_mark_staged(
                    repository.clone(),
                    child,
                    NodeFlags::NoFlags,
                    NodeFlags::NoFlags,
                )
                .await
                .expect("Failed to settle the child");

            state
                .move_node(repository.clone(), directory, ROOT_NODE, "after")
                .await
                .expect("Failed to rename the directory");

            let after = state
                .node(repository.clone(), directory)
                .await
                .expect("Failed to read the directory back");
            assert_eq!(
                state
                    .node_name_clone(repository.clone(), directory)
                    .await
                    .expect("Failed to read the name"),
                "after",
                "the stored name must be the new one"
            );
            assert_eq!(
                after.name_hash,
                hash_string("after"),
                "the name hash must follow the stored name, which the commit verifier checks"
            );
            assert_eq!(
                after.address.context, before.address.context,
                "the file id must survive the rename"
            );
            assert!(
                after.is_staged_move(),
                "the node must carry the move for the commit to record"
            );
            assert_eq!(
                state
                    .node_children(repository.clone(), directory)
                    .await
                    .expect("Failed to walk the children"),
                vec![child],
                "the children come along without being relinked"
            );
            assert!(
                state
                    .node(repository.clone(), child)
                    .await
                    .expect("Failed to read the child")
                    .is_staged_move(),
                "a child's path changed with its parent's, so it is recorded as moved too"
            );
        })
        .await;
    }

    /// A node whose parent pointer names a directory it is not a child of cannot be
    /// unlinked, and the move fails there — after every check has passed. Nothing may be
    /// half-written at that point: the node must be where it was, under the name it had,
    /// and the chain it really sits in must still be whole.
    #[tokio::test]
    async fn move_node_that_cannot_unlink_writes_nothing() {
        with_state(|repository, state| async move {
            let source = add_node(&repository, &state, ROOT_NODE, "src", NodeFlags::NoFlags).await;
            let destination =
                add_node(&repository, &state, ROOT_NODE, "dst", NodeFlags::NoFlags).await;
            let first = add_node(&repository, &state, source, "first.bin", NodeFlags::File).await;
            let second = add_node(&repository, &state, source, "second.bin", NodeFlags::File).await;

            // Point the node at a parent whose chain does not hold it, which is the shape
            // a broken hierarchy takes and the one the unlink refuses to guess at.
            let block = state
                .block(repository.clone(), NodeBlock::index(second))
                .await
                .expect("Failed to read the block");
            block.write().node(Node::index(second)).parent = destination;

            let failure = state
                .move_node(repository.clone(), second, ROOT_NODE, "second.bin")
                .await
                .expect_err("a node missing from its parent's chain cannot be moved");
            assert!(
                failure.to_string().contains("not in the child chain"),
                "the failure must name what it could not do, got {failure}"
            );

            // Read the links rather than walking them: the fixture's own corruption is
            // what a chain walk refuses, and it is the links this has to pin.
            let source_node = state
                .node(repository.clone(), source)
                .await
                .expect("Failed to read the source");
            let moved_node = state
                .node(repository.clone(), second)
                .await
                .expect("Failed to read the node");
            let destination_node = state
                .node(repository.clone(), destination)
                .await
                .expect("Failed to read the destination");
            assert_eq!(
                source_node.child, second,
                "the chain the node really sits in must still start at it"
            );
            assert_eq!(moved_node.sibling, first, "and must still run through it");
            assert!(
                destination_node.child().is_none(),
                "and the node must not have been linked in anywhere else"
            );
            assert_eq!(
                state
                    .node_name_clone(repository.clone(), second)
                    .await
                    .expect("Failed to read the name"),
                "second.bin",
                "nor renamed on the way to a failure"
            );
        })
        .await;
    }

    /// The reason a refused move gave. Several distinct rules all report invalid
    /// arguments, so a test that only asserts "some error" cannot tell which one fired —
    /// or notice when a later change makes a different one fire first.
    async fn move_failure(
        repository: &Arc<RepositoryContext>,
        state: &Arc<State>,
        node_id: NodeID,
        destination_parent_id: NodeID,
        dst_name: &str,
    ) -> String {
        state
            .move_node(repository.clone(), node_id, destination_parent_id, dst_name)
            .await
            .expect_err("the move must be refused")
            .to_string()
    }

    /// The checks that are about a node's *staging state* rather than the tree's shape. The
    /// verb refuses all three before the primitive sees them, so these are what any other
    /// `lore-revision` caller gets — and what stops a move recording a path change for a
    /// node that is on its way out.
    #[tokio::test]
    async fn move_node_rejects_a_node_the_revision_is_letting_go() {
        with_state(|repository, state| async move {
            let destination =
                add_node(&repository, &state, ROOT_NODE, "dst", NodeFlags::NoFlags).await;
            let deleted = add_node(
                &repository,
                &state,
                ROOT_NODE,
                "deleted.bin",
                NodeFlags::File,
            )
            .await;
            let discarded = add_node(
                &repository,
                &state,
                ROOT_NODE,
                "discarded.bin",
                NodeFlags::File,
            )
            .await;
            let doomed_parent =
                add_node(&repository, &state, ROOT_NODE, "doomed", NodeFlags::NoFlags).await;
            let mover =
                add_node(&repository, &state, ROOT_NODE, "mover.bin", NodeFlags::File).await;

            state
                .node_delete(repository.clone(), deleted)
                .await
                .expect("Failed to stage the deletion");
            state
                .node_delete(repository.clone(), doomed_parent)
                .await
                .expect("Failed to stage the destination's deletion");
            lore_revision::state::node_discard_patch(
                state.clone(),
                repository.clone(),
                discarded,
                |_node_id, _flags| {},
            )
            .await
            .expect("Failed to discard the node");

            let reason =
                move_failure(&repository, &state, deleted, destination, "deleted.bin").await;
            assert!(
                reason.contains("staged for deletion"),
                "a node staged for deletion is leaving the revision, not moving in it: {reason}"
            );
            let reason =
                move_failure(&repository, &state, discarded, destination, "discarded.bin").await;
            assert!(
                reason.contains("discarded"),
                "a discarded node is gone; its slot reads back as an empty directory: {reason}"
            );
            let reason = move_failure(&repository, &state, mover, doomed_parent, "mover.bin").await;
            assert!(
                reason.contains("would go with it"),
                "a destination staged for deletion would take the moved node with it: {reason}"
            );

            assert_eq!(
                state
                    .node(repository.clone(), mover)
                    .await
                    .expect("Failed to read the node")
                    .parent,
                ROOT_NODE,
                "every rejection must leave the node where it was"
            );
            assert!(
                state
                    .node_children(repository.clone(), destination)
                    .await
                    .expect("Failed to walk the destination")
                    .is_empty(),
                "and the destination empty"
            );
        })
        .await;
    }

    /// The rejections that are properties of the tree rather than of one call, each
    /// refused before anything is relinked.
    #[tokio::test]
    async fn move_node_rejects_what_would_break_the_tree() {
        with_state(|repository, state| async move {
            let directory =
                add_node(&repository, &state, ROOT_NODE, "dir", NodeFlags::NoFlags).await;
            let nested =
                add_node(&repository, &state, directory, "nested", NodeFlags::NoFlags).await;
            let file = add_node(&repository, &state, ROOT_NODE, "leaf.bin", NodeFlags::File).await;

            let reason = move_failure(&repository, &state, ROOT_NODE, directory, "root").await;
            assert!(
                reason.contains("does not name a movable node"),
                "the root is the revision itself and has no parent to move it under: {reason}"
            );
            let reason = move_failure(&repository, &state, directory, nested, "dir").await;
            assert!(
                reason.contains("descendants"),
                "a node moved into its own subtree takes the subtree out of the tree: {reason}"
            );
            let reason = move_failure(&repository, &state, directory, directory, "dir").await;
            assert!(
                reason.contains("descendants"),
                "and so does a node moved into itself: {reason}"
            );
            let reason = move_failure(&repository, &state, file, file, "leaf.bin").await;
            assert!(
                reason.contains("not a directory"),
                "only a directory holds children: {reason}"
            );
            let reason = move_failure(&repository, &state, file, ROOT_NODE, "leaf.bin").await;
            assert!(
                reason.contains("already under that parent by that name"),
                "a move that changes nothing would record a move the tree never made: {reason}"
            );
            let reason = move_failure(&repository, &state, file, ROOT_NODE, "with/slash").await;
            assert!(
                reason.contains("not storable"),
                "a name the name table would refuse is caught before anything is relinked: {reason}"
            );

            assert_eq!(
                state
                    .node(repository.clone(), directory)
                    .await
                    .expect("Failed to read the directory")
                    .parent,
                ROOT_NODE,
                "every rejection must leave the tree as it was"
            );
            assert_eq!(
                state
                    .node_children(repository.clone(), ROOT_NODE)
                    .await
                    .expect("Failed to walk the root"),
                vec![file, directory],
                "including the chain a relink would have rewritten"
            );
        })
        .await;
    }

    /// The name check belongs to the caller, as it does on `node_add`: a batch caller
    /// has to hold a name against the tree its whole batch produces, which this cannot
    /// see. Two siblings under one name is what the commit's validator refuses.
    #[tokio::test]
    async fn move_node_leaves_the_name_check_to_the_caller() {
        with_state(|repository, state| async move {
            let destination =
                add_node(&repository, &state, ROOT_NODE, "dst", NodeFlags::NoFlags).await;
            add_node(
                &repository,
                &state,
                destination,
                "taken.bin",
                NodeFlags::File,
            )
            .await;
            let node_id =
                add_node(&repository, &state, ROOT_NODE, "taken.bin", NodeFlags::File).await;

            state
                .move_node(repository.clone(), node_id, destination, "taken.bin")
                .await
                .expect("the primitive moves what it is told to move");

            assert_eq!(
                state
                    .node_children(repository.clone(), destination)
                    .await
                    .expect("Failed to walk the destination")
                    .len(),
                2,
                "both children are there under one name, for the caller's check to have \
                 prevented"
            );
        })
        .await;
    }
}

mod single_file_compare_result_tests {
    use std::path::Path;
    use std::sync::Arc;

    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_revision::change;
    use lore_revision::change::FileAction;
    use lore_revision::change::NodeChange;
    use lore_revision::change::NodeChangeState;
    use lore_revision::node::INVALID_NODE;
    use lore_revision::node::NodeFlags;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::state::SingleFileCompareResult;
    use lore_revision::state::State;
    use lore_revision::state::detect_and_coalesce_moves;
    use lore_revision::util::path::RelativePath;
    use lore_storage::local::immutable_store;
    use lore_storage::local::mutable_store;

    use crate::tests::default_repository_creation_args;

    #[test]
    fn debug_format_displays_variant_names() {
        assert_eq!(
            format!("{:?}", SingleFileCompareResult::Unmodified),
            "Unmodified"
        );
        assert_eq!(
            format!("{:?}", SingleFileCompareResult::Modified),
            "Modified"
        );
        assert_eq!(format!("{:?}", SingleFileCompareResult::NewFile), "NewFile");
        assert_eq!(
            format!("{:?}", SingleFileCompareResult::TypeChangedToFile),
            "TypeChangedToFile"
        );
        assert_eq!(
            format!("{:?}", SingleFileCompareResult::TypeChangedToDirectory),
            "TypeChangedToDirectory"
        );
    }

    fn make_change_state(repository: Arc<RepositoryContext>, context: Context) -> NodeChangeState {
        NodeChangeState {
            repository,
            state: Arc::new(State::new()),
            node: INVALID_NODE,
            flags: NodeFlags::NoFlags,
            address: Address {
                hash: Hash::default(),
                context,
            },
        }
    }

    fn make_change(
        repository: Arc<RepositoryContext>,
        action: FileAction,
        path: &str,
        from_context: Context,
        to_context: Context,
    ) -> NodeChange {
        NodeChange {
            action,
            flags: change::Flags::None,
            from: make_change_state(repository.clone(), from_context),
            to: make_change_state(repository, to_context),
            path: RelativePath::new_from_initial_path(path).unwrap_or_default(),
            from_path: None,
        }
    }

    /// Create a Context from a u128 value for testing
    fn context_from_u128(value: u128) -> Context {
        Context::from(value.to_ne_bytes())
    }

    #[tokio::test]
    async fn empty_changes_remains_empty() {
        let mut changes: Vec<NodeChange> = vec![];
        detect_and_coalesce_moves(&mut changes);
        assert!(changes.is_empty());
    }

    /// Create a test repository context
    async fn new_test_context() -> Arc<RepositoryContext> {
        let immutable = immutable_store::LocalImmutableStore::new(
            None,
            immutable_store::ImmutableStoreSettings::default(),
        )
        .await
        .expect("Failed to create store");
        let mutable = Arc::new(
            mutable_store::LocalMutableStore::new(
                None::<&Path>,
                lore_storage::MutableStoreSettings::default(),
                immutable.clone(),
            )
            .await
            .expect("Failed to create store"),
        );
        Arc::new(RepositoryContext::new(default_repository_creation_args(
            immutable, mutable,
        )))
    }

    #[tokio::test]
    async fn single_add_remains_unchanged() {
        let repo = new_test_context().await;
        let ctx = context_from_u128(1);
        let mut changes = vec![make_change(
            repo,
            FileAction::Add,
            "new_file.txt",
            Context::default(),
            ctx,
        )];

        detect_and_coalesce_moves(&mut changes);

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].action, FileAction::Add);
        assert_eq!(changes[0].path.as_str(), "new_file.txt");
    }

    #[tokio::test]
    async fn single_delete_remains_unchanged() {
        let repo = new_test_context().await;
        let ctx = context_from_u128(1);
        let mut changes = vec![make_change(
            repo,
            FileAction::Delete,
            "deleted_file.txt",
            ctx,
            Context::default(),
        )];

        detect_and_coalesce_moves(&mut changes);

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].action, FileAction::Delete);
        assert_eq!(changes[0].path.as_str(), "deleted_file.txt");
    }

    #[tokio::test]
    async fn matching_add_delete_coalesced_to_move() {
        let repo = new_test_context().await;
        let file_id = context_from_u128(42);

        let mut changes = vec![
            make_change(
                repo.clone(),
                FileAction::Delete,
                "old/path.txt",
                file_id,
                Context::default(),
            ),
            make_change(
                repo,
                FileAction::Add,
                "new/path.txt",
                Context::default(),
                file_id,
            ),
        ];

        detect_and_coalesce_moves(&mut changes);

        // Should have exactly one move change
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].action, FileAction::Move);
        assert_eq!(changes[0].path.as_str(), "new/path.txt");
        assert_eq!(
            changes[0].from_path.as_ref().map(|p| p.as_str()),
            Some("old/path.txt")
        );
    }

    #[tokio::test]
    async fn multiple_independent_moves_coalesced() {
        let repo = new_test_context().await;
        let file_id_1 = context_from_u128(1);
        let file_id_2 = context_from_u128(2);

        let mut changes = vec![
            make_change(
                repo.clone(),
                FileAction::Delete,
                "old/file1.txt",
                file_id_1,
                Context::default(),
            ),
            make_change(
                repo.clone(),
                FileAction::Delete,
                "old/file2.txt",
                file_id_2,
                Context::default(),
            ),
            make_change(
                repo.clone(),
                FileAction::Add,
                "new/file1.txt",
                Context::default(),
                file_id_1,
            ),
            make_change(
                repo,
                FileAction::Add,
                "new/file2.txt",
                Context::default(),
                file_id_2,
            ),
        ];

        detect_and_coalesce_moves(&mut changes);

        // Should have exactly two move changes
        assert_eq!(changes.len(), 2);

        // Both should be moves
        assert!(changes.iter().all(|c| c.action == FileAction::Move));

        // Both should have from_path set
        assert!(changes.iter().all(|c| c.from_path.is_some()));
    }

    #[tokio::test]
    async fn unmatched_add_and_delete_remain_unchanged() {
        let repo = new_test_context().await;
        let file_id_1 = context_from_u128(1);
        let file_id_2 = context_from_u128(2);

        let mut changes = vec![
            make_change(
                repo.clone(),
                FileAction::Delete,
                "deleted.txt",
                file_id_1,
                Context::default(),
            ),
            make_change(
                repo,
                FileAction::Add,
                "added.txt",
                Context::default(),
                file_id_2,
            ),
        ];

        detect_and_coalesce_moves(&mut changes);

        // Both should remain as they don't share context
        assert_eq!(changes.len(), 2);
        assert!(changes.iter().any(|c| c.action == FileAction::Delete));
        assert!(changes.iter().any(|c| c.action == FileAction::Add));
    }

    #[tokio::test]
    async fn zero_context_not_matched() {
        let repo = new_test_context().await;
        let zero_ctx = Context::default();

        let mut changes = vec![
            make_change(
                repo.clone(),
                FileAction::Delete,
                "deleted.txt",
                zero_ctx,
                zero_ctx,
            ),
            make_change(repo, FileAction::Add, "added.txt", zero_ctx, zero_ctx),
        ];

        detect_and_coalesce_moves(&mut changes);

        // Should remain unchanged since zero context is ignored
        assert_eq!(changes.len(), 2);
        assert!(changes.iter().any(|c| c.action == FileAction::Delete));
        assert!(changes.iter().any(|c| c.action == FileAction::Add));
    }

    #[tokio::test]
    async fn keep_changes_not_affected() {
        let repo = new_test_context().await;
        let file_id = context_from_u128(1);

        let mut changes = vec![
            make_change(
                repo.clone(),
                FileAction::Keep,
                "modified.txt",
                file_id,
                file_id,
            ),
            make_change(
                repo,
                FileAction::Delete,
                "old.txt",
                file_id,
                Context::default(),
            ),
        ];

        detect_and_coalesce_moves(&mut changes);

        // Keep should remain, delete should remain (no matching add)
        assert_eq!(changes.len(), 2);
        assert!(changes.iter().any(|c| c.action == FileAction::Keep));
        assert!(changes.iter().any(|c| c.action == FileAction::Delete));
    }

    #[tokio::test]
    async fn mixed_changes_only_moves_coalesced() {
        let repo = new_test_context().await;
        let move_file_id = context_from_u128(1);
        let keep_file_id = context_from_u128(2);
        let delete_file_id = context_from_u128(3);
        let add_file_id = context_from_u128(4);

        let mut changes = vec![
            make_change(
                repo.clone(),
                FileAction::Delete,
                "moved_from.txt",
                move_file_id,
                Context::default(),
            ),
            make_change(
                repo.clone(),
                FileAction::Keep,
                "kept.txt",
                keep_file_id,
                keep_file_id,
            ),
            make_change(
                repo.clone(),
                FileAction::Delete,
                "truly_deleted.txt",
                delete_file_id,
                Context::default(),
            ),
            make_change(
                repo.clone(),
                FileAction::Add,
                "moved_to.txt",
                Context::default(),
                move_file_id,
            ),
            make_change(
                repo,
                FileAction::Add,
                "truly_added.txt",
                Context::default(),
                add_file_id,
            ),
        ];

        detect_and_coalesce_moves(&mut changes);

        // Should have 4 changes: 1 move, 1 keep, 1 delete, 1 add
        assert_eq!(changes.len(), 4);
        assert_eq!(
            changes
                .iter()
                .filter(|c| c.action == FileAction::Move)
                .count(),
            1
        );
        assert_eq!(
            changes
                .iter()
                .filter(|c| c.action == FileAction::Keep)
                .count(),
            1
        );
        assert_eq!(
            changes
                .iter()
                .filter(|c| c.action == FileAction::Delete)
                .count(),
            1
        );
        assert_eq!(
            changes
                .iter()
                .filter(|c| c.action == FileAction::Add)
                .count(),
            1
        );

        // The move should have correct from_path
        let move_change = changes
            .iter()
            .find(|c| c.action == FileAction::Move)
            .unwrap();
        assert_eq!(move_change.path.as_str(), "moved_to.txt");
        assert_eq!(
            move_change.from_path.as_ref().map(|p| p.as_str()),
            Some("moved_from.txt")
        );
    }

    #[tokio::test]
    async fn from_state_copied_from_delete_to_move() {
        let repo = new_test_context().await;
        let file_id = context_from_u128(42);

        let mut delete_change = make_change(
            repo.clone(),
            FileAction::Delete,
            "old/path.txt",
            file_id,
            Context::default(),
        );
        // Set a specific from address hash to verify it's copied
        delete_change.from.address.hash = Hash::from_u64(12345);

        let mut changes = vec![
            delete_change,
            make_change(
                repo,
                FileAction::Add,
                "new/path.txt",
                Context::default(),
                file_id,
            ),
        ];

        detect_and_coalesce_moves(&mut changes);

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].action, FileAction::Move);
        // The from state should have been copied from the delete
        // Verify that the from address was copied (using the hash we set)
        assert_eq!(changes[0].from.address.hash, Hash::from_u64(12345));
        // Also verify the file_id (context) is preserved in the from state
        assert_eq!(changes[0].from.address.context, file_id);
    }
}

/// Tests for `is_file_modified` against objects whose stored fragmentation is not the one
/// the current chunker would produce — here multiple 64 KiB chunks. Replaying the stored
/// list is the only comparison that holds, since a commit may reuse a previous
/// fragmentation and the stored hash is then a function of that history too.
mod is_file_modified_chunking_compat {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::future::Future;
    use std::path::PathBuf;
    use std::sync::Arc;

    use bytes::Bytes;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Fragment;
    use lore_base::types::FragmentFlags;
    use lore_base::types::FragmentReference;
    use lore_base::types::Hash;
    use lore_revision::immutable;
    use lore_revision::node::Node;
    use lore_revision::node::NodeFlags;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::state::file_modification;
    use lore_revision::util::path::RelativePath;
    use rand::Rng;
    use zerocopy::IntoBytes;

    include!("helper.rs");

    /// Store raw content chunks and a fragment list that references them,
    /// returning the root address whose hash is the hash of the fragment list
    /// payload. This simulates content that was written under the old 64 KiB
    /// chunking threshold.
    async fn store_as_legacy_chunks(
        repository: &Arc<RepositoryContext>,
        context: Context,
        content: &[u8],
        chunk_size: usize,
    ) -> Address {
        let chunks: Vec<&[u8]> = content.chunks(chunk_size).collect();

        // Store each chunk as a raw fragment
        let mut refs = Vec::with_capacity(chunks.len());
        let mut offset: u64 = 0;
        for chunk in &chunks {
            let chunk_bytes = Bytes::copy_from_slice(chunk);
            let hash = Hash::hash_buffer(chunk);
            let address = Address { hash, context };
            let fragment = Fragment {
                flags: FragmentFlags::PayloadStoredLocal.bits(),
                size_payload: chunk.len() as u32,
                size_content: chunk.len() as u64,
            };
            immutable::store_raw(
                repository.clone(),
                address,
                fragment,
                chunk_bytes,
                true,
                false,
            )
            .await
            .expect("Failed to store chunk fragment");

            refs.push(FragmentReference {
                hash,
                offset_content: offset,
            });
            offset += chunk.len() as u64;
        }

        // Serialize the fragment reference list to bytes
        let list_bytes: Vec<u8> = refs.as_slice().as_bytes().to_vec();
        let list_bytes = Bytes::from(list_bytes);
        let list_hash = Hash::hash_buffer(list_bytes.as_ref());

        // Store the fragment list with PayloadFragmented flag
        let list_address = Address {
            hash: list_hash,
            context,
        };
        let list_fragment = Fragment {
            flags: FragmentFlags::PayloadStoredLocal.bits()
                | FragmentFlags::PayloadFragmented.bits(),
            size_payload: list_bytes.len() as u32,
            size_content: content.len() as u64,
        };
        immutable::store_raw(
            repository.clone(),
            list_address,
            list_fragment,
            list_bytes,
            true,
            false,
        )
        .await
        .expect("Failed to store fragment list");

        list_address
    }

    /// Runs `body` on a repository rooted in a fresh temporary directory, inside the
    /// execution context the store operations read.
    ///
    /// The directory outlives `body`, which is what lets it write the files to hash.
    async fn on_a_repository<Body, Run>(body: Body)
    where
        Body: FnOnce(Arc<RepositoryContext>, PathBuf, Context) -> Run,
        Run: Future<Output = ()>,
    {
        let tempdir = generate_tempdir();
        let dir = tempdir.path().to_path_buf();

        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        LORE_CONTEXT
            .scope(execution, async move {
                let context: Context = rand::random();
                let repository = Arc::new(RepositoryContext::new(
                    default_repository_creation_args(immutable_store, mutable_store)
                        .with_path(dir.as_path())
                        .with_id(rand::random()),
                ));

                body(repository, dir, context).await;
            })
            .await;
    }

    /// `size` bytes of random content, which no chunker collapses into fewer chunks than
    /// the sizes it is stored under.
    fn random_content(size: usize) -> Vec<u8> {
        let mut rng = rand::rng();
        (0..size).map(|_| rng.random_range(0..=255u8)).collect()
    }

    /// A buffer hash is tried first for a file this size and misses here, because the stored
    /// object is a list no buffer hash can equal. The miss settles nothing, so the stored
    /// list is walked and recognises the content.
    #[tokio::test]
    async fn a_buffer_hash_miss_falls_through_to_the_stored_list() {
        on_a_repository(|repository, dir, context| async move {
            let size = 128 * 1024;
            let content = random_content(size);
            let address = store_as_legacy_chunks(&repository, context, &content, 64 * 1024).await;
            let path = dir.join("small.bin");
            std::fs::write(&path, &content).expect("Failed to write small file");

            assert_ne!(
                Hash::hash_buffer(&content),
                address.hash,
                "a buffer hash cannot equal a fragment list hash"
            );
            assert_eq!(
                immutable::file_matches(
                    repository.clone(),
                    address,
                    Some(size),
                    &lore_storage::ContentHashMemo::new(path.as_path()),
                )
                .await
                .expect("Failed to compare small file"),
                lore_storage::FileMatch::Match,
                "the stored list holds this content whatever the buffer hash says"
            );
        })
        .await;
    }

    /// The fall-through walks the stored list rather than assuming the buffer hash miss was
    /// the answer, so it still reports a small file whose content really did change.
    #[tokio::test]
    async fn a_modified_small_file_differs_through_the_stored_list() {
        on_a_repository(|repository, dir, context| async move {
            let size = 128 * 1024;
            let content = random_content(size);
            let address = store_as_legacy_chunks(&repository, context, &content, 64 * 1024).await;

            let mut modified = content.clone();
            modified[size / 2] ^= 0xff;
            let path = dir.join("small.bin");
            std::fs::write(&path, &modified).expect("Failed to write small file");

            assert_eq!(
                immutable::file_matches(
                    repository.clone(),
                    address,
                    Some(size),
                    &lore_storage::ContentHashMemo::new(path.as_path()),
                )
                .await
                .expect("Failed to compare small file"),
                lore_storage::FileMatch::Differs,
            );
        })
        .await;
    }

    /// A clone into a directory of existing files starts with an empty store, so there is no
    /// stored fragmentation to measure the file against. Hashing it under the current
    /// chunking is what recognises it, reading nothing but the file.
    #[tokio::test]
    async fn a_large_file_is_recognised_with_nothing_in_the_store() {
        on_a_repository(|repository, dir, context| async move {
            let size = 1_234_567;
            let content = random_content(size);
            let path = dir.join("large.bin");
            std::fs::write(&path, &content).expect("Failed to write large file");

            let (address, _) = immutable::write_from_file(
                repository.clone(),
                path.as_path(),
                context,
                lore_storage::WriteOptions::default().no_remote_write(),
            )
            .await
            .expect("Failed to store large file");

            let (empty_immutable, empty_mutable, _) =
                test_store_create().await.expect("Failed to create stores");
            let empty = Arc::new(RepositoryContext::new(
                default_repository_creation_args(empty_immutable, empty_mutable)
                    .with_path(dir.as_path())
                    .with_id(rand::random()),
            ));

            assert_eq!(
                immutable::file_matches(
                    empty,
                    address,
                    Some(size),
                    &lore_storage::ContentHashMemo::new(path.as_path()),
                )
                .await
                .expect("Failed to compare large file"),
                lore_storage::FileMatch::Match,
                "the current chunking reproduces the address it was stored under"
            );
        })
        .await;
    }

    /// Above the threshold the file is measured against the stored object's own chunking,
    /// which is what settles a difference without reading any stored content.
    #[tokio::test]
    async fn a_modified_large_file_differs_against_the_stored_chunking() {
        on_a_repository(|repository, dir, context| async move {
            let size = 640 * 1024;
            let content = random_content(size);
            let address = store_as_legacy_chunks(&repository, context, &content, 64 * 1024).await;

            let mut modified = content.clone();
            modified[size / 2] ^= 0xff;
            let path = dir.join("large.bin");
            std::fs::write(&path, &modified).expect("Failed to write large file");

            assert_eq!(
                immutable::file_matches(
                    repository.clone(),
                    address,
                    Some(size),
                    &lore_storage::ContentHashMemo::new(path.as_path()),
                )
                .await
                .expect("Failed to compare large file"),
                lore_storage::FileMatch::Differs,
            );
        })
        .await;
    }

    /// The verdict has to reach `is_file_modified`, which is the caller that would otherwise
    /// read the whole stored object to reach the same answer.
    #[tokio::test]
    async fn a_modified_fragmented_file_is_reported_modified() {
        on_a_repository(|repository, dir, context| async move {
            let size = 640 * 1024;
            let content = random_content(size);
            let address = store_as_legacy_chunks(&repository, context, &content, 64 * 1024).await;

            let mut modified = content.clone();
            modified[size / 2] ^= 0xff;
            let path = dir.join("large.bin");
            std::fs::write(&path, &modified).expect("Failed to write large file");

            let metadata = std::fs::metadata(&path).expect("Failed to get metadata");
            let (mtime, file_size) = lore_revision::util::fs::file_mtime_and_size(&metadata);
            let node = Node {
                flags: NodeFlags::File.bits(),
                size: size as u64,
                address,
                ..Default::default()
            };

            let is_modified = file_modification(
                repository.clone(),
                &node,
                mtime,
                file_size,
                &RelativePath::new_from_initial_path("large.bin").unwrap(),
                true,
                None,
            )
            .await
            .expect("file_modification failed")
            .is_modified();

            assert!(is_modified, "a file with one byte changed is modified");
        })
        .await;
    }

    /// 128 KiB file stored as two 64 KiB chunks under the old strategy, unmodified on disk.
    /// Under the current threshold one fragment would cover it, so the buffer hash misses and
    /// the stored list is what recognises the content.
    #[tokio::test]
    async fn unmodified_128k_file_with_legacy_two_chunk_fragmentation() {
        on_a_repository(|repository, dir, context| async move {
            let content_size = 128 * 1024;
            let content = random_content(content_size);
            let root_address =
                store_as_legacy_chunks(&repository, context, &content, 64 * 1024).await;

            let file_path = dir.join("test_file_128k.bin");
            std::fs::write(&file_path, &content).expect("Failed to write test file");

            let metadata = std::fs::metadata(&file_path).expect("Failed to get metadata");
            let relative_path = RelativePath::new_from_initial_path("test_file_128k.bin").unwrap();
            let node = Node {
                flags: NodeFlags::File.bits(),
                size: content_size as u64,
                address: root_address,
                ..Default::default()
            };

            let (mtime, size) = lore_revision::util::fs::file_mtime_and_size(&metadata);
            let modified =
                file_modification(
                    repository.clone(),
                    &node,
                    mtime,
                    size,
                    &relative_path,
                    true,
                    None,
                )
                    .await
                    .expect("file_modification failed")
            .is_modified();

            assert!(
                !modified,
                "128 KiB file with legacy two-chunk fragmentation should NOT be detected as modified"
            );
        })
        .await;
    }

    /// 640 KiB file stored as ten 64 KiB chunks. Each chunk is validated against the stored
    /// list individually, so the file is recognised without reading any stored content.
    #[tokio::test]
    async fn unmodified_640k_file_reuses_previous_chunk_fragmentation() {
        on_a_repository(|repository, dir, context| async move {
            let content_size = 640 * 1024;
            let content = random_content(content_size);
            let root_address =
                store_as_legacy_chunks(&repository, context, &content, 64 * 1024).await;

            let file_path = dir.join("test_file_640k.bin");
            std::fs::write(&file_path, &content).expect("Failed to write test file");

            let metadata = std::fs::metadata(&file_path).expect("Failed to get metadata");
            let relative_path = RelativePath::new_from_initial_path("test_file_640k.bin").unwrap();
            let node = Node {
                flags: NodeFlags::File.bits(),
                size: content_size as u64,
                address: root_address,
                ..Default::default()
            };

            let (mtime, size) = lore_revision::util::fs::file_mtime_and_size(&metadata);
            let modified =
                file_modification(
                    repository.clone(),
                    &node,
                    mtime,
                    size,
                    &relative_path,
                    true,
                    None,
                )
                    .await
                    .expect("file_modification failed")
            .is_modified();

            assert!(
                !modified,
                "640 KiB file with legacy ten-chunk fragmentation should NOT be detected as modified"
            );
        })
        .await;
    }
}

mod block_single_flight {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;

    use async_trait::async_trait;
    use bytes::Bytes;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Fragment;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_base::types::TypedBytes;
    use lore_revision::immutable;
    use lore_revision::interface::ExecutionContext;
    use lore_revision::node::Node;
    use lore_revision::node::NodeFileMetadata;
    use lore_revision::node::NodeFileMetadataBlock;
    use lore_revision::node::ROOT_NODE;
    use lore_revision::node::node_to_file_metadata;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryWriteToken;
    use lore_revision::state::State;
    use lore_storage::ImmutableStore;
    use lore_storage::MutableStore;
    use lore_storage::StoreError;
    use lore_storage::StoreGetData;
    use lore_storage::StoreMatchResult;
    use lore_storage::StoreObliterateStats;
    use lore_storage::hash::hash_string;
    use lore_storage::local::immutable_store::LocalImmutableStore;

    use crate::tests::RepositoryContextCreationArgsExt;
    use crate::tests::TempDir;
    use crate::tests::default_repository_creation_args;
    use crate::tests::generate_tempdir;
    use crate::tests::test_store_create;

    /// Tasks per burst. Large enough that a store read per task is unmistakable
    /// next to the handful the state needs whatever the concurrency.
    const BURST: usize = 32;

    /// How long each payload read is held open. Every task in a burst arrives
    /// inside this window, so a state that does not gate its block reads does
    /// them all, rather than losing a race it would usually win by accident.
    const READ_DELAY: Duration = Duration::from_millis(50);

    /// Counts the payload reads reaching the store underneath, per address, and
    /// paces them.
    struct CountingStore {
        inner: Arc<dyn ImmutableStore>,
        reads: Mutex<BTreeMap<Address, u32>>,
    }

    impl CountingStore {
        fn take_reads(&self) -> BTreeMap<Address, u32> {
            std::mem::take(&mut *self.reads.lock().expect("Read counter poisoned"))
        }
    }

    impl std::fmt::Debug for CountingStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "CountingStore")
        }
    }

    #[async_trait]
    impl ImmutableStore for CountingStore {
        async fn get(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
        ) -> Result<StoreGetData, StoreError> {
            *self
                .reads
                .lock()
                .expect("Read counter poisoned")
                .entry(address)
                .or_default() += 1;
            tokio::time::sleep(READ_DELAY).await;
            self.inner.clone().get(partition, address).await
        }

        async fn get_metadata(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
        ) -> Result<StoreGetData, StoreError> {
            self.inner.clone().get_metadata(partition, address).await
        }

        async fn query(
            self: Arc<Self>,
            partition: Partition,
            addresses: &[Address],
            results: &mut [StoreMatchResult],
        ) -> Result<(), StoreError> {
            self.inner
                .clone()
                .query(partition, addresses, results)
                .await
        }

        async fn put(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
            fragment: Fragment,
            payload: Option<Bytes>,
            force: bool,
        ) -> Result<(), StoreError> {
            self.inner
                .clone()
                .put(partition, address, fragment, payload, force)
                .await
        }

        async fn obliterate(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
            stats: Arc<StoreObliterateStats>,
        ) -> Result<(), StoreError> {
            self.inner
                .clone()
                .obliterate(partition, address, stats)
                .await
        }

        async fn evict(
            self: Arc<Self>,
            max_capacity: usize,
            sync_data: bool,
            sink: Option<lore_storage::gc_event::GcEventSinkRef>,
        ) -> Result<usize, StoreError> {
            self.inner
                .clone()
                .evict(max_capacity, sync_data, sink)
                .await
        }

        async fn compact(
            self: Arc<Self>,
            max_size: usize,
            at: Option<usize>,
            sync_data: bool,
            sink: Option<lore_storage::gc_event::GcEventSinkRef>,
        ) -> Result<Option<usize>, StoreError> {
            self.inner
                .clone()
                .compact(max_size, at, sync_data, sink)
                .await
        }

        async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
            self.inner.clone().compact_resume_at().await
        }

        fn max_query_batch(&self) -> Option<usize> {
            self.inner.max_query_batch()
        }

        async fn flush(self: Arc<Self>, sync_data: bool) -> Result<(), StoreError> {
            self.inner.clone().flush(sync_data).await
        }

        async fn verify(self: Arc<Self>, heal: bool) -> Result<(), StoreError> {
            self.inner.clone().verify(heal).await
        }

        async fn copy(
            self: Arc<Self>,
            source_partition: Partition,
            source_address: Address,
            destination_partition: Partition,
            destination_context: Context,
            durable: bool,
        ) -> Result<(), StoreError> {
            self.inner
                .clone()
                .copy(
                    source_partition,
                    source_address,
                    destination_partition,
                    destination_context,
                    durable,
                )
                .await
        }
    }

    /// A repository over a counting store, and a state written into it carrying
    /// one node block and one file metadata block.
    struct Seed {
        _tempdir: TempDir,
        store: Arc<CountingStore>,
        repository: Arc<RepositoryContext>,
        signature: Hash,
        block_index: usize,
        /// Store address of the node block, so a burst can be held to reading
        /// that address once rather than to a total that says nothing about
        /// which block was read.
        node_block: Address,
        /// Store address of the file metadata block.
        metadata_block: Address,
    }

    /// Store address of block `block_index` in the address list rooted at `list`.
    async fn block_address(
        repository: &Arc<RepositoryContext>,
        list: Hash,
        block_index: usize,
    ) -> Address {
        let bytes = immutable::read(
            repository.clone(),
            Address::zero_context_hash(list),
            None,
            immutable::read_options_from_repository(repository),
        )
        .await
        .expect("Failed to read a block address list");
        Address::zero_context_hash(bytes.as_type_slice::<Hash>()[block_index])
    }

    async fn seeded_repository(mutable_store: Arc<dyn MutableStore>) -> Seed {
        let tempdir = generate_tempdir();
        let path = tempdir.to_path_buf();

        let store = Arc::new(CountingStore {
            inner: LocalImmutableStore::new(
                None,
                lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
            )
            .await
            .expect("Failed to create store"),
            reads: Mutex::default(),
        });

        let write_token = RepositoryWriteToken::acquire(path.as_path()).await;
        let repository = Arc::new(
            RepositoryContext::new(
                default_repository_creation_args(store.clone(), mutable_store).with_path(&path),
            )
            .with_write_token(write_token.share()),
        );

        let state = State::new();
        let node = state
            .node_add(
                repository.clone(),
                ROOT_NODE,
                Node {
                    name_hash: hash_string("first"),
                    ..Default::default()
                },
                "first",
            )
            .await
            .expect("Failed to add a node");

        let metadata_node = node_to_file_metadata(node);
        let block_index = NodeFileMetadataBlock::index(metadata_node);
        let metadata_block = state
            .block_file_metadata(repository.clone(), block_index)
            .await
            .expect("Failed to read the file metadata block");
        {
            let mut writer = metadata_block.write();
            writer.node(NodeFileMetadata::index(metadata_node)).metadata = Hash::from_u64(9);
            writer.mark_dirty();
        }
        state.block_file_metadata_modified(metadata_block, block_index);
        state.mark_dirty();

        let signature = state
            .serialize(repository.clone(), &write_token)
            .await
            .expect("Failed to serialize");

        let tree = State::deserialize(repository.clone(), signature)
            .await
            .expect("Failed to deserialize state")
            .tree(repository.clone())
            .await
            .expect("Failed to load the tree");

        Seed {
            node_block: block_address(&repository, tree.hash_node, block_index).await,
            metadata_block: block_address(&repository, tree.hash_file_metadata, block_index).await,
            _tempdir: tempdir,
            store,
            repository,
            signature,
            block_index,
        }
    }

    /// Payload reads per address, taken while [`BURST`] tasks all ask one freshly
    /// deserialized state for the same block.
    ///
    /// The tree is loaded before counting starts, because it has a check-then-read
    /// of its own that the burst would otherwise measure instead.
    ///
    /// Blocks are held until the counts are taken, as a walk holds one it is
    /// descending through. A file metadata block is published only as a `Weak`, so
    /// a burst that dropped each block on arrival would read it again for the next
    /// task whatever the gate does.
    async fn reads_during_burst<F, Fut, T>(
        seed: &Seed,
        execution: &Arc<ExecutionContext>,
        read: F,
    ) -> BTreeMap<Address, u32>
    where
        F: Fn(Arc<State>, Arc<RepositoryContext>) -> Fut + Copy + Send + 'static,
        Fut: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let state = State::deserialize(seed.repository.clone(), seed.signature)
            .await
            .expect("Failed to deserialize state");
        state
            .tree(seed.repository.clone())
            .await
            .expect("Failed to load the tree");

        seed.store.take_reads();

        let mut tasks = Vec::with_capacity(BURST);
        for _ in 0..BURST {
            let state = state.clone();
            let repository = seed.repository.clone();
            #[allow(clippy::disallowed_methods)]
            tasks.push(
                runtime().spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                    read(state, repository).await
                })),
            );
        }
        let mut held = Vec::with_capacity(BURST);
        for task in tasks {
            held.push(task.await.expect("Block read task failed"));
        }

        let reads = seed.store.take_reads();
        drop(held);
        reads
    }

    /// Every walk descending through a block that is not resident wants it at the
    /// same time, and deserializing one is a store read and a decompress. The
    /// burst must cost the store one read of that block.
    #[tokio::test]
    async fn concurrent_readers_of_a_node_block_read_it_once() {
        let (_, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let spawn_execution = execution.clone();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let seed = Box::pin(seeded_repository(mutable_store)).await;
                let block_index = seed.block_index;

                let reads = reads_during_burst(
                    &seed,
                    &spawn_execution,
                    move |state: Arc<State>, repository: Arc<RepositoryContext>| async move {
                        state
                            .block(repository, block_index)
                            .await
                            .expect("Failed to read the node block")
                    },
                )
                .await;

                assert_eq!(
                    reads.get(&seed.node_block).copied(),
                    Some(1),
                    "{BURST} readers of one node block must read it once"
                );
                assert!(
                    reads.values().all(|&count| count == 1),
                    "the burst must read nothing twice, got {reads:?}"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// The file metadata block is reached the same way and is far larger, and it
    /// draws on the same permits, so it owes the same guarantee.
    #[tokio::test]
    async fn concurrent_readers_of_a_file_metadata_block_read_it_once() {
        let (_, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let spawn_execution = execution.clone();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let seed = Box::pin(seeded_repository(mutable_store)).await;
                let block_index = seed.block_index;

                let reads = reads_during_burst(
                    &seed,
                    &spawn_execution,
                    move |state: Arc<State>, repository: Arc<RepositoryContext>| async move {
                        state
                            .block_file_metadata(repository, block_index)
                            .await
                            .expect("Failed to read the file metadata block")
                    },
                )
                .await;

                assert_eq!(
                    reads.get(&seed.metadata_block).copied(),
                    Some(1),
                    "{BURST} readers of one file metadata block must read it once"
                );
                assert!(
                    reads.values().all(|&count| count == 1),
                    "the burst must read nothing twice, got {reads:?}"
                );
            }))
            .await
            .expect("Test task failed");
    }
}
