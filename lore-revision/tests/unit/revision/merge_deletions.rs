// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![allow(clippy::disallowed_methods)] // The tests delete files from the working tree directly.

//! `lore branch merge` of a file one branch deletes and the other changes: what the merge stages,
//! what a resolution and the commit make of it, and what an abort leaves; and the deletions a
//! commit's delta records.

use std::collections::BTreeMap;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_base::types::TypedBytes as _;
use lore_revision::branch;
use lore_revision::branch::BranchLatestStatus;
use lore_revision::change::FileAction;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreString;
use lore_revision::node::Node;
use lore_revision::node::NodeDelta;
use lore_revision::node::NodeFileMetadata;
use lore_revision::node::NodeFileMetadataBlock;
use lore_revision::node::NodeID;
use lore_revision::node::node_to_file_metadata;
use lore_revision::relay::EventDispatcher;

use super::instance::Edit;
use super::instance::Instance;
use super::instance::commit;
use super::instance::commit_edits;
use super::instance::create_branch;
use super::instance::current;
use super::instance::revision_state;
use super::instance::run;
use super::instance::stage;
use super::instance::start_merge;
use super::instance::switch_to;
use super::instance::sync_to;
use super::instance::working_tree;
use super::instance::write;
use crate::helper::test_filter;

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

/// `path` in the working tree, as a resolve names it.
fn named(instance: &Instance, path: &str) -> LoreArray<LoreString> {
    LoreArray::from_vec(vec![LoreString::from(instance.path.join(path).as_path())])
}

/// The adds, deletes and moves the delta of the current revision records, as the action and the
/// path: a deleted node's path read from the first parent, the tree that holds it.
async fn delta(instance: &Instance) -> Vec<(u16, String)> {
    let merged = revision_state(instance, current(instance).await).await;
    let parent = revision_state(instance, merged.parent_self()).await;
    let delta = merged
        .delta_block(instance.repository.clone())
        .await
        .expect("Failed to read the delta")
        .to_aligned::<NodeDelta>();
    let mut changes = Vec::new();
    for entry in delta.as_type_slice::<NodeDelta>() {
        if entry.action == FileAction::Keep as u16 {
            continue;
        }
        let tree = if entry.action == FileAction::Delete as u16 {
            &parent
        } else {
            &merged
        };
        let path = tree
            .node_path(instance.repository.clone(), entry.node)
            .await
            .expect("Failed to read a delta path");
        changes.push((entry.action, path));
    }
    changes
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

            commit(&instance, "Merge").await.expect("The merge commits");
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

/// A forced commit of a stage left on an earlier revision records the deletion of a file staged on
/// it that the revision it commits on holds, and no deletion of a file that revision lacks, whose
/// slot its tree reuses for a file it adds. A file the stage keeps in that slot keeps its history,
/// which the add does not take.
#[tokio::test(flavor = "multi_thread")]
async fn a_forced_commit_of_a_stale_stage_records_the_deletions_its_parent_holds() {
    const TREE: [&str; 3] = ["K.txt", "Top.txt", "gone.txt"];
    for keep_gone in [false, true] {
        run(
            "a_forced_commit_of_a_stale_stage_records_the_deletions_its_parent_holds",
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
                assert_eq!(
                    deletions(&instance).await,
                    ["K.txt"],
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
