// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![allow(clippy::disallowed_methods)] // The tests delete files from the working tree directly.

//! `lore branch merge` of a file one branch deletes and the other changes: what the merge stages,
//! what a resolution and committing make of it, and what an abort leaves; and the changes a
//! revision's delta records, from its first parent.

use std::collections::BTreeMap;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_base::types::TypedBytes as _;
use lore_revision::branch;
use lore_revision::branch::BranchLatestStatus;
use lore_revision::change;
use lore_revision::change::FileAction;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreString;
use lore_revision::node::Node;
use lore_revision::node::NodeDelta;
use lore_revision::node::NodeFileMetadata;
use lore_revision::node::NodeFileMetadataBlock;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::node_to_file_metadata;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository::status::StatusOptions;
use lore_revision::repository::status::status_boxed;
use lore_revision::state::State;

use super::instance::Edit;
use super::instance::Instance;
use super::instance::commit;
use super::instance::commit_edits;
use super::instance::create_branch;
use super::instance::current;
use super::instance::named;
use super::instance::node_at;
use super::instance::revision_state;
use super::instance::run;
use super::instance::stage;
use super::instance::start_merge;
use super::instance::switch_to;
use super::instance::sync_to;
use super::instance::working_tree;
use super::instance::write;
use crate::helper::test_filter;
use crate::helper::test_staged_by_intent;

/// The files every fixture starts from, each holding its own path.
const TREE: [&str; 2] = ["Dir/x.txt", "Top.txt"];

/// The working tree of the fixtures once `Dir/x.txt` is gone.
fn without_x() -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([("Top.txt".to_string(), b"Top.txt".to_vec())])
}

/// Commits an edit of `Dir/x.txt` on a feature branch and `here` on main, both made on the first
/// revision, and merges the feature branch into main without committing.
async fn merge_over(instance: &Instance, here: Edit) {
    create_branch(instance, "feature").await;
    commit_edits(instance, &[Edit::Write("Dir/x.txt", "feature")], "Feature").await;
    switch_to(instance, branch::DEFAULT_DEFAULT_NAME).await;
    commit_edits(instance, &[here], "Main").await;
    start_merge(instance, "feature", true)
        .await
        .expect("Failed to merge");
}

/// [`merge_over`] the deletion of `Dir/x.txt` on main.
async fn merge_deleted_here(instance: &Instance) {
    merge_over(instance, Edit::Delete("Dir/x.txt")).await;
}

/// Resolves the conflict at `path`, taking our side where `mine` asks, and otherwise marking it
/// resolved as the working tree holds it.
async fn resolve(instance: &Instance, path: &str, mine: bool) {
    let paths = named(instance, path);
    if mine {
        branch::merge::merge_resolve_mine(
            instance.repository.clone(),
            &instance.write_token,
            paths,
        )
        .await
        .expect("Failed to resolve taking our side");
    } else {
        branch::merge::branch_merge_resolve(
            instance.repository.clone(),
            &instance.write_token,
            paths,
        )
        .await
        .expect("Failed to resolve");
    }
}

/// The node the staged revision holds at `path`.
async fn staged_node(instance: &Instance, path: &str) -> Node {
    let revision = lore_revision::instance::load_staged_revision(&instance.repository)
        .await
        .expect("Failed to load the staged revision")
        .expect("A merge left a staged revision");
    let state = revision_state(instance, revision).await;
    node_in(instance, &state, path).await
}

/// The node `state` holds at `path`.
async fn node_in(instance: &Instance, state: &State, path: &str) -> Node {
    let node = state
        .find_node_link(instance.repository.clone(), path)
        .await
        .expect("Failed to find the path")
        .node;
    state
        .node(instance.repository.clone(), node)
        .await
        .expect("Failed to read the node")
}

/// Each entry the delta of `revision` records, as its action, path, flags and whether its node is a
/// file: a deleted node's path read from the first parent, the tree that holds it.
pub(super) async fn delta_entries(
    instance: &Instance,
    revision: Hash,
) -> Vec<(u16, String, u16, bool)> {
    let state = revision_state(instance, revision).await;
    let parent = revision_state(instance, state.parent_self()).await;
    let delta = state
        .delta_block(instance.repository.clone())
        .await
        .expect("Failed to read the delta")
        .to_aligned::<NodeDelta>();
    let mut entries = Vec::new();
    for entry in delta.as_type_slice::<NodeDelta>() {
        let tree = if entry.action == FileAction::Delete as u16 {
            &parent
        } else {
            &state
        };
        let node = tree
            .node(instance.repository.clone(), entry.node)
            .await
            .expect("Failed to read a delta node");
        let path = tree
            .node_path(instance.repository.clone(), entry.node)
            .await
            .expect("Failed to read a delta path");
        entries.push((entry.action, path, entry.flags, node.is_file()));
    }
    entries
}

/// The adds, deletes and moves the delta of the current revision records, as the action and the
/// path ([`delta_entries`]).
async fn delta(instance: &Instance) -> Vec<(u16, String)> {
    delta_entries(instance, current(instance).await)
        .await
        .into_iter()
        .filter(|(action, ..)| *action != FileAction::Keep as u16)
        .map(|(action, path, ..)| (action, path))
        .collect()
}

/// The paths of the deletions the [`delta`] of the current revision records.
async fn deletions(instance: &Instance) -> Vec<String> {
    delta(instance)
        .await
        .into_iter()
        .filter(|(action, _)| *action == FileAction::Delete as u16)
        .map(|(_, path)| path)
        .collect()
}

/// Merges `latest`, the branch latest a remote answers, into `local`, the current revision and the
/// branch's local latest, without committing, as a sync of diverged history does.
async fn merge_diverged(instance: &Instance, local: Hash, latest: Hash) {
    let main = |previous, latest| {
        branch::store_latest(
            instance.repository.clone(),
            instance.main,
            previous,
            latest,
            BranchLatestStatus::Divergent,
        )
    };
    main(local, latest)
        .await
        .expect("Failed to move the branch");
    start_merge(instance, branch::DEFAULT_DEFAULT_NAME, true)
        .await
        .expect("Failed to merge");
    main(latest, local)
        .await
        .expect("Failed to move the branch back");
}

/// Runs `command` under the offline global arguments `flags` sets, as a command given those flags
/// runs.
async fn with_flags<T>(
    flags: impl FnOnce(&mut LoreGlobalArgs),
    command: impl Future<Output = T>,
) -> T {
    let mut globals = LoreGlobalArgs::default().set_offline();
    flags(&mut globals);
    let execution = Arc::new(ExecutionContext::new_client(
        globals,
        EventDispatcher::no_dispatch(),
    ));
    execution.set_user_id("operator@example.com").await;
    LORE_CONTEXT.scope(execution, command).await
}

/// The revision and the node in it that the first-parent history of the file at `path` names, in
/// the tree of `revision`.
async fn first_parent_history(instance: &Instance, revision: Hash, path: &str) -> (Hash, NodeID) {
    let state = revision_state(instance, revision).await;
    let node = state
        .find_node_link(instance.repository.clone(), path)
        .await
        .expect("Failed to find the file")
        .node;
    let metadata_node = node_to_file_metadata(node);
    let block = state
        .block_file_metadata(
            instance.repository.clone(),
            NodeFileMetadataBlock::index(metadata_node),
        )
        .await
        .expect("Failed to read the file metadata");
    let history = block.node(NodeFileMetadata::index(metadata_node));
    (history.revision[0], history.node[0])
}

/// A file this branch deletes and the incoming branch changes is staged deleted in conflict, an
/// add taken back, with the incoming copy written beside the path; the commit refuses it.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_deleted_here_and_changed_there_is_staged_deleted_in_conflict() {
    run(
        "a_file_deleted_here_and_changed_there_is_staged_deleted_in_conflict",
        &TREE,
        |instance| async move {
            merge_deleted_here(&instance).await;

            let node = staged_node(&instance, "Dir/x.txt").await;
            assert!(node.is_staged_delete(), "flags {:#x}", node.flags);
            assert!(node.is_staged_add(), "flags {:#x}", node.flags);
            assert!(node.is_staged_merge_unresolved(), "flags {:#x}", node.flags);
            let mut expected = without_x();
            expected.insert("Dir/x.txt~theirs".to_string(), b"feature".to_vec());
            assert_eq!(working_tree(&instance.path), expected);

            let refused = commit(&instance, "Merge")
                .await
                .expect_err("The commit refuses an unresolved conflict");
            assert!(refused.is_conflict(), "{refused}");
        },
    )
    .await;
}

/// Resolving the conflict of a file this branch deletes, taking our side or marking it resolved
/// with no file there, commits the deletion and removes the incoming copy. The merge revision
/// records no change at the path, which neither parent it descends from first holds.
#[tokio::test(flavor = "multi_thread")]
async fn a_resolved_deletion_commits_without_the_copies() {
    for mine in [true, false] {
        run(
            "a_resolved_deletion_commits_without_the_copies",
            &TREE,
            move |instance| async move {
                merge_deleted_here(&instance).await;
                resolve(&instance, "Dir/x.txt", mine).await;

                commit(&instance, "Merge")
                    .await
                    .expect("The resolved merge commits");
                assert_eq!(working_tree(&instance.path), without_x(), "mine {mine}");
                assert_eq!(delta(&instance).await, [], "mine {mine}");
            },
        )
        .await;
    }
}

/// A merge resolving a file this branch deletes, alone or with its directory, to our side or as
/// the deletion it is, records in its delta the deletion of a file the incoming branch deletes,
/// which the self parent holds, and no deletion of a node the merge staged that the self parent
/// does not hold, at the path or at another, such as the untouched `Dir/b.txt`. It leaves no
/// directory the self parent lacks.
#[tokio::test(flavor = "multi_thread")]
async fn a_merge_records_only_the_deletions_its_self_parent_holds() {
    const TREE: [&str; 3] = ["Dir/b.txt", "Dir/x.txt", "Top.txt"];
    for (deleted, mine) in [
        ("Dir/x.txt", true),
        ("Dir", true),
        ("Dir/x.txt", false),
        ("Dir", false),
    ] {
        run(
            "a_merge_records_only_the_deletions_its_self_parent_holds",
            &TREE,
            move |instance| async move {
                create_branch(&instance, "feature").await;
                commit_edits(
                    &instance,
                    &[Edit::Write("Dir/x.txt", "feature"), Edit::Delete("Top.txt")],
                    "Feature",
                )
                .await;
                switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
                commit_edits(&instance, &[Edit::Delete(deleted)], "Main").await;
                start_merge(&instance, "feature", true)
                    .await
                    .expect("Failed to merge");
                resolve(&instance, "Dir/x.txt", mine).await;

                commit(&instance, "Merge")
                    .await
                    .expect("The resolved merge commits");
                let context = format!("{deleted} deleted, resolved to mine {mine}");
                assert_eq!(
                    delta(&instance).await,
                    [(FileAction::Delete as u16, "Top.txt".to_string())],
                    "{context}"
                );
                let kept = if deleted == "Dir" {
                    BTreeMap::new()
                } else {
                    BTreeMap::from([("Dir/b.txt".to_string(), b"Dir/b.txt".to_vec())])
                };
                assert_eq!(working_tree(&instance.path), kept, "{context}");
                assert_eq!(
                    instance.path.join("Dir").exists(),
                    deleted != "Dir",
                    "{context}"
                );
            },
        )
        .await;
    }
}

/// A commit of the deletion of a committed directory records the deletion of every node in it,
/// each held by the self parent, the directories below it included.
#[tokio::test(flavor = "multi_thread")]
async fn a_committed_subtree_deletion_records_every_node() {
    const TREE: [&str; 3] = ["Dir/Sub/y.txt", "Dir/x.txt", "Top.txt"];
    run(
        "a_committed_subtree_deletion_records_every_node",
        &TREE,
        |instance| async move {
            commit_edits(&instance, &[Edit::Delete("Dir")], "Delete").await;
            let mut recorded = delta(&instance).await;
            recorded.sort_unstable();
            let deleted = FileAction::Delete as u16;
            assert_eq!(
                recorded,
                ["Dir", "Dir/Sub", "Dir/Sub/y.txt", "Dir/x.txt"]
                    .map(|path| (deleted, path.to_string()))
            );
        },
    )
    .await;
}

/// A file this branch deletes, which the working tree holds again when its conflict is marked
/// resolved, is staged from the working tree as the add it is, and committed with its content.
#[tokio::test(flavor = "multi_thread")]
async fn a_resolved_deletion_the_working_tree_holds_commits_the_file() {
    run(
        "a_resolved_deletion_the_working_tree_holds_commits_the_file",
        &TREE,
        |instance| async move {
            merge_deleted_here(&instance).await;
            write(&instance, "Dir/x.txt", "recreated");
            branch::merge::branch_merge_resolve(
                instance.repository.clone(),
                &instance.write_token,
                named(&instance, "Dir/x.txt"),
            )
            .await
            .expect("Failed to resolve");

            let node = staged_node(&instance, "Dir/x.txt").await;
            assert!(!node.is_staged_delete(), "flags {:#x}", node.flags);
            assert!(node.is_staged_add(), "flags {:#x}", node.flags);
            assert!(node.is_staged_merge_resolved(), "flags {:#x}", node.flags);

            commit(&instance, "Merge")
                .await
                .expect("The resolved merge commits");
            let mut expected = without_x();
            expected.insert("Dir/x.txt".to_string(), b"recreated".to_vec());
            assert_eq!(working_tree(&instance.path), expected);
            assert_eq!(
                delta(&instance).await,
                [(FileAction::Add as u16, "Dir/x.txt".to_string())]
            );
            let merged = revision_state(&instance, current(&instance).await).await;
            let node = merged
                .find_node_link(instance.repository.clone(), "Dir/x.txt")
                .await
                .expect("The merge revision holds the file")
                .node;
            let node = merged
                .node(instance.repository.clone(), node)
                .await
                .expect("Failed to read the node");
            assert_eq!(node.size, b"recreated".len() as u64);
        },
    )
    .await;
}

/// A file this branch deletes, resolved taking our side and then resolved again, as the file the
/// working tree holds once more or taking their side, stays an add taken back and then added: the
/// staged node carries the add alone, never a modify of a node the self parent lacks.
#[tokio::test(flavor = "multi_thread")]
async fn a_deletion_resolved_to_our_side_and_then_again_stays_an_add() {
    for theirs in [false, true] {
        run(
            "a_deletion_resolved_to_our_side_and_then_again_stays_an_add",
            &TREE,
            move |instance| async move {
                merge_deleted_here(&instance).await;
                branch::merge::merge_resolve_mine(
                    instance.repository.clone(),
                    &instance.write_token,
                    named(&instance, "Dir/x.txt"),
                )
                .await
                .expect("Failed to resolve taking our side");
                let node = staged_node(&instance, "Dir/x.txt").await;
                assert!(
                    node.is_staged_delete() && node.is_staged_add(),
                    "flags {:#x}",
                    node.flags
                );

                let paths = named(&instance, "Dir/x.txt");
                if theirs {
                    branch::merge::merge_resolve_theirs(
                        instance.repository.clone(),
                        &instance.write_token,
                        paths,
                    )
                    .await
                    .expect("Failed to resolve taking their side");
                } else {
                    write(&instance, "Dir/x.txt", "recreated");
                    branch::merge::branch_merge_resolve(
                        instance.repository.clone(),
                        &instance.write_token,
                        paths,
                    )
                    .await
                    .expect("Failed to resolve");
                }

                let node = staged_node(&instance, "Dir/x.txt").await;
                let context = format!("theirs {theirs}: flags {:#x}", node.flags);
                assert!(node.is_staged_add(), "{context}");
                assert!(!node.is_staged_delete(), "{context}");
                assert!(!node.is_staged_modify(), "{context}");
            },
        )
        .await;
    }
}

/// Aborting a merge that staged a file this branch deletes leaves neither the file nor the copy
/// the merge wrote beside it.
#[tokio::test(flavor = "multi_thread")]
async fn aborting_a_staged_deletion_leaves_no_copy() {
    run(
        "aborting_a_staged_deletion_leaves_no_copy",
        &TREE,
        |instance| async move {
            merge_deleted_here(&instance).await;
            branch::merge::branch_merge_abort(
                instance.repository.clone(),
                &instance.write_token,
                None,
                false,
            )
            .await
            .expect("Failed to abort the merge");
            assert_eq!(working_tree(&instance.path), without_x());
        },
    )
    .await;
}

/// A conflict at a path the view excludes, where the incoming branch deletes the file this branch
/// changes, takes the incoming deletion: the merge stages it deleted with the incoming side taken,
/// in no conflict, and the merge revision holds no file there.
#[tokio::test(flavor = "multi_thread")]
async fn an_incoming_deletion_out_of_view_is_taken() {
    run(
        "an_incoming_deletion_out_of_view_is_taken",
        &TREE,
        |mut instance| async move {
            create_branch(&instance, "feature").await;
            commit_edits(&instance, &[Edit::Delete("Dir/x.txt")], "Feature").await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            commit_edits(&instance, &[Edit::Write("Dir/x.txt", "main")], "Main").await;
            instance.repository = Arc::new(
                instance
                    .repository
                    .to_filter_context(Arc::new(test_filter(&[], &["/Dir"]))),
            );
            start_merge(&instance, "feature", true)
                .await
                .expect("Failed to merge");

            let node = staged_node(&instance, "Dir/x.txt").await;
            assert!(node.is_staged_delete(), "flags {:#x}", node.flags);
            assert!(node.is_staged_merge_theirs(), "flags {:#x}", node.flags);
            assert!(!node.is_staged_merge_conflict(), "flags {:#x}", node.flags);

            commit(&instance, "Merge")
                .await
                .expect("The merge revision commits");
            let merged = revision_state(&instance, current(&instance).await).await;
            assert!(
                !merged
                    .find_node_link(instance.repository.clone(), "Dir/x.txt")
                    .await
                    .is_ok_and(|found| found.is_valid()),
                "the merge revision holds the deleted file"
            );
        },
    )
    .await;
}

/// A resolution keeping the deletion of a file in a directory the current revision lacks, taking
/// our side or marking it resolved with no file there, leaves the directory where the working tree
/// holds more in it, such as an untracked file or one an ignore rule names, and removes only the
/// incoming copy.
#[tokio::test(flavor = "multi_thread")]
async fn a_resolved_deletion_keeps_the_files_the_tree_does_not_track() {
    for (local, mine) in [
        ("Dir/notes.txt", true),
        ("Dir/notes.txt", false),
        ("Dir/build.log", true),
        ("Dir/build.log", false),
    ] {
        run(
            "a_resolved_deletion_keeps_the_files_the_tree_does_not_track",
            &TREE,
            move |mut instance| async move {
                instance.repository = Arc::new(
                    instance
                        .repository
                        .to_filter_context(Arc::new(test_filter(&["*.log"], &[]))),
                );
                merge_over(&instance, Edit::Delete("Dir")).await;
                write(&instance, local, "local");
                resolve(&instance, "Dir/x.txt", mine).await;

                commit(&instance, "Merge")
                    .await
                    .expect("The resolved merge commits");
                let mut expected = without_x();
                expected.insert(local.to_string(), b"local".to_vec());
                assert_eq!(
                    working_tree(&instance.path),
                    expected,
                    "{local}, resolved to mine {mine}"
                );
            },
        )
        .await;
    }
}

/// A file this branch deletes with its directory, recreated where an ignore rule names it, stays
/// in the working tree when its conflict is marked resolved.
#[tokio::test(flavor = "multi_thread")]
async fn a_resolved_deletion_keeps_an_ignored_recreated_file() {
    run(
        "a_resolved_deletion_keeps_an_ignored_recreated_file",
        &TREE,
        |mut instance| async move {
            merge_over(&instance, Edit::Delete("Dir")).await;
            instance.repository = Arc::new(
                instance
                    .repository
                    .to_filter_context(Arc::new(test_filter(&["x.txt"], &[]))),
            );
            write(&instance, "Dir/x.txt", "recreated");
            resolve(&instance, "Dir/x.txt", false).await;

            commit(&instance, "Merge")
                .await
                .expect("The resolved merge commits");
            let mut expected = without_x();
            expected.insert("Dir/x.txt".to_string(), b"recreated".to_vec());
            assert_eq!(working_tree(&instance.path), expected);
        },
    )
    .await;
}

/// A symbolic link at the path of a file this branch deletes with its directory, which the stage
/// does not version, stays in the working tree, and its directory with it, when the conflict is
/// marked resolved.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_resolved_deletion_keeps_a_symbolic_link_at_its_path() {
    run(
        "a_resolved_deletion_keeps_a_symbolic_link_at_its_path",
        &TREE,
        |instance| async move {
            merge_over(&instance, Edit::Delete("Dir")).await;
            let link = instance.path.join("Dir/x.txt");
            std::os::unix::fs::symlink("missing.txt", &link).expect("Failed to link");
            resolve(&instance, "Dir/x.txt", false).await;

            commit(&instance, "Merge")
                .await
                .expect("The resolved merge commits");
            assert!(
                std::fs::symlink_metadata(&link).is_ok_and(|held| held.file_type().is_symlink()),
                "the link stays"
            );
        },
    )
    .await;
}

/// A conflict at a file both branches change, which the working tree lacks when it is marked
/// resolved, is staged as a resolved deletion, and the commit records the deletion and leaves no
/// copy.
#[tokio::test(flavor = "multi_thread")]
async fn a_resolved_conflict_whose_file_is_gone_commits_the_deletion() {
    run(
        "a_resolved_conflict_whose_file_is_gone_commits_the_deletion",
        &TREE,
        |instance| async move {
            merge_over(&instance, Edit::Write("Dir/x.txt", "main")).await;
            std::fs::remove_file(instance.path.join("Dir/x.txt")).expect("Failed to delete");
            resolve(&instance, "Dir/x.txt", false).await;

            let node = staged_node(&instance, "Dir/x.txt").await;
            assert!(node.is_staged_delete(), "flags {:#x}", node.flags);
            assert!(node.is_staged_merge_resolved(), "flags {:#x}", node.flags);

            commit(&instance, "Merge")
                .await
                .expect("The resolved merge commits");
            assert_eq!(working_tree(&instance.path), without_x());
            assert_eq!(
                delta(&instance).await,
                [(FileAction::Delete as u16, "Dir/x.txt".to_string())]
            );
        },
    )
    .await;
}

/// A commit refuses an unresolved conflict below a directory staged for deletion.
#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_directory_holding_an_unresolved_conflict_is_refused() {
    run(
        "a_deleted_directory_holding_an_unresolved_conflict_is_refused",
        &TREE,
        |instance| async move {
            merge_over(&instance, Edit::Write("Dir/x.txt", "main")).await;
            std::fs::remove_dir_all(instance.path.join("Dir")).expect("Failed to delete");
            stage(&instance, &instance.path.join("Dir")).await;

            let refused = commit(&instance, "Merge")
                .await
                .expect_err("The commit refuses an unresolved conflict");
            assert!(refused.is_conflict(), "{refused}");
        },
    )
    .await;
}

/// A merge reconciling diverged history, the local revision merged with the branch latest, which
/// is its first parent, records the deletion of a file staged on it that both hold, and no
/// deletion of a file the latest deleted, whose slot the latest's tree reuses for a file it adds.
#[tokio::test(flavor = "multi_thread")]
async fn a_merge_of_diverged_history_records_the_deletions_its_first_parent_holds() {
    const TREE: [&str; 3] = ["K.txt", "Top.txt", "gone.txt"];
    run(
        "a_merge_of_diverged_history_records_the_deletions_its_first_parent_holds",
        &TREE,
        |instance| async move {
            create_branch(&instance, "upstream").await;
            commit_edits(&instance, &[Edit::Delete("gone.txt")], "Delete").await;
            let latest = commit_edits(
                &instance,
                &[
                    Edit::Write("added.txt", "added"),
                    Edit::Write("Top.txt", "upstream"),
                ],
                "Upstream",
            )
            .await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            let local = commit_edits(&instance, &[Edit::Write("Top.txt", "local")], "Local").await;
            merge_diverged(&instance, local, latest).await;
            std::fs::remove_file(instance.path.join("K.txt")).expect("Failed to delete");
            stage(&instance, &instance.path.join("K.txt")).await;
            resolve(&instance, "Top.txt", true).await;

            commit(&instance, "Merge")
                .await
                .expect("The resolved merge commits");
            let merged = revision_state(&instance, current(&instance).await).await;
            assert_eq!(merged.parent_self(), latest);
            assert_eq!(deletions(&instance).await, ["K.txt"]);
        },
    )
    .await;
}

/// Changes staged on an earlier revision, committed by force, record the revision's change from the
/// revision they're committed on: the deletion of a file staged for deletion, and of the file that
/// revision added, which the staged tree lacks, and no deletion of a file that revision lacks, whose
/// slot its tree reuses for that add. A file the staged tree keeps in that slot is recorded as
/// added, and keeps its history, which the add does not take.
#[tokio::test(flavor = "multi_thread")]
async fn stale_staged_changes_committed_by_force_record_their_change_from_the_parent() {
    const TREE: [&str; 3] = ["K.txt", "Top.txt", "gone.txt"];
    for keep_gone in [false, true] {
        run(
            "stale_staged_changes_committed_by_force_record_their_change_from_the_parent",
            &TREE,
            move |instance| async move {
                let first = current(&instance).await;
                commit_edits(&instance, &[Edit::Delete("gone.txt")], "Delete").await;
                let latest =
                    commit_edits(&instance, &[Edit::Write("added.txt", "added")], "Add").await;
                sync_to(&instance, first).await.expect("Failed to sync");
                let deleted: &[&str] = if keep_gone {
                    &["K.txt"]
                } else {
                    &["K.txt", "gone.txt"]
                };
                for path in deleted {
                    std::fs::remove_file(instance.path.join(path)).expect("Failed to delete");
                }
                stage(&instance, &instance.path).await;
                lore_revision::instance::store_current_anchor(&instance.repository, latest)
                    .await
                    .expect("Failed to move the current revision");

                let committed = with_flags(
                    |flags| flags.force = 1,
                    commit(&instance, "Delete on a stale stage"),
                )
                .await
                .expect("The forced commit succeeds");
                let state = revision_state(&instance, committed).await;
                assert_eq!(state.parent_self(), latest, "keep gone {keep_gone}");
                let mut expected = vec![
                    (FileAction::Delete as u16, "K.txt".to_string(), false),
                    (FileAction::Delete as u16, "added.txt".to_string(), false),
                ];
                if keep_gone {
                    expected.push((FileAction::Add as u16, "gone.txt".to_string(), false));
                }
                assert_eq!(
                    file_changes(&instance, committed).await,
                    expected,
                    "keep gone {keep_gone}"
                );
                if keep_gone {
                    assert_eq!(
                        first_parent_history(&instance, committed, "gone.txt").await,
                        first_parent_history(&instance, first, "gone.txt").await
                    );
                }
            },
        )
        .await;
    }
}

/// A plain resolve of a file this branch deletes, in a directory the current revision holds,
/// leaves the copy the merge wrote beside it until the commit, which removes it.
#[tokio::test(flavor = "multi_thread")]
async fn a_plain_resolve_keeps_the_copies_until_the_commit() {
    run(
        "a_plain_resolve_keeps_the_copies_until_the_commit",
        &TREE,
        |instance| async move {
            merge_deleted_here(&instance).await;
            resolve(&instance, "Dir/x.txt", false).await;
            let mut expected = without_x();
            expected.insert("Dir/x.txt~theirs".to_string(), b"feature".to_vec());
            assert_eq!(working_tree(&instance.path), expected);

            commit(&instance, "Merge")
                .await
                .expect("The resolved merge commits");
            assert_eq!(working_tree(&instance.path), without_x());
        },
    )
    .await;
}

/// A dry run of a resolve keeping the deletion of a file in a directory the current revision
/// lacks, taking our side or marking it resolved with no file there, removes nothing, and stages
/// nothing the directory holds: neither the copy the merge wrote nor an untracked file.
#[tokio::test(flavor = "multi_thread")]
async fn a_dry_run_of_a_resolved_deletion_stages_nothing_the_directory_holds() {
    for mine in [true, false] {
        run(
            "a_dry_run_of_a_resolved_deletion_stages_nothing_the_directory_holds",
            &TREE,
            move |instance| async move {
                merge_over(&instance, Edit::Delete("Dir")).await;
                write(&instance, "Dir/notes.txt", "local");
                let before = working_tree(&instance.path);
                with_flags(
                    |flags| flags.dry_run = 1,
                    Box::pin(resolve(&instance, "Dir/x.txt", mine)),
                )
                .await;

                assert_eq!(working_tree(&instance.path), before, "mine {mine}");
                let revision = lore_revision::instance::load_staged_revision(&instance.repository)
                    .await
                    .expect("Failed to load the staged revision")
                    .expect("A merge left a staged revision");
                let state = revision_state(&instance, revision).await;
                for path in ["Dir/x.txt~theirs", "Dir/notes.txt"] {
                    assert!(
                        !state
                            .find_node_link(instance.repository.clone(), path)
                            .await
                            .is_ok_and(|found| found.is_valid()),
                        "mine {mine}: the dry run staged {path}"
                    );
                }
            },
        )
        .await;
    }
}

/// A merge reconciling diverged history records the deletion of a file this side deleted and the
/// branch latest changed, resolved as the deletion it is, under the ID the latest holds the file
/// at, found at its path: this side's tree holds the file at another ID, its slot reused for an
/// add.
#[tokio::test(flavor = "multi_thread")]
async fn a_merge_of_diverged_history_records_a_deletion_found_at_its_path() {
    const TREE: [&str; 2] = ["F.txt", "Top.txt"];
    run(
        "a_merge_of_diverged_history_records_a_deletion_found_at_its_path",
        &TREE,
        |instance| async move {
            create_branch(&instance, "upstream").await;
            let latest =
                commit_edits(&instance, &[Edit::Write("F.txt", "upstream")], "Upstream").await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            commit_edits(&instance, &[Edit::Delete("F.txt")], "Delete").await;
            let local = commit_edits(&instance, &[Edit::Write("G.txt", "local")], "Add").await;
            merge_diverged(&instance, local, latest).await;
            resolve(&instance, "F.txt", false).await;

            commit(&instance, "Merge")
                .await
                .expect("The resolved merge commits");
            let merged = revision_state(&instance, current(&instance).await).await;
            assert_eq!(merged.parent_self(), latest);
            assert_eq!(deletions(&instance).await, ["F.txt"]);
        },
    )
    .await;
}

/// The file changes the delta of `revision` records ([`delta_entries`]), each as the action, the
/// path and whether its content changed, sorted by path. Every entry of a revision with a second
/// parent is checked to be marked merged.
async fn file_changes(instance: &Instance, revision: Hash) -> Vec<(u16, String, bool)> {
    let merge = !revision_state(instance, revision)
        .await
        .parent_other()
        .is_zero();
    let mut changes: Vec<(u16, String, bool)> = delta_entries(instance, revision)
        .await
        .into_iter()
        .filter(|&(.., is_file)| is_file)
        .map(|(action, path, flags, _)| {
            assert_eq!(
                flags & change::Flags::Merge.bits() != 0,
                merge,
                "{action} {path} {flags:#x}"
            );
            (action, path, flags & change::Flags::Modify.bits() != 0)
        })
        .collect();
    changes.sort_by(|a, b| a.1.cmp(&b.1));
    changes
}

/// The files [`merge_divergent_changes`] starts from.
const DIVERGENT: [&str; 4] = ["K.txt", "Top.txt", "gone.txt", "moved.txt"];

/// Commits, on an upstream branch, the deletion of `gone.txt` and then the add of `added.txt`, its
/// tree reusing the deleted file's slot, and on main the add of `local.txt`, an edit of `K.txt` and
/// the move of `moved.txt`. Merges the upstream latest, which a remote would answer as the branch
/// latest, into the local revision, as a sync of divergent history does, and commits the merge
/// revision. Answers the upstream latest and the merge revision.
async fn merge_divergent_changes(instance: &Instance) -> (Hash, Hash) {
    create_branch(instance, "upstream").await;
    commit_edits(instance, &[Edit::Delete("gone.txt")], "Delete").await;
    let latest = commit_edits(instance, &[Edit::Write("added.txt", "added")], "Add").await;
    switch_to(instance, branch::DEFAULT_DEFAULT_NAME).await;
    let local = commit_edits(
        instance,
        &[
            Edit::Write("local.txt", "local"),
            Edit::Write("K.txt", "local"),
            Edit::Move("moved.txt", "moved-locally.txt"),
        ],
        "Local",
    )
    .await;
    merge_diverged(instance, local, latest).await;
    let merged = commit(instance, "Merge")
        .await
        .expect("The merge revision commits");
    (latest, merged)
}

/// A merge revision reconciling divergent history records its change from its first parent, the
/// branch latest: this side's changes, each marked merged, under the IDs the merge revision holds
/// them at and, for the move, its source in the latest's tree. It records none of the latest's own
/// changes, which the latest holds already.
#[tokio::test(flavor = "multi_thread")]
async fn a_merge_of_divergent_history_records_its_change_from_its_first_parent() {
    run(
        "a_merge_of_divergent_history_records_its_change_from_its_first_parent",
        &DIVERGENT,
        |instance| async move {
            let (latest, merged) = merge_divergent_changes(&instance).await;
            assert_eq!(
                revision_state(&instance, merged).await.parent_self(),
                latest
            );
            assert_eq!(
                file_changes(&instance, merged).await,
                [
                    (FileAction::Keep as u16, "K.txt".to_string(), true),
                    (FileAction::Add as u16, "local.txt".to_string(), false),
                    (
                        FileAction::Move as u16,
                        "moved-locally.txt".to_string(),
                        false
                    ),
                ]
            );
        },
    )
    .await;
}

/// A revision committed on a merge revision reconciling divergent history records the merge
/// revision as the last change, on its first-parent line, of the files it changed from its first
/// parent, this side's, and the branch latest as that of the file the latest added.
#[tokio::test(flavor = "multi_thread")]
async fn a_revision_committed_on_a_merge_of_divergent_history_records_the_merge_as_the_change_of_this_sides_files()
 {
    run(
        "a_revision_committed_on_a_merge_of_divergent_history_records_the_merge_as_the_change_of_this_sides_files",
        &DIVERGENT,
        |instance| async move {
            let (latest, merged) = merge_divergent_changes(&instance).await;
            let next = commit_edits(&instance, &[Edit::Write("Top.txt", "next")], "Next").await;

            assert_eq!(
                first_parent_history(&instance, next, "local.txt").await.0,
                merged
            );
            assert_eq!(first_parent_history(&instance, next, "K.txt").await.0, merged);
            assert_eq!(
                first_parent_history(&instance, next, "added.txt").await.0,
                latest
            );
        },
    )
    .await;
}

/// A merge revision reconciling divergent history records, directories included, the entries a
/// revision with the same local changes committed on the branch latest records, each marked
/// merged: this side's edit of a file in one directory, an add in another, an edit two directories
/// down, a deletion, and a move into a directory it adds, and none of the latest's change in
/// another directory.
#[tokio::test(flavor = "multi_thread")]
async fn a_merge_of_divergent_history_records_the_entries_a_linear_revision_records() {
    const NESTED: [&str; 6] = [
        "A/a.txt",
        "B/b.txt",
        "C/c.txt",
        "D/E/e.txt",
        "F/f.txt",
        "F/g.txt",
    ];
    run(
        "a_merge_of_divergent_history_records_the_entries_a_linear_revision_records",
        &NESTED,
        |instance| async move {
            let local_edits = [
                Edit::Write("A/a.txt", "local"),
                Edit::Write("B/new.txt", "local"),
                Edit::Write("D/E/e.txt", "local"),
                Edit::Delete("F/f.txt"),
                Edit::Write("G/h.txt", "local"),
                Edit::Move("C/c.txt", "G/c.txt"),
            ];
            create_branch(&instance, "upstream").await;
            let latest =
                commit_edits(&instance, &[Edit::Write("F/g.txt", "upstream")], "Upstream").await;
            create_branch(&instance, "linear").await;
            let linear = commit_edits(&instance, &local_edits, "Linear").await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            let local = commit_edits(&instance, &local_edits, "Local").await;
            merge_diverged(&instance, local, latest).await;
            let merged = commit(&instance, "Merge")
                .await
                .expect("The merge revision commits");

            let entries = async |revision: Hash| {
                let mut entries: Vec<(u16, String, u16, bool)> = delta_entries(&instance, revision)
                    .await
                    .into_iter()
                    .map(|(action, path, flags, is_file)| {
                        (action, path, flags & !change::Flags::Merge.bits(), is_file)
                    })
                    .collect();
                entries.sort();
                entries
            };
            assert_eq!(entries(merged).await, entries(linear).await);
            assert!(
                delta_entries(&instance, merged)
                    .await
                    .iter()
                    .all(|&(_, _, flags, _)| flags & change::Flags::Merge.bits() != 0)
            );
        },
    )
    .await;
}

/// An add a merge staged, modified, marked dirty by a scanning status and then removed from the
/// working tree, stays in the staged State through `lore file dirty` of its path, marked as the
/// scan left it. The merge commit holds no node at its path.
#[tokio::test(flavor = "multi_thread")]
async fn a_scanned_merge_staged_add_removed_stays_in_the_staged_state() {
    run(
        "a_scanned_merge_staged_add_removed_stays_in_the_staged_state",
        &TREE,
        |instance| async move {
            create_branch(&instance, "feature").await;
            let theirs = commit_edits(&instance, &[Edit::Write("New.txt", "new")], "Feature").await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            start_merge(&instance, "feature", true)
                .await
                .expect("Failed to merge");
            let merged = staged_node(&instance, "New.txt").await;
            write(&instance, "New.txt", "changed");
            status_boxed(
                instance.repository.clone(),
                None,
                StatusOptions {
                    staged: true,
                    scan: true,
                    check_dirty: false,
                    reset: false,
                    sync_point: false,
                    revision_only: false,
                    count: false,
                },
            )
            .await
            .expect("Failed to scan");
            let scanned = staged_node(&instance, "New.txt").await;
            assert_eq!(
                scanned.flags,
                merged.flags | NodeFlags::Dirty.bits(),
                "flags {:#x}",
                scanned.flags
            );
            std::fs::remove_file(instance.path.join("New.txt")).expect("Failed to remove New.txt");

            marked_dirty(&instance, "New.txt").await;
            let node = staged_node(&instance, "New.txt").await;
            assert_eq!(node.flags, scanned.flags, "flags {:#x}", node.flags);

            commit(&instance, "Merge").await.expect("Failed to commit");
            let committed = revision_state(&instance, current(&instance).await).await;
            assert_eq!(committed.parent_other(), theirs);
            assert!(
                !committed
                    .find_node_link(instance.repository.clone(), "New.txt")
                    .await
                    .is_ok_and(|found| found.is_valid()),
                "the merge commit holds New.txt"
            );
        },
    )
    .await;
}

/// Marks the working tree's `path` dirty, as `lore file dirty` does.
async fn marked_dirty(instance: &Instance, path: &str) {
    lore_revision::file::dirty::dirty(
        instance.repository.clone(),
        LoreArray::from_vec(vec![LoreString::from(&instance.path.join(path))]),
    )
    .await
    .expect("Failed to mark the path dirty");
}

/// A directory a view names that holds a change a pending merge staged, removed from the working
/// tree, stays while the merge is pending, whichever [`Route`] stages it: under
/// `**`, `!/a/b/`, `/a/b/*`, `!/Top.txt`, with the feature branch's change to the hidden
/// `a/b/y.txt` staged, removing `a` stages no deletion, and the merge revision holds `a/b/x.txt`
/// and the feature branch's `a/b/y.txt`. A scan then marks `a` deleted, and staging the marks and a
/// commit remove it with everything below it.
#[tokio::test(flavor = "multi_thread")]
async fn a_removed_directory_holding_a_merged_change_is_deleted_after_the_merge() {
    const TREE: [&str; 3] = ["a/b/x.txt", "a/b/y.txt", "Top.txt"];
    const VIEW: [&str; 4] = ["**", "!/a/b/", "/a/b/*", "!/Top.txt"];
    for route in [Route::Marked, Route::Scanned, Route::Walked] {
        run(
            "a_removed_directory_holding_a_merged_change_is_deleted_after_the_merge",
            &TREE,
            move |mut instance| async move {
                let what = format!("{route:?}");
                create_branch(&instance, "feature").await;
                let feature =
                    commit_edits(&instance, &[Edit::Write("a/b/y.txt", "feature")], "Feature")
                        .await;
                let merged = node_at(&instance, feature, "a/b/y.txt").await.address;
                switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
                commit_edits(&instance, &[Edit::Write("Top.txt", "main")], "Main").await;
                for path in ["a/b/x.txt", "a/b/y.txt"] {
                    std::fs::remove_file(instance.path.join(path)).expect("Failed to delete");
                }
                instance.repository = Arc::new(
                    instance
                        .repository
                        .to_filter_context(Arc::new(test_filter(&[], &VIEW))),
                );
                start_merge(&instance, "feature", true)
                    .await
                    .expect("Failed to merge");

                std::fs::remove_dir_all(instance.path.join("a")).expect("Failed to delete");
                let node = if matches!(route, Route::Walked) {
                    let staged = test_staged_by_intent(&instance.repository).await;
                    node_in(&instance, &staged, "a").await
                } else {
                    staged_root(&instance, matches!(route, Route::Scanned)).await;
                    staged_node(&instance, "a").await
                };
                assert!(!node.is_staged_delete(), "{what}: flags {:#x}", node.flags);

                commit(&instance, "Merge")
                    .await
                    .unwrap_or_else(|error| panic!("{what}: committing: {error}"));
                let merge = current(&instance).await;
                assert_eq!(
                    node_at(&instance, merge, "a/b/y.txt").await.address,
                    merged,
                    "{what}"
                );
                node_at(&instance, merge, "a/b/x.txt").await;

                status_boxed(
                    instance.repository.clone(),
                    None,
                    StatusOptions {
                        staged: true,
                        scan: true,
                        check_dirty: false,
                        reset: false,
                        sync_point: false,
                        revision_only: false,
                        count: false,
                    },
                )
                .await
                .expect("Failed to scan");
                let node = staged_node(&instance, "a").await;
                assert!(node.is_dirty_delete(), "{what}: flags {:#x}", node.flags);
                staged_root(&instance, false).await;
                commit(&instance, "Removed")
                    .await
                    .expect("The removal commits");
                let removed = revision_state(&instance, current(&instance).await).await;
                for (path, held) in [("a", false), ("Top.txt", true)] {
                    assert_eq!(
                        removed
                            .find_node_link(instance.repository.clone(), path)
                            .await
                            .is_ok_and(|found| found.is_valid()),
                        held,
                        "{what}: {path}"
                    );
                }
            },
        )
        .await;
    }
}

/// How a test stages: the marks `lore dirty .` sets, by a scan, or by the scan walk that stages,
/// held in memory.
#[derive(Clone, Copy, Debug)]
enum Route {
    Marked,
    Scanned,
    Walked,
}

/// Stages the whole working tree: by a scan where `scan`, and otherwise the marks `lore dirty .`
/// sets.
async fn staged_root(instance: &Instance, scan: bool) {
    if scan {
        stage(instance, &instance.path).await;
        return;
    }
    lore_revision::file::dirty::dirty(
        instance.repository.clone(),
        LoreArray::from_vec(vec![LoreString::from(instance.path.as_path())]),
    )
    .await
    .expect("Failed to mark the working tree");
    lore_revision::file::stage::stage(
        instance.repository.clone(),
        &instance.write_token,
        LoreArray::from_vec(vec![LoreString::from(instance.path.as_path())]),
        lore_revision::stage::StageOptions::default(),
    )
    .await
    .expect("Failed to stage");
}
