// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Diffs from a stored revision whose staged directory keeps the address it held before the changes
//! staged below it, to a revision whose same directory holds that address clean.

use std::sync::Arc;

use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::branch::BranchLatestStatus;
use lore_revision::change::FileAction;
use lore_revision::change::NodeChange;
use lore_revision::commit::merge_tree_changes;
use lore_revision::filter::FilterMode;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::revision::DiffItem;
use lore_revision::revision::LinkSettlement;
use lore_revision::revision::MergesAcross;
use lore_revision::revision::diff3_collect;
use lore_revision::revision::diff3_with_source_cap;
use lore_revision::stage::stage_single_node;
use lore_revision::state::apply_tree_changes;
use lore_revision::state::diff_collect;
use lore_revision::util::path::RelativePath;
use tokio::sync::mpsc;

use super::instance::Edit;
use super::instance::Instance;
use super::instance::commit_edits;
use super::instance::create_branch;
use super::instance::current;
use super::instance::node_at;
use super::instance::revision_state;
use super::instance::run;
use super::instance::switch_to;
use super::instance::sync_to;
use super::instance::working_tree;

/// The files every fixture starts from, each holding its own path.
pub(super) const TREE: [&str; 2] = ["Dir/a.txt", "Top.txt"];

/// The revisions of a fixture: `stale`, a merge that adds `Dir/x.txt` stored as it was staged, holds
/// `Dir` staged with the address of `Dir` in its first parent, and `deleted`, a committed merge onto
/// `stale` of the delete of `Dir/x.txt`, holds that address clean.
pub(super) struct Revisions {
    pub(super) stale: Hash,
    pub(super) deleted: Hash,
}

/// Stages `changes`, which carry `source`'s content, onto the tree of `target`, and answers the
/// stored revision: the merged tree committed as a client merge commits it where `commit`, and
/// stored as staged where not, with `Dir` staged at the address it holds in `target`.
async fn store_merge(
    instance: &Instance,
    mut changes: Vec<NodeChange>,
    source: Hash,
    target: Hash,
    commit: bool,
) -> Hash {
    let repository = instance.repository.clone();
    let merged = revision_state(instance, target).await;
    let source = revision_state(instance, source).await;
    if commit {
        merge_tree_changes(repository.clone(), merged.clone(), &source, &mut changes)
            .await
            .expect("Failed to merge the changes");
    } else {
        merged.set_parent_self(target);
        merged.set_parent_other(source.revision());
        merged.set_revision_number(merged.revision_number() + 1);
        apply_tree_changes(repository.clone(), merged.clone(), &source, &mut changes)
            .await
            .expect("Failed to stage the merged changes");
        let directory = node_at(instance, target, "Dir").await;
        stage_single_node(
            repository.clone(),
            merged.clone(),
            RelativePath::new_from_clean_parts("Dir", ""),
            Node {
                flags: NodeFlags::StagedMerge.bits(),
                address: directory.address,
                mode: directory.mode,
                ..Default::default()
            },
            Arc::default(),
            None,
            FilterMode::empty(),
        )
        .await
        .expect("Failed to stage the directory");
    }
    merged
        .serialize(repository, &instance.write_token)
        .await
        .expect("Failed to store the merged revision")
}

/// The changes of the 3-way diff of `source` and `target` from `base`, which has no conflicts.
async fn merged_changes(
    instance: &Instance,
    base: Hash,
    source: Hash,
    target: Hash,
) -> Vec<NodeChange> {
    let diff = diff3_collect(
        instance.repository.clone(),
        base,
        source,
        target,
        None,
        true,
    )
    .await
    .expect("Failed to diff the revisions");
    assert!(diff.conflicts.is_empty(), "{:?}", diff.conflicts);
    diff.changes
}

/// The changes of the 2-way diff from `from` to `to`.
pub(super) async fn changes_between(instance: &Instance, from: Hash, to: Hash) -> Vec<NodeChange> {
    diff_collect(
        instance.repository.clone(),
        revision_state(instance, from).await,
        instance.repository.clone(),
        revision_state(instance, to).await,
        None,
        FilterMode::Full,
    )
    .await
    .expect("Failed to diff the revisions")
}

/// Stores the fixture's [`Revisions`], `stale` as the latest of the main branch, and leaves the
/// instance there.
pub(super) async fn revisions(instance: &Instance) -> Revisions {
    let base = current(instance).await;
    create_branch(instance, "feature").await;
    let added = commit_edits(instance, &[Edit::Write("Dir/x.txt", "x")], "Add").await;
    switch_to(instance, branch::DEFAULT_DEFAULT_NAME).await;
    let target = commit_edits(instance, &[Edit::Write("Top.txt", "top")], "Top").await;
    let changes = merged_changes(instance, base, added, target).await;
    let stale = store_merge(instance, changes, added, target, false).await;
    branch::store_latest(
        instance.repository.clone(),
        instance.main,
        target,
        stale,
        BranchLatestStatus::Convergent,
    )
    .await
    .expect("Failed to move the branch");
    switch_to(instance, "feature").await;
    let removed = commit_edits(instance, &[Edit::Delete("Dir/x.txt")], "Delete").await;
    switch_to(instance, branch::DEFAULT_DEFAULT_NAME).await;
    let changes = changes_between(instance, added, removed).await;
    let deleted = store_merge(instance, changes, removed, stale, true).await;

    let address = node_at(instance, target, "Dir").await.address;
    let (stale_directory, deleted_directory) = (
        node_at(instance, stale, "Dir").await,
        node_at(instance, deleted, "Dir").await,
    );
    assert_eq!(
        (stale_directory.address, stale_directory.is_staged()),
        (address, true)
    );
    assert_eq!(
        (deleted_directory.address, deleted_directory.is_staged()),
        (address, false)
    );
    Revisions { stale, deleted }
}

/// Each change by its action and the path it stands at.
pub(super) fn actions(changes: &[NodeChange]) -> Vec<(FileAction, String)> {
    changes
        .iter()
        .map(|change| (change.action, change.path().to_string()))
        .collect()
}

/// A sync from the revision holding the stale directory removes the file its target deleted below
/// it.
#[tokio::test]
async fn a_sync_from_a_stale_directory_removes_what_the_target_deleted_below_it() {
    run(
        "a_sync_from_a_stale_directory_removes_what_the_target_deleted_below_it",
        &TREE,
        |instance| async move {
            let Revisions { deleted, .. } = revisions(&instance).await;
            assert!(working_tree(&instance.path).contains_key("Dir/x.txt"));

            sync_to(&instance, deleted).await.expect("Failed to sync");

            assert!(
                !working_tree(&instance.path).contains_key("Dir/x.txt"),
                "the sync left the deleted file"
            );
        },
    )
    .await;
}

/// The 2-way diff from the revision holding the stale directory reports the delete below it.
#[tokio::test]
async fn a_diff_from_a_stale_directory_reports_the_delete_below_it() {
    run(
        "a_diff_from_a_stale_directory_reports_the_delete_below_it",
        &TREE,
        |instance| async move {
            let Revisions { stale, deleted } = revisions(&instance).await;

            let changes = changes_between(&instance, stale, deleted).await;

            assert_eq!(
                actions(&changes),
                vec![(FileAction::Delete, "Dir/x.txt".to_string())]
            );
        },
    )
    .await;
}

/// The 3-way diff based on the revision holding the stale directory reports the delete the source
/// made below it.
#[tokio::test]
async fn a_merge_based_on_a_stale_directory_takes_the_delete_below_it() {
    run(
        "a_merge_based_on_a_stale_directory_takes_the_delete_below_it",
        &TREE,
        |instance| async move {
            let Revisions { stale, deleted } = revisions(&instance).await;
            let target = commit_edits(&instance, &[Edit::Write("Top.txt", "again")], "Top").await;

            let diff = diff3_collect(
                instance.repository.clone(),
                stale,
                deleted,
                target,
                None,
                false,
            )
            .await
            .expect("Failed to diff the revisions");

            assert!(diff.conflicts.is_empty(), "{:?}", diff.conflicts);
            assert!(
                actions(&diff.changes).contains(&(FileAction::Delete, "Dir/x.txt".to_string())),
                "{:?}",
                actions(&diff.changes)
            );
        },
    )
    .await;
}

/// A merge whose view excludes the stale directory takes the source's edit below it, rather than
/// the source's directory whole over a target whose directory holds the stale address clean, which
/// would undo the target's delete below it.
#[tokio::test]
async fn a_merge_does_not_graft_over_a_target_matching_a_stale_base_directory() {
    run(
        "a_merge_does_not_graft_over_a_target_matching_a_stale_base_directory",
        &TREE,
        |instance| async move {
            let Revisions { stale, deleted } = revisions(&instance).await;
            let source = commit_edits(&instance, &[Edit::Write("Dir/a.txt", "a")], "Edit").await;
            let mut view = (*instance.repository.filter).clone();
            view.view
                .add_exclusion("Dir/")
                .expect("Failed to add the rule");

            let (tx, mut rx) = mpsc::channel(64);
            let diff = diff3_with_source_cap(
                instance.repository.clone(),
                stale,
                source,
                deleted,
                None,
                false,
                None,
                None,
                Some(Arc::new(view)),
                MergesAcross::Unknown,
                LinkSettlement::Tree,
                tx,
            );
            let collect = async {
                let mut changes = Vec::new();
                while let Some(item) = rx.recv().await {
                    match item.expect("A diff item") {
                        DiffItem::Change(change) => changes.push(change),
                        DiffItem::Conflict(pair) => panic!("A conflict: {pair:?}"),
                    }
                }
                changes
            };
            let (summary, changes) = tokio::join!(diff, collect);
            summary.expect("Failed to diff the revisions");

            assert_eq!(
                actions(&changes),
                vec![(FileAction::Keep, "Dir/a.txt".to_string())]
            );
        },
    )
    .await;
}
