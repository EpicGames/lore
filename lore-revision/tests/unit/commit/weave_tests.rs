// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_revision::commit::BasedOn;
use lore_revision::commit::weave_history;
use lore_revision::node::*;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::Content;
use lore_revision::state::Mark;
use lore_revision::state::Rewrite;
use lore_revision::state::State;

use crate::helper::in_memory;
use crate::helper::in_memory::add;
use crate::helper::in_memory::branch_id;
use crate::helper::in_memory::commit;
use crate::helper::in_memory::directory;
use crate::helper::in_memory::file;
use crate::helper::in_memory::revision_state;
use crate::helper::test_store_create;

/// The revision and the node in it that the first-parent history of `node_id` names.
async fn first_parent_history(
    repository: &Arc<RepositoryContext>,
    state: &State,
    node_id: NodeID,
) -> (Hash, NodeID) {
    let metadata_node = node_to_file_metadata(node_id);
    let block = state
        .block_file_metadata(
            repository.clone(),
            NodeFileMetadataBlock::index(metadata_node),
        )
        .await
        .expect("reading the file metadata block");
    let history = block.node(NodeFileMetadata::index(metadata_node));
    (history.revision[0], history.node[0])
}

/// `merged`, woven again as the merge a sync stages for divergent history: the remote latest
/// first, and the local latest, which the tree of `merged` was built from, second.
async fn weave_as_divergent_merge(
    repository: &Arc<RepositoryContext>,
    merged: Hash,
    remote_latest: Hash,
    local_latest: Hash,
) -> Arc<State> {
    let state = revision_state(repository, merged).await;
    state.set_parent_self(remote_latest);
    state.set_parent_other(local_latest);
    weave_history(repository.clone(), state.clone(), BasedOn::Other)
        .await
        .expect("weaving the merge history");
    state
}

/// Both sides add a file after they diverge, and each takes the same node ID. The remote's add
/// is recorded on the file it added, under the node ID the remote latest has it at, and not on
/// the local file that has that node ID in the merge.
#[tokio::test]
async fn weaving_a_merge_of_divergent_history_records_the_remote_history_on_its_file() {
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let repository = in_memory::repository(immutable, mutable);
        let main = branch_id();

        let state = State::new();
        add(
            &state,
            repository.clone(),
            ROOT_NODE,
            file("base.txt"),
            "base.txt",
        )
        .await;
        let base = commit(&repository, &state, Hash::default(), main).await;

        let local = revision_state(&repository, base).await;
        let local_file = add(
            &local,
            repository.clone(),
            ROOT_NODE,
            file("local.txt"),
            "local.txt",
        )
        .await;
        let local_latest = commit(&repository, &local, base, main).await;

        let remote_file = file("remote.txt");
        let remote = revision_state(&repository, base).await;
        let remote_file_in_remote = add(
            &remote,
            repository.clone(),
            ROOT_NODE,
            remote_file,
            "remote.txt",
        )
        .await;
        let remote_latest = commit(&repository, &remote, base, branch_id()).await;
        assert_eq!(remote_file_in_remote, local_file);

        let merged = revision_state(&repository, local_latest).await;
        let remote_file_in_merge = add(
            &merged,
            repository.clone(),
            ROOT_NODE,
            remote_file,
            "remote.txt",
        )
        .await;
        let merged = commit(&repository, &merged, local_latest, main).await;

        let state =
            weave_as_divergent_merge(&repository, merged, remote_latest, local_latest).await;

        assert_eq!(
            first_parent_history(&repository, &state, remote_file_in_merge).await,
            (remote_latest, remote_file_in_remote)
        );
        assert_ne!(
            first_parent_history(&repository, &state, local_file)
                .await
                .0,
            remote_latest
        );
    }))
    .await
    .expect("Task failed");
}

/// A file the local side renamed keeps its node ID and its file ID, so the remote's change to it
/// is recorded on it although no file of the merge is at the path the remote has it at.
#[tokio::test]
async fn weaving_a_merge_of_divergent_history_records_the_remote_history_on_a_renamed_file() {
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let repository = in_memory::repository(immutable, mutable);
        let main = branch_id();

        let state = State::new();
        let renamed = add(
            &state,
            repository.clone(),
            ROOT_NODE,
            file("a.txt"),
            "a.txt",
        )
        .await;
        let base = commit(&repository, &state, Hash::default(), main).await;

        let remote = revision_state(&repository, base).await;
        let changed = Address {
            hash: Hash::from_u64(0x5b),
            context: Context::default(),
        };
        remote
            .node_rewrite(
                &repository,
                renamed,
                &Content {
                    mode: 0o644,
                    size: 11,
                    address: changed,
                },
                Rewrite::Plain,
            )
            .await
            .expect("modifying the file");
        remote
            .node_mark(
                &repository,
                renamed,
                Mark::staged_and_dirty(NodeFlags::StagedModify, NodeFlags::DirtyModify),
            )
            .await
            .expect("staging the modification");
        let remote_latest = commit(&repository, &remote, base, branch_id()).await;

        let local = revision_state(&repository, base).await;
        local
            .node_move_staged(
                repository.clone(),
                renamed,
                ROOT_NODE,
                ("b.txt", lore_storage::hash::hash_string("b.txt")),
                &local.moves_permit().await.expect("the moves permit"),
                false,
            )
            .await
            .expect("renaming the file");
        let local_latest = commit(&repository, &local, base, main).await;

        let merged = revision_state(&repository, local_latest).await;
        add(
            &merged,
            repository.clone(),
            ROOT_NODE,
            file("merged.txt"),
            "merged.txt",
        )
        .await;
        let merged = commit(&repository, &merged, local_latest, main).await;

        let state =
            weave_as_divergent_merge(&repository, merged, remote_latest, local_latest).await;

        assert_eq!(
            first_parent_history(&repository, &state, renamed).await,
            (remote_latest, renamed)
        );
    }))
    .await
    .expect("Task failed");
}

/// The remote latest numbers a file past every node block the merge holds. The file is found by
/// its path, and no block past the end of the merge is read.
#[tokio::test]
async fn weaving_a_merge_of_divergent_history_finds_a_file_numbered_past_the_merge_by_path() {
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let repository = in_memory::repository(immutable, mutable);
        let main = branch_id();

        let state = State::new();
        add(
            &state,
            repository.clone(),
            ROOT_NODE,
            file("base.txt"),
            "base.txt",
        )
        .await;
        let base = commit(&repository, &state, Hash::default(), main).await;

        let remote = revision_state(&repository, base).await;
        for index in 0..BLOCK_NODE_COUNT {
            let name = format!("remote-{index}.txt");
            add(&remote, repository.clone(), ROOT_NODE, file(&name), &name).await;
        }
        let last_file = file("last.txt");
        let last_in_remote = add(
            &remote,
            repository.clone(),
            ROOT_NODE,
            last_file,
            "last.txt",
        )
        .await;
        let remote_latest = commit(&repository, &remote, base, branch_id()).await;

        let local = revision_state(&repository, base).await;
        add(
            &local,
            repository.clone(),
            ROOT_NODE,
            file("local.txt"),
            "local.txt",
        )
        .await;
        let local_latest = commit(&repository, &local, base, main).await;

        let merged = revision_state(&repository, local_latest).await;
        let last_in_merge = add(
            &merged,
            repository.clone(),
            ROOT_NODE,
            last_file,
            "last.txt",
        )
        .await;
        let merged = commit(&repository, &merged, local_latest, main).await;

        let state =
            weave_as_divergent_merge(&repository, merged, remote_latest, local_latest).await;
        assert!(NodeBlock::index(last_in_remote) >= state.block_count());

        assert_eq!(
            first_parent_history(&repository, &state, last_in_merge).await,
            (remote_latest, last_in_remote)
        );
    }))
    .await
    .expect("Task failed");
}

/// A revision committed on its first parent shares that parent's node IDs, so a directory it
/// renames keeps the history of the change its parent made under it.
#[tokio::test]
async fn weaving_a_revision_on_its_first_parent_records_history_on_a_renamed_directory() {
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let repository = in_memory::repository(immutable, mutable);
        let main = branch_id();

        let state = State::new();
        let renamed = add(&state, repository.clone(), ROOT_NODE, directory("d"), "d").await;
        add(&state, repository.clone(), renamed, file("f.txt"), "f.txt").await;
        let first = commit(&repository, &state, Hash::default(), main).await;

        add(&state, repository.clone(), renamed, file("g.txt"), "g.txt").await;
        let second = commit(&repository, &state, first, main).await;

        state
            .node_move_staged(
                repository.clone(),
                renamed,
                ROOT_NODE,
                ("e", lore_storage::hash::hash_string("e")),
                &state.moves_permit().await.expect("the moves permit"),
                false,
            )
            .await
            .expect("renaming the directory");
        let third = commit(&repository, &state, second, main).await;

        let published = revision_state(&repository, third).await;
        assert_eq!(
            first_parent_history(&repository, &published, renamed).await,
            (second, renamed)
        );
    }))
    .await
    .expect("Task failed");
}

/// The remote changes a file in a revision before its latest, and the merge takes the change. The
/// merge's first-parent history of the file names that revision, as the remote latest's does.
#[tokio::test]
async fn weaving_a_merge_of_divergent_history_carries_the_remote_history_before_its_latest() {
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let repository = in_memory::repository(immutable, mutable);
        let main = branch_id();
        let remote_branch = branch_id();

        let state = State::new();
        let original = file("x.txt");
        let changed_file = add(&state, repository.clone(), ROOT_NODE, original, "x.txt").await;
        let base = commit(&repository, &state, Hash::default(), main).await;
        let changed_address = Address {
            hash: Hash::from_u64(0x5b),
            context: original.address.context,
        };

        let remote = revision_state(&repository, base).await;
        modify(
            &remote,
            &repository,
            changed_file,
            changed_address,
            NodeFlags::StagedModify,
        )
        .await;
        let changed = commit(&repository, &remote, base, remote_branch).await;
        let remote = revision_state(&repository, changed).await;
        add(
            &remote,
            repository.clone(),
            ROOT_NODE,
            file("y.txt"),
            "y.txt",
        )
        .await;
        let remote_latest = commit(&repository, &remote, changed, remote_branch).await;
        let remote_latest_state = revision_state(&repository, remote_latest).await;
        assert_eq!(
            first_parent_history(&repository, &remote_latest_state, changed_file).await,
            (changed, changed_file)
        );

        let local = revision_state(&repository, base).await;
        add(
            &local,
            repository.clone(),
            ROOT_NODE,
            file("z.txt"),
            "z.txt",
        )
        .await;
        let local_latest = commit(&repository, &local, base, main).await;

        let merged = revision_state(&repository, local_latest).await;
        modify(
            &merged,
            &repository,
            changed_file,
            changed_address,
            NodeFlags::StagedModify | NodeFlags::StagedMerge,
        )
        .await;
        let merged = commit(&repository, &merged, local_latest, main).await;

        let state =
            weave_as_divergent_merge(&repository, merged, remote_latest, local_latest).await;

        assert_eq!(
            first_parent_history(&repository, &state, changed_file).await,
            (changed, changed_file)
        );
    }))
    .await
    .expect("Task failed");
}

/// Sets the content of file `node_id` of `state` to `address`, staged with `staged`.
async fn modify(
    state: &State,
    repository: &Arc<RepositoryContext>,
    node_id: NodeID,
    address: Address,
    staged: NodeFlags,
) {
    state
        .node_rewrite(
            repository,
            node_id,
            &Content {
                mode: 0o644,
                size: 11,
                address,
            },
            Rewrite::Plain,
        )
        .await
        .expect("modifying the file");
    state
        .node_mark(
            repository,
            node_id,
            Mark::staged_and_dirty(staged, NodeFlags::DirtyModify),
        )
        .await
        .expect("staging the modification");
}
