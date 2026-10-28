// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Address;
    use lore_base::types::BranchPoint;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_revision::branch;
    use lore_revision::change;
    use lore_revision::change::FileAction;
    use lore_revision::commit::commit_in_memory_revision;
    use lore_revision::commit::freeze_and_serialize;
    use lore_revision::commit::resolve_commit_branch;
    use lore_revision::metadata::CREATED_BY;
    use lore_revision::metadata::Metadata;
    use lore_revision::node::*;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::state::State;
    use lore_revision::state::StateNodeChildrenIterator;
    use lore_storage::hash::hash_string;
    use lore_storage::local::immutable_store::LocalImmutableStore;

    include!("helper.rs");

    use in_memory::add;
    use in_memory::branch_id;
    use in_memory::directory;
    use in_memory::file;
    use in_memory::metadata_on;
    use in_memory::token;

    /// A path-less context, as the memory-based revision tree handle builds, so the
    /// primitive is exercised against the shape it actually runs on.
    async fn test_repository(
        mutable_store: Arc<dyn lore_storage::MutableStore>,
    ) -> Arc<RepositoryContext> {
        let immutable_store = LocalImmutableStore::new(
            None,
            lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
        )
        .await
        .expect("Failed to create store");
        in_memory::repository(immutable_store, mutable_store)
    }

    /// The directory is checked apart from the file: its flags are cleared by
    /// `rehash_directory`, not by the freeze walk, which only reaches files and
    /// links.
    #[tokio::test]
    async fn commit_in_memory_revision_publishes_the_tree_and_advances_the_tip() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;
                let branch = branch_id();
                let state = State::new();
                let dir = add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    directory("dir"),
                    "dir",
                )
                .await;
                add(&state, repository.clone(), dir, file("a.bin"), "a.bin").await;

                let revision = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    state.clone(),
                    metadata_on(branch),
                    Hash::default(),
                    branch,
                )
                .await
                .expect("committing the initial revision must succeed");

                assert!(!revision.is_zero(), "the commit must produce a revision");
                assert_eq!(
                    branch::load_latest(repository.clone(), branch)
                        .await
                        .expect("the branch tip must read back"),
                    revision,
                    "the branch tip must advance to the new revision"
                );

                let published = State::deserialize(repository.clone(), revision)
                    .await
                    .expect("the committed revision must deserialize");
                let link = published
                    .find_node_link(repository.clone(), "dir/a.bin")
                    .await
                    .expect("the committed tree must hold the file");
                let node = published
                    .node(repository.clone(), link.node)
                    .await
                    .expect("the node must read back");
                assert!(
                    !node.is_dirty_or_staged(),
                    "a committed node must carry no change flags, got 0x{:x}",
                    node.flags
                );

                let committed_dir = published
                    .node(repository, dir)
                    .await
                    .expect("the directory must read back");
                assert!(
                    !committed_dir.is_dirty_or_staged(),
                    "a committed directory must carry no change flags, got 0x{:x}",
                    committed_dir.flags
                );
            }))
            .await
            .expect("Task failed");
    }

    /// `first`, holding a file and the directory `inner`, beside `second`: the freeze must reach
    /// all three. A directory it skipped would leave its file staged, which the commit refuses.
    #[tokio::test]
    async fn commit_in_memory_revision_freezes_every_directory_of_a_nested_tree() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;
                let branch = branch_id();
                let state = State::new();
                let first = add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    directory("first"),
                    "first",
                )
                .await;
                add(&state, repository.clone(), first, file("a.bin"), "a.bin").await;
                let inner = add(
                    &state,
                    repository.clone(),
                    first,
                    directory("inner"),
                    "inner",
                )
                .await;
                add(&state, repository.clone(), inner, file("b.bin"), "b.bin").await;
                let second = add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    directory("second"),
                    "second",
                )
                .await;
                add(&state, repository.clone(), second, file("c.bin"), "c.bin").await;

                let revision = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    state,
                    metadata_on(branch),
                    Hash::default(),
                    branch,
                )
                .await
                .expect("a nested tree must commit");

                let published = State::deserialize(repository.clone(), revision)
                    .await
                    .expect("the committed revision must deserialize");
                for path in ["first/a.bin", "first/inner/b.bin", "second/c.bin"] {
                    let link = published
                        .find_node_link(repository.clone(), path)
                        .await
                        .unwrap_or_else(|err| {
                            panic!("the committed tree must hold {path}: {err:?}")
                        });
                    let node = published
                        .node(repository.clone(), link.node)
                        .await
                        .expect("the node must read back");
                    assert!(
                        !node.is_dirty_or_staged(),
                        "{path} must carry no change flags, got 0x{:x}",
                        node.flags
                    );
                }
            }))
            .await
            .expect("Task failed");
    }

    /// The freeze treats a staged link exactly like a file — there is nothing to
    /// fragment and no subtree of its own to walk, since that lives in the linked
    /// repository. What has to survive is its identity as a link: the address is the
    /// target `(revision, repository)` and `child` is the target node, which is why
    /// the walk does not zero `child` the way `commit_file` does for a file.
    #[tokio::test]
    async fn commit_in_memory_revision_keeps_a_link_pointing_at_its_target() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;
                let branch = branch_id();
                let state = State::new();

                let target_revision = Hash::from_u64(0xabcd);
                let target_repository = Context::from(uuid::Uuid::now_v7());
                let target_node: NodeID = 7;
                let link = Node {
                    flags: NodeFlags::Link.bits(),
                    mode: 0o755,
                    name_hash: hash_string("vendor"),
                    child: target_node,
                    address: Address {
                        hash: target_revision,
                        context: target_repository,
                    },
                    ..Default::default()
                };
                let link_id = add(&state, repository.clone(), ROOT_NODE, link, "vendor").await;

                let revision = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    state,
                    metadata_on(branch),
                    Hash::default(),
                    branch,
                )
                .await
                .expect("a revision holding a link must commit");

                let published = State::deserialize(repository.clone(), revision)
                    .await
                    .expect("the committed revision must deserialize");
                let committed = published
                    .node(repository, link_id)
                    .await
                    .expect("the link must read back");
                assert!(committed.is_link(), "it must still be a link");
                assert!(
                    !committed.is_dirty_or_staged(),
                    "a committed link must carry no change flags, got 0x{:x}",
                    committed.flags
                );
                assert_eq!(
                    committed.address.hash, target_revision,
                    "the link must still address its target revision"
                );
                assert_eq!(
                    committed.address.context, target_repository,
                    "the link must still address its target repository"
                );
                assert_eq!(
                    committed.child, target_node,
                    "the link must still point at its target node"
                );
            }))
            .await
            .expect("Task failed");
    }

    /// Node ids are allocated sequentially, so anything under `BLOCK_NODE_COUNT` sits
    /// entirely in block zero and never crosses a boundary — which is where the
    /// per-block work a commit does (a serialize task per dirty block, the discard
    /// list, the delta buffer) could go wrong without any smaller test noticing.
    /// The last id allocated is in the highest block, so reading it back is what
    /// proves the block beyond the first was serialized and reloaded.
    #[tokio::test]
    async fn commit_in_memory_revision_publishes_more_nodes_than_one_block_holds() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;
                let branch = branch_id();
                let state = State::new();

                let count = BLOCK_NODE_COUNT * 3;
                let mut last = ROOT_NODE;
                for index in 0..count {
                    let name = format!("file-{index}.bin");
                    last = add(&state, repository.clone(), ROOT_NODE, file(&name), &name).await;
                }

                let revision = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    state,
                    metadata_on(branch),
                    Hash::default(),
                    branch,
                )
                .await
                .expect("a tree spanning several blocks must commit");

                let published = State::deserialize(repository.clone(), revision)
                    .await
                    .expect("the committed revision must deserialize");

                let mut children = StateNodeChildrenIterator::new(
                    published.clone(),
                    repository.clone(),
                    ROOT_NODE,
                )
                .await
                .expect("the published root must iterate");
                let mut published_count = 0usize;
                while let Some((_node_id, node)) =
                    children.next().await.expect("the chain must walk")
                {
                    assert!(
                        !node.is_dirty_or_staged(),
                        "every committed node must be clean, got 0x{:x}",
                        node.flags
                    );
                    published_count += 1;
                }
                assert_eq!(
                    published_count, count,
                    "every added node must survive the commit"
                );

                let tail = published
                    .node(repository, last)
                    .await
                    .expect("a node in the last block must read back");
                assert_eq!(
                    tail.name_hash,
                    hash_string(&format!("file-{}.bin", count - 1))
                );
            }))
            .await
            .expect("Task failed");
    }

    /// A handle nobody edited has nothing to publish, and the error is the one the
    /// working-tree commit returns for an empty staged set.
    #[tokio::test]
    async fn commit_in_memory_revision_rejects_a_state_with_no_edits() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;
                let branch = branch_id();

                let error = commit_in_memory_revision(
                    repository,
                    &token(),
                    State::new(),
                    Metadata::new(),
                    Hash::default(),
                    branch,
                )
                .await
                .expect_err("an unedited handle must not produce a revision");
                assert!(
                    error.error.is_nothing_staged(),
                    "Expected NothingStaged, got {error}"
                );
                assert!(
                    !error.tree_mutated(),
                    "a rejection before the freeze must leave the tree alone"
                );
            }))
            .await
            .expect("Task failed");
    }

    /// Metadata alone is a revision: the caller changed what the revision records
    /// even though the tree is identical.
    #[tokio::test]
    async fn commit_in_memory_revision_publishes_a_metadata_only_revision() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;
                let branch = branch_id();

                commit_in_memory_revision(
                    repository,
                    &token(),
                    State::new(),
                    metadata_on(branch),
                    Hash::default(),
                    branch,
                )
                .await
                .expect("a metadata-only revision must commit");
            }))
            .await
            .expect("Task failed");
    }

    #[tokio::test]
    async fn commit_in_memory_revision_returns_branch_advanced_when_the_tip_moved() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;
                let branch = branch_id();
                let state = State::new();
                add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    file("a.bin"),
                    "a.bin",
                )
                .await;

                branch::store_latest(
                    repository.clone(),
                    branch,
                    Hash::default(),
                    Hash::from_u64(0xfeed),
                    branch::BranchLatestStatus::Divergent,
                )
                .await
                .expect("seeding another committer's tip must succeed");

                let error = commit_in_memory_revision(
                    repository,
                    &token(),
                    state,
                    metadata_on(branch),
                    Hash::default(),
                    branch,
                )
                .await
                .expect_err("a tip that moved must not be overwritten");
                assert!(
                    error.error.is_branch_advanced(),
                    "Expected BranchAdvanced, got {error}"
                );
                assert!(
                    !error.tree_mutated(),
                    "a tip collision is caught before any write, so the handle stays usable"
                );
            }))
            .await
            .expect("Task failed");
    }

    /// The freeze is what removes a staged deletion: staging only tags, so a node
    /// deleted through the handle has to be gone from the published tree.
    #[tokio::test]
    async fn commit_in_memory_revision_drops_a_node_staged_for_deletion() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;
                let branch = branch_id();
                let state = State::new();
                add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    file("keep.bin"),
                    "keep.bin",
                )
                .await;
                add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    file("go.bin"),
                    "go.bin",
                )
                .await;

                let first = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    state.clone(),
                    metadata_on(branch),
                    Hash::default(),
                    branch,
                )
                .await
                .expect("the first revision must commit");

                let going = state
                    .find_node_link(repository.clone(), "go.bin")
                    .await
                    .expect("the file must resolve")
                    .node;
                state
                    .node_delete(repository.clone(), going)
                    .await
                    .expect("staging the deletion must succeed");

                let second = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    state,
                    metadata_on(branch),
                    first,
                    branch,
                )
                .await
                .expect("the deletion must commit");

                let published = State::deserialize(repository.clone(), second)
                    .await
                    .expect("the committed revision must deserialize");
                assert!(
                    published
                        .find_node_link(repository.clone(), "go.bin")
                        .await
                        .is_err(),
                    "a node staged for deletion must not survive the commit"
                );
                published
                    .find_node_link(repository, "keep.bin")
                    .await
                    .expect("its sibling must survive");
            }))
            .await
            .expect("Task failed");
    }

    #[tokio::test]
    async fn commit_in_memory_revision_chains_the_parent_and_numbers_the_revision() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;
                let branch = branch_id();
                let state = State::new();
                add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    file("a.bin"),
                    "a.bin",
                )
                .await;

                let first = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    state.clone(),
                    metadata_on(branch),
                    Hash::default(),
                    branch,
                )
                .await
                .expect("the first revision must commit");

                add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    file("b.bin"),
                    "b.bin",
                )
                .await;

                let second = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    state.clone(),
                    metadata_on(branch),
                    first,
                    branch,
                )
                .await
                .expect("the second revision must commit");

                assert_ne!(first, second, "each commit must produce its own revision");
                let published = State::deserialize(repository, second)
                    .await
                    .expect("the second revision must deserialize");
                assert_eq!(
                    published.parent_self(),
                    first,
                    "the second revision must record the first as its parent"
                );
                assert_eq!(
                    published.revision_number(),
                    2,
                    "the revision number must follow the parent's"
                );
            }))
            .await
            .expect("Task failed");
    }

    /// The handle stays usable: node ids captured before a commit still resolve and
    /// the state reports the revision it just published.
    #[tokio::test]
    async fn commit_in_memory_revision_leaves_the_state_on_the_new_revision() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;
                let branch = branch_id();
                let state = State::new();
                let node_id = add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    file("a.bin"),
                    "a.bin",
                )
                .await;

                let revision = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    state.clone(),
                    metadata_on(branch),
                    Hash::default(),
                    branch,
                )
                .await
                .expect("the revision must commit");

                assert_eq!(
                    state.revision(),
                    revision,
                    "the state must report the revision it published"
                );
                assert!(
                    !state.is_dirty(),
                    "a published state must not still be dirty"
                );
                let node = state
                    .node(repository, node_id)
                    .await
                    .expect("a node id captured before the commit must still resolve");
                assert_eq!(node.name_hash, hash_string("a.bin"));
            }))
            .await
            .expect("Task failed");
    }

    /// A failure past the freeze hands back a snapshot of the tree as it was, and the
    /// tree it describes is the staged one — still staged, still named, still there.
    /// A file with a zero content hash and a non-zero size gets past the validator,
    /// which checks no addresses, and is refused by the rehash inside the freeze.
    #[tokio::test]
    async fn commit_in_memory_revision_hands_back_a_snapshot_when_the_freeze_fails() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;
                let branch = branch_id();
                let state = State::new();
                let unhashed = Node {
                    address: Address::default(),
                    ..file("a.bin")
                };
                let node_id = add(&state, repository.clone(), ROOT_NODE, unhashed, "a.bin").await;

                let failure = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    state.clone(),
                    metadata_on(branch),
                    Hash::default(),
                    branch,
                )
                .await
                .expect_err("a zero content hash must not reach a revision");

                assert!(
                    failure.tree_mutated(),
                    "the rehash refuses inside the freeze, got {failure}"
                );
                assert!(
                    !failure.restore_from.is_zero(),
                    "a mutated tree must carry the snapshot to restore from"
                );

                let restored = State::deserialize(repository.clone(), failure.restore_from)
                    .await
                    .expect("the snapshot must read back");
                let node = restored
                    .node(repository.clone(), node_id)
                    .await
                    .expect("the staged node must survive in the snapshot");
                assert_eq!(node.name_hash, hash_string("a.bin"));
                assert!(
                    node.is_staged(),
                    "the snapshot is of the staged tree, so the edit is still staged: 0x{:x}",
                    node.flags
                );
                assert_eq!(
                    restored.parents()[0],
                    Hash::default(),
                    "the snapshot predates the commit, so it records no parent yet"
                );
            }))
            .await
            .expect("Task failed");
    }

    /// The same snapshot, taken on a state that has already published a revision — the
    /// case a handle is actually in when a second commit fails. The blocks were
    /// serialized once already, so the snapshot has to carry the edits made since.
    #[tokio::test]
    async fn commit_in_memory_revision_snapshots_a_state_that_already_committed() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;
                let branch = branch_id();
                let state = State::new();
                add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    file("a.bin"),
                    "a.bin",
                )
                .await;
                let published = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    state.clone(),
                    metadata_on(branch),
                    Hash::default(),
                    branch,
                )
                .await
                .expect("the first revision must commit");

                let unhashed = Node {
                    address: Address::default(),
                    ..file("b.bin")
                };
                let node_id = add(&state, repository.clone(), ROOT_NODE, unhashed, "b.bin").await;

                let failure = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    state.clone(),
                    metadata_on(branch),
                    published,
                    branch,
                )
                .await
                .expect_err("a zero content hash must not reach a revision");
                assert!(failure.tree_mutated(), "got {failure}");

                let restored = State::deserialize(repository.clone(), failure.restore_from)
                    .await
                    .expect("the snapshot must read back");
                let node = restored
                    .node(repository.clone(), node_id)
                    .await
                    .expect("the edit made since the last commit must be in the snapshot");
                assert_eq!(node.name_hash, hash_string("b.bin"));
                assert!(
                    node.is_staged(),
                    "the edit must still be staged in the snapshot: 0x{:x}",
                    node.flags
                );
                let name = restored
                    .node_name_clone(repository, node_id)
                    .await
                    .expect("the snapshot must carry the node's name");
                assert_eq!(name, "b.bin", "the name table must survive the snapshot");
            }))
            .await
            .expect("Task failed");
    }

    #[tokio::test]
    async fn commit_in_memory_revision_stamps_the_timestamp_and_author_when_unset() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;
                let branch = branch_id();
                let state = State::new();
                add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    file("a.bin"),
                    "a.bin",
                )
                .await;

                let revision = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    state,
                    metadata_on(branch),
                    Hash::default(),
                    branch,
                )
                .await
                .expect("the revision must commit");

                let published = State::deserialize(repository.clone(), revision)
                    .await
                    .expect("the revision must deserialize");
                let metadata = Metadata::deserialize(repository, published.metadata_hash())
                    .await
                    .expect("the revision metadata must deserialize");
                assert!(
                    metadata.get_timestamp().unwrap_or_default() > 0,
                    "a commit must record when it happened"
                );
                assert_eq!(
                    metadata.get_string(CREATED_BY).unwrap_or_default(),
                    "test-user",
                    "a commit must record who made it"
                );
            }))
            .await
            .expect("Task failed");
    }

    #[tokio::test]
    async fn commit_in_memory_revision_keeps_the_timestamp_and_author_the_caller_set() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;
                let branch = branch_id();
                let state = State::new();
                add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    file("a.bin"),
                    "a.bin",
                )
                .await;

                let mut supplied = metadata_on(branch);
                supplied
                    .set_timestamp(1_700_000_000)
                    .expect("setting the timestamp must succeed");
                supplied
                    .set_string(CREATED_BY, "importer")
                    .expect("setting the author must succeed");

                let revision = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    state,
                    supplied,
                    Hash::default(),
                    branch,
                )
                .await
                .expect("the revision must commit");

                let published = State::deserialize(repository.clone(), revision)
                    .await
                    .expect("the revision must deserialize");
                let metadata = Metadata::deserialize(repository, published.metadata_hash())
                    .await
                    .expect("the revision metadata must deserialize");
                assert_eq!(
                    metadata.get_timestamp().unwrap_or_default(),
                    1_700_000_000,
                    "a supplied timestamp must survive"
                );
                assert_eq!(
                    metadata.get_string(CREATED_BY).unwrap_or_default(),
                    "importer",
                    "a supplied author must survive"
                );
            }))
            .await
            .expect("Task failed");
    }

    /// The stamping rule is to supply what the caller did not, so a value stored
    /// under a kind the commit did not expect is still the caller's choice. Reading
    /// the key as a string would report it absent and replace it.
    #[tokio::test]
    async fn commit_in_memory_revision_keeps_an_author_the_caller_stored_as_another_kind() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;
                let branch = branch_id();
                let state = State::new();
                add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    file("a.bin"),
                    "a.bin",
                )
                .await;

                let mut supplied = metadata_on(branch);
                supplied
                    .set_u64(CREATED_BY, 4242)
                    .expect("storing the author as a number must succeed");

                let revision = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    state,
                    supplied,
                    Hash::default(),
                    branch,
                )
                .await
                .expect("the revision must commit");

                let published = State::deserialize(repository.clone(), revision)
                    .await
                    .expect("the revision must deserialize");
                let metadata = Metadata::deserialize(repository, published.metadata_hash())
                    .await
                    .expect("the revision metadata must deserialize");
                assert_eq!(
                    metadata.get_u64(CREATED_BY).ok(),
                    Some(4242),
                    "a non-string author must survive as the kind the caller stored"
                );
            }))
            .await
            .expect("Task failed");
    }

    #[tokio::test]
    async fn resolve_commit_branch_takes_the_parent_branch_when_the_key_is_unset() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;
                let branch = branch_id();
                let state = State::new();
                add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    file("a.bin"),
                    "a.bin",
                )
                .await;
                let first = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    state.clone(),
                    metadata_on(branch),
                    Hash::default(),
                    branch,
                )
                .await
                .expect("the first revision must commit");

                assert_eq!(
                    resolve_commit_branch(repository, state, &Metadata::new(), first)
                        .await
                        .expect("an unset branch must resolve"),
                    branch
                );
            }))
            .await
            .expect("Task failed");
    }

    #[tokio::test]
    async fn resolve_commit_branch_rejects_an_initial_revision_with_no_branch() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;

                let error = resolve_commit_branch(
                    repository,
                    State::new(),
                    &Metadata::new(),
                    Hash::default(),
                )
                .await
                .expect_err("an initial revision has no parent to derive a branch from");
                assert!(
                    error.is_invalid_arguments(),
                    "Expected InvalidArguments, got {error}"
                );
            }))
            .await
            .expect("Task failed");
    }

    /// A revision written before branch metadata existed, or by something that never
    /// recorded it, gives nothing to continue from — and guessing would publish onto
    /// a branch the caller never named. A state carrying no metadata reports a zero
    /// branch, which is what such a parent looks like to the resolver.
    #[tokio::test]
    async fn resolve_commit_branch_rejects_a_parent_that_records_no_branch() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;

                let error = resolve_commit_branch(
                    repository,
                    State::new(),
                    &Metadata::new(),
                    Hash::from_u64(0x99),
                )
                .await
                .expect_err("a parent with no branch must not be guessed at");
                assert!(
                    error.is_invalid_arguments(),
                    "Expected InvalidArguments, got {error}"
                );
                assert!(
                    error.to_string().contains("records no branch"),
                    "the reason must name what is missing, got {error}"
                );
            }))
            .await
            .expect("Task failed");
    }

    #[tokio::test]
    async fn resolve_commit_branch_accepts_a_branch_that_branches_from_this_revision() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;
                let parent_branch = branch_id();
                let state = State::new();
                add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    file("a.bin"),
                    "a.bin",
                )
                .await;
                let first = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    state.clone(),
                    metadata_on(parent_branch),
                    Hash::default(),
                    parent_branch,
                )
                .await
                .expect("the first revision must commit");

                let child_branch = branch_id();
                branch::create(
                    repository.clone(),
                    &token(),
                    child_branch,
                    "child",
                    branch::default_category(),
                    "test-user",
                    1,
                    vec![BranchPoint {
                        branch: parent_branch,
                        revision: first,
                    }],
                    false,
                    false,
                )
                .await
                .expect("creating the child branch must succeed");

                let mut metadata = Metadata::new();
                metadata
                    .set_branch(child_branch)
                    .expect("setting the branch must succeed");
                assert_eq!(
                    resolve_commit_branch(repository, state, &metadata, first)
                        .await
                        .expect("the first revision on a branch created here must resolve"),
                    child_branch
                );
            }))
            .await
            .expect("Task failed");
    }

    #[tokio::test]
    async fn resolve_commit_branch_rejects_a_branch_that_does_not_branch_from_this_revision() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;
                let parent_branch = branch_id();
                let state = State::new();
                add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    file("a.bin"),
                    "a.bin",
                )
                .await;
                let first = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    state.clone(),
                    metadata_on(parent_branch),
                    Hash::default(),
                    parent_branch,
                )
                .await
                .expect("the first revision must commit");

                let elsewhere = branch_id();
                branch::create(
                    repository.clone(),
                    &token(),
                    elsewhere,
                    "elsewhere",
                    branch::default_category(),
                    "test-user",
                    1,
                    vec![BranchPoint {
                        branch: parent_branch,
                        revision: Hash::from_u64(0xdead),
                    }],
                    false,
                    false,
                )
                .await
                .expect("creating the other branch must succeed");

                let mut metadata = Metadata::new();
                metadata
                    .set_branch(elsewhere)
                    .expect("setting the branch must succeed");
                let error = resolve_commit_branch(repository, state, &metadata, first)
                    .await
                    .expect_err("a branch rooted elsewhere must not accept this revision");
                assert!(
                    error.is_invalid_arguments(),
                    "Expected InvalidArguments, got {error}"
                );
            }))
            .await
            .expect("Task failed");
    }

    /// A memory-based handle has no working tree, so a conflict on any kind of node has no file to
    /// resolve and no markers to read. The freeze clears the flags of everything it walks, so an
    /// unresolved one has to be refused rather than published as settled.
    #[tokio::test]
    async fn commit_in_memory_revision_rejects_an_unresolved_conflict() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;

                let linked = Node {
                    flags: NodeFlags::Link.bits(),
                    mode: 0o755,
                    name_hash: hash_string("node"),
                    child: 7,
                    address: Address {
                        hash: Hash::from_u64(0xabcd),
                        context: Context::from(uuid::Uuid::now_v7()),
                    },
                    ..Default::default()
                };

                for conflicted in [file("node"), directory("node"), linked] {
                    let branch = branch_id();
                    let state = State::new();
                    let node_id =
                        add(&state, repository.clone(), ROOT_NODE, conflicted, "node").await;
                    state
                        .node_mark_staged(
                            repository.clone(),
                            node_id,
                            NodeFlags::StagedMergeConflict,
                            NodeFlags::NoFlags,
                        )
                        .await
                        .expect("marking the conflict must succeed");

                    let failure = commit_in_memory_revision(
                        repository.clone(),
                        &token(),
                        state,
                        metadata_on(branch),
                        Hash::default(),
                        branch,
                    )
                    .await
                    .expect_err("an unresolved conflict must not reach a revision");

                    assert!(
                        failure.error.is_conflict(),
                        "Expected Conflict, got {failure}"
                    );
                    assert!(
                        branch::load_latest(repository.clone(), branch)
                            .await
                            .unwrap_or_default()
                            .is_zero(),
                        "the branch must hold no revision"
                    );
                }
            }))
            .await
            .expect("Task failed");
    }

    /// Stages `path`'s file as modified to new content.
    async fn modify(state: &State, repository: Arc<RepositoryContext>, path: &str, content: u64) {
        let node_id = state
            .find_node_link(repository.clone(), path)
            .await
            .expect("the file must resolve")
            .node;
        state
            .node_modify(
                repository.clone(),
                node_id,
                0o644,
                20,
                Address {
                    hash: Hash::from_u64(content),
                    context: Context::default(),
                },
            )
            .await
            .expect("modifying the file must succeed");
        state
            .node_mark_staged(
                repository,
                node_id,
                NodeFlags::StagedModify,
                NodeFlags::DirtyModify,
            )
            .await
            .expect("marking the modification must succeed");
    }

    /// The server's fast-forward merge: the pushed revision's changes applied onto a moved
    /// head, then committed. The merge must record what it merged in, marked as merged, and
    /// none of the head's own changes, and must carry no change flags.
    #[tokio::test]
    async fn freeze_and_serialize_records_a_server_merge_as_merged_in() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;
                let main = branch_id();
                let base = State::new();
                add(&base, repository.clone(), ROOT_NODE, file("a.bin"), "a.bin").await;
                add(&base, repository.clone(), ROOT_NODE, file("b.bin"), "b.bin").await;
                let r1 = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    base,
                    metadata_on(main),
                    Hash::default(),
                    main,
                )
                .await
                .expect("committing the base must succeed");

                // The pushed side, on its own branch: modify a.bin, add c.bin
                let feature = branch_id();
                let pushed = State::deserialize(repository.clone(), r1)
                    .await
                    .expect("the base must deserialize");
                modify(&pushed, repository.clone(), "a.bin", 0x61).await;
                add(
                    &pushed,
                    repository.clone(),
                    ROOT_NODE,
                    file("c.bin"),
                    "c.bin",
                )
                .await;
                let incoming = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    pushed,
                    metadata_on(feature),
                    r1,
                    feature,
                )
                .await
                .expect("committing the pushed side must succeed");

                // The moved head: modify b.bin
                let moved = State::deserialize(repository.clone(), r1)
                    .await
                    .expect("the base must deserialize");
                modify(&moved, repository.clone(), "b.bin", 0x62).await;
                let head = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    moved,
                    metadata_on(main),
                    r1,
                    main,
                )
                .await
                .expect("committing the moved head must succeed");

                // What `try_fast_forward_merge` does
                let diff = lore_revision::revision::diff3_collect(
                    repository.clone(),
                    r1,
                    incoming,
                    head,
                    None,
                    false,
                )
                .await
                .expect("the diff3 must succeed");
                assert!(diff.conflicts.is_empty(), "the case needs a clean merge");
                let merge = State::deserialize(repository.clone(), head)
                    .await
                    .expect("the head must deserialize");
                lore_revision::state::apply_tree_changes(
                    repository.clone(),
                    merge.clone(),
                    &diff.changes,
                )
                .await
                .expect("applying the changes must succeed");
                merge.set_parent_self(head);
                merge.set_parent_other(incoming);

                let signature = freeze_and_serialize(repository.clone(), merge, &token())
                    .await
                    .expect("committing the merge must succeed");

                let committed = State::deserialize(repository.clone(), signature)
                    .await
                    .expect("the merge must deserialize");
                for (path, action) in [("a.bin", FileAction::Keep), ("c.bin", FileAction::Add)] {
                    let node_id = committed
                        .find_node_link(repository.clone(), path)
                        .await
                        .expect("the merge must hold the file")
                        .node;
                    let delta = committed
                        .node_delta(repository.clone(), node_id)
                        .await
                        .expect("the delta must read back")
                        .unwrap_or_else(|| panic!("{path} must be in the merge's delta"));
                    assert_eq!(delta.action, action as u16, "{path}'s action");
                    assert_ne!(
                        delta.flags & change::Flags::Merge.bits(),
                        0,
                        "{path} must be recorded as merged in"
                    );
                    let node = committed
                        .node(repository.clone(), node_id)
                        .await
                        .expect("the node must read back");
                    assert!(
                        !node.is_dirty_or_staged(),
                        "{path} must carry no change flags, got 0x{:x}",
                        node.flags
                    );
                }

                let head_only = committed
                    .find_node_link(repository.clone(), "b.bin")
                    .await
                    .expect("the merge must hold the head's file")
                    .node;
                assert!(
                    committed
                        .node_delta(repository.clone(), head_only)
                        .await
                        .expect("the delta must read back")
                        .is_none(),
                    "the head's own change must not be in the merge's delta"
                );
            }))
            .await
            .expect("Task failed");
    }
}
