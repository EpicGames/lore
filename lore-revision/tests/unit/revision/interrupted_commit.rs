// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![allow(clippy::disallowed_methods)] // Test fixtures edit the working tree directly.

//! A commit cut short after its branch latest reached disk and before its anchors did, as a flush
//! that writes the mutable store's buckets one by one leaves it, and the command after it; the
//! commit intents that name no such commit; and the flush order that keeps the anchors from
//! reaching disk before the branch latest, and the cleared intent from reaching disk before the
//! anchors.

use std::collections::HashMap;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::commit;
use lore_revision::commit::CommitOptions;
use lore_revision::instance::ANCHOR_COMMITTING;
use lore_revision::instance::ANCHOR_CURRENT;
use lore_revision::instance::ANCHOR_CURRENT_BRANCH;
use lore_revision::instance::ANCHOR_STAGED;
use lore_revision::instance::anchor_key;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreString;
use lore_revision::lore::RepositoryId;
use lore_revision::node::ROOT_NODE;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision::amend::AmendRevisionOptions;
use lore_revision::revision::amend::amend_revision_boxed;
use lore_revision::state::State;
use lore_storage::FlushOptions;
use lore_storage::KeyType;
use lore_storage::MutableStore;
use lore_storage::StoreError;
use lore_storage::test_util::HookedStore;
use lore_storage::test_util::StoreCall;
use lore_storage::test_util::answered;

use super::instance::Instance;
use super::instance::commit;
use super::instance::create_branch;
use super::instance::current;
use super::instance::merge_branch;
use super::instance::revision_state;
use super::instance::run;
use super::instance::stage;
use super::instance::stage_and_commit;
use super::instance::switch_to;
use super::instance::write;
use crate::fs::filesystem_provider::setup_test_execution;
use crate::fs::filesystem_provider::test_store_create;
use crate::helper::RepositoryContextCreationArgsExt;
use crate::helper::default_repository_creation_args;
use crate::helper::in_memory;

const TREE: [&str; 2] = ["a.txt", "b.txt"];

/// Writes `revision` to the anchor `function` names, as a flush that reached disk for that
/// anchor's bucket alone leaves it.
async fn store_anchor_as_on_disk(instance: &Instance, function: &str, revision: Hash) {
    let repository = &instance.repository;
    let (key, key_type) = anchor_key(repository.salt(), function, repository.instance_id);
    repository
        .try_write_mutable_store()
        .expect("A write handle")
        .store(repository.id, key, revision, key_type)
        .await
        .expect("Failed to store the anchor");
}

async fn staged(instance: &Instance) -> Option<Hash> {
    lore_revision::instance::load_staged_revision(&instance.repository)
        .await
        .expect("Failed to load the staged anchor")
}

async fn latest(instance: &Instance) -> Hash {
    branch::load_latest(instance.repository.clone(), instance.main)
        .await
        .expect("Failed to load the branch latest")
}

async fn intent(instance: &Instance) -> Hash {
    lore_revision::instance::load_commit_intent(&instance.repository)
        .await
        .expect("Failed to load the commit intent")
}

/// Stages a change to `a.txt` and commits it, answering the revision committed over, the staged
/// revision the commit read and the revision committed.
async fn commit_change(instance: &Instance) -> (Hash, Hash, Hash) {
    let parent = current(instance).await;
    write(instance, "a.txt", "changed");
    stage(instance, &instance.path).await;
    let staged = staged(instance).await.expect("A staged revision");
    let committed = commit(instance, "Second")
        .await
        .expect("The commit succeeds");
    (parent, staged, committed)
}

/// Leaves the anchors as a commit of `committed` cut short leaves them: its intent and the branch
/// latest on disk, and the current and staged anchors on `current` and `staged`.
async fn cut_short(instance: &Instance, committed: Hash, current: Hash, staged: Hash) {
    store_anchor_as_on_disk(instance, ANCHOR_COMMITTING, committed).await;
    store_anchor_as_on_disk(instance, ANCHOR_CURRENT, current).await;
    store_anchor_as_on_disk(instance, ANCHOR_STAGED, staged).await;
}

/// Requires the commit that follows a commit cut short to make the cut-short commit's revision
/// current with nothing left staged, and a commit after it to build on that revision.
async fn assert_completed(instance: &Instance, committed: Hash) {
    let error = commit(instance, "Second")
        .await
        .expect_err("The interrupted commit left nothing to commit");
    assert!(
        error.is_nothing_staged(),
        "Expected NothingStaged, got {error}"
    );
    assert_eq!(current(instance).await, committed);
    assert_eq!(latest(instance).await, committed);
    assert_eq!(staged(instance).await, None);
    assert_eq!(intent(instance).await, Hash::default());

    write(instance, "b.txt", "changed");
    let next = stage_and_commit(instance, &instance.path, "Third").await;
    assert_eq!(
        revision_state(instance, next).await.parent_self(),
        committed
    );
}

#[tokio::test]
async fn a_commit_killed_before_its_current_anchor_reached_disk_is_completed() {
    run("interrupted-commit-current", &TREE, |instance| async move {
        let (parent, staged, committed) = commit_change(&instance).await;
        cut_short(&instance, committed, parent, staged).await;

        assert_completed(&instance, committed).await;
    })
    .await;
}

/// A commit of a delete, cut short before its current anchor reached disk, is completed: the
/// staged delete of a path its revision no longer holds is the commit's own.
#[tokio::test]
async fn a_commit_of_a_delete_killed_before_its_current_anchor_reached_disk_is_completed() {
    run("interrupted-commit-delete", &TREE, |instance| async move {
        let parent = current(&instance).await;
        std::fs::remove_file(instance.path.join("b.txt")).expect("Failed to delete b.txt");
        stage(&instance, &instance.path.join("b.txt")).await;
        let read = staged(&instance).await.expect("A staged delete");
        let committed = commit(&instance, "Second")
            .await
            .expect("The commit succeeds");
        cut_short(&instance, committed, parent, read).await;

        assert_completed(&instance, committed).await;
    })
    .await;
}

#[tokio::test]
async fn a_commit_killed_before_its_staged_anchor_reached_disk_is_completed() {
    run("interrupted-commit-staged", &TREE, |instance| async move {
        let (_parent, staged, committed) = commit_change(&instance).await;
        cut_short(&instance, committed, committed, staged).await;

        assert_completed(&instance, committed).await;
    })
    .await;
}

/// The staged anchor a commit writes for the dirty nodes it leaves, reparented onto its revision,
/// reached disk while the current anchor did not: the dirty node is carried over and stays dirty.
#[tokio::test]
async fn a_commit_killed_after_its_reparented_staged_anchor_reached_disk_is_completed() {
    run(
        "interrupted-commit-reparented",
        &TREE,
        |instance| async move {
            let parent = current(&instance).await;
            write(&instance, "b.txt", "dirty");
            lore_revision::file::dirty::dirty(
                instance.repository.clone(),
                LoreArray::from_vec(vec![LoreString::from(&instance.path.join("b.txt"))]),
            )
            .await
            .expect("Failed to mark the file dirty");
            write(&instance, "a.txt", "changed");
            stage(&instance, &instance.path.join("a.txt")).await;
            let committed = commit(&instance, "Second")
                .await
                .expect("The commit succeeds");
            let reparented = staged(&instance)
                .await
                .expect("A staged anchor of dirty nodes");
            assert_eq!(
                revision_state(&instance, reparented).await.parent_self(),
                committed
            );
            cut_short(&instance, committed, parent, reparented).await;

            let error = commit(&instance, "Second")
                .await
                .expect_err("The interrupted commit left only a dirty file");
            assert!(
                error.is_nothing_staged(),
                "Expected NothingStaged, got {error}"
            );
            assert_eq!(current(&instance).await, committed);
            let carried = staged(&instance)
                .await
                .expect("The dirty node carried over");
            assert_eq!(
                revision_state(&instance, carried).await.parent_self(),
                committed
            );

            let next = stage_and_commit(&instance, &instance.path, "Third").await;
            assert_eq!(
                revision_state(&instance, next).await.parent_self(),
                committed
            );
        },
    )
    .await;
}

/// A merge staged after a commit was cut short, over the staged anchor of dirty nodes it
/// reparented onto its revision, leaves the commit unfinished: finishing it would carry over only
/// the dirty nodes and drop the merge.
#[tokio::test]
async fn a_merge_staged_over_a_reparented_staged_anchor_leaves_the_commit_unfinished() {
    run(
        "interrupted-commit-reparented-merge-since",
        &TREE,
        |instance| async move {
            create_branch(&instance, "side").await;
            switch_to(&instance, "side").await;
            write(&instance, "b.txt", "side");
            let side = stage_and_commit(&instance, &instance.path, "Side").await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            let parent = current(&instance).await;
            write(&instance, "b.txt", "dirty");
            lore_revision::file::dirty::dirty(
                instance.repository.clone(),
                LoreArray::from_vec(vec![LoreString::from(&instance.path.join("b.txt"))]),
            )
            .await
            .expect("Failed to mark the file dirty");
            write(&instance, "a.txt", "changed");
            stage(&instance, &instance.path.join("a.txt")).await;
            let committed = commit(&instance, "Second")
                .await
                .expect("The commit succeeds");
            let reparented = staged(&instance)
                .await
                .expect("A staged anchor of dirty nodes");
            let merge = revision_state(&instance, reparented).await;
            merge.set_parent_other(side);
            let staged_merge = merge
                .serialize(instance.repository.clone(), &instance.write_token)
                .await
                .expect("Failed to serialize the merge");
            cut_short(&instance, committed, parent, staged_merge).await;

            assert_left_as_is(&instance, committed).await;
        },
    )
    .await;
}

/// A merge of divergent history committed as a sync leaves it: the branch latest, `remote`, merged
/// into `local`, the revision the instance stood on, both on one branch. Answers the revision both
/// were made over, `remote`, the merge and the staged merge it committed.
async fn commit_divergent_merge(instance: &Instance) -> (Hash, Hash, Hash, Hash) {
    let base = current(instance).await;
    write(instance, "a.txt", "remote");
    let remote = stage_and_commit(instance, &instance.path, "Remote").await;
    let (latest_key, latest_type) = Keys::of(instance).latest;
    instance
        .repository
        .try_write_mutable_store()
        .expect("A write handle")
        .store(instance.repository.id, latest_key, base, latest_type)
        .await
        .expect("Failed to store the branch latest");
    store_anchor_as_on_disk(instance, ANCHOR_CURRENT, base).await;
    write(instance, "a.txt", "a.txt");
    write(instance, "b.txt", "local");
    let local = stage_and_commit(instance, &instance.path, "Local").await;

    let merge = revision_state(instance, local).await;
    merge.set_parent_self(remote);
    merge.set_parent_other(local);
    let staged_merge = merge
        .serialize(instance.repository.clone(), &instance.write_token)
        .await
        .expect("Failed to serialize the merge");
    store_anchor_as_on_disk(instance, ANCHOR_STAGED, staged_merge).await;
    let merged = commit(instance, "Merge").await.expect("The merge commits");
    assert_eq!(revision_state(instance, merged).await.parent_other(), local);
    // The killed commit's write of the last sync did not reach disk.
    branch::store_last_sync(instance.repository.clone(), instance.main, base).await;
    (base, remote, merged, staged_merge)
}

async fn last_sync(instance: &Instance) -> Hash {
    branch::load_last_sync(instance.repository.clone(), instance.main)
        .await
        .expect("Failed to load the last sync")
}

/// A merge of divergent history is committed over its second parent: a merge commit cut short
/// with the current anchor on that parent is completed, and leaves the last sync on the revision
/// it merged in.
#[tokio::test]
async fn a_merge_of_divergent_history_killed_with_its_current_anchor_on_its_second_parent_is_completed()
 {
    run(
        "interrupted-commit-sync-merge",
        &TREE,
        |instance| async move {
            let (_, remote, merged, _) = commit_divergent_merge(&instance).await;
            let local = revision_state(&instance, merged).await.parent_other();
            cut_short(&instance, merged, local, Hash::default()).await;

            assert_completed(&instance, merged).await;
            assert_eq!(last_sync(&instance).await, remote);
        },
    )
    .await;
}

/// A merge of divergent history cut short with its current anchor on disk and its staged merge
/// still staged over its first parent is completed, and leaves the last sync on the revision it
/// merged in.
#[tokio::test]
async fn a_merge_of_divergent_history_killed_with_its_current_anchor_on_disk_stores_the_last_sync()
{
    run(
        "interrupted-commit-sync-merge-current",
        &TREE,
        |instance| async move {
            let (_, remote, merged, staged_merge) = commit_divergent_merge(&instance).await;
            cut_short(&instance, merged, merged, staged_merge).await;

            assert_completed(&instance, merged).await;
            assert_eq!(last_sync(&instance).await, remote);
        },
    )
    .await;
}

/// A modification staged after a commit was cut short, of a path the commit did not change,
/// leaves the commit unfinished.
#[tokio::test]
async fn a_modification_of_another_path_staged_after_a_commit_was_cut_short_leaves_it_unfinished() {
    run(
        "interrupted-commit-modify-since",
        &TREE,
        |instance| async move {
            let (parent, read, committed) = commit_change(&instance).await;
            cut_short(&instance, committed, parent, read).await;
            write(&instance, "b.txt", "changed since");
            stage(&instance, &instance.path.join("b.txt")).await;

            assert_left_as_is(&instance, committed).await;
        },
    )
    .await;
}

/// A merge of another branch is committed over its first parent: an intent naming one, with the
/// current anchor on its second parent, names no commit cut short.
#[tokio::test]
async fn a_branch_merge_with_the_current_anchor_on_its_second_parent_is_not_completed() {
    run(
        "interrupted-commit-branch-merge",
        &TREE,
        |instance| async move {
            create_branch(&instance, "side").await;
            switch_to(&instance, "side").await;
            write(&instance, "b.txt", "side");
            stage_and_commit(&instance, &instance.path, "Side").await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            write(&instance, "a.txt", "main");
            stage_and_commit(&instance, &instance.path, "Main").await;
            assert!(merge_branch(&instance, "side").await, "The merge committed");
            let merged = current(&instance).await;
            let second_parent = revision_state(&instance, merged).await.parent_other();
            assert!(!second_parent.is_zero(), "The merge has a second parent");
            cut_short(&instance, merged, second_parent, Hash::default()).await;
            write(&instance, "c.txt", "added");
            stage(&instance, &instance.path.join("c.txt")).await;

            let error = commit(&instance, "Next")
                .await
                .expect_err("The intent names no commit cut short");
            assert!(
                error.is_branch_advanced(),
                "Expected BranchAdvanced, got {error}"
            );
            assert_eq!(current(&instance).await, second_parent);
            assert_eq!(intent(&instance).await, Hash::default());
        },
    )
    .await;
}

/// A branch latest that cannot be read fails the commit and keeps the intent, rather than
/// reading as a branch latest the intent does not name.
#[tokio::test]
async fn a_branch_latest_that_cannot_be_read_keeps_the_intent() {
    run(
        "interrupted-commit-unreadable",
        &TREE,
        |instance| async move {
            let (parent, read, committed) = commit_change(&instance).await;
            cut_short(&instance, committed, parent, read).await;
            let keys = Keys::of(&instance);
            let committing = failing_load_of(&instance, keys.latest.0);

            commit::commit_boxed(
                committing,
                &instance.write_token,
                CommitOptions::new(String::from("Third")),
            )
            .await
            .expect_err("The branch latest cannot be read");
            assert_eq!(intent(&instance).await, committed);
        },
    )
    .await;
}

#[tokio::test]
async fn a_branch_latest_the_anchor_moved_away_from_is_not_taken_for_an_interrupted_commit() {
    run("interrupted-commit-moved", &TREE, |instance| async move {
        let (parent, staged, committed) = commit_change(&instance).await;
        store_anchor_as_on_disk(&instance, ANCHOR_COMMITTING, committed).await;
        lore_revision::instance::store_current_anchor(&instance.repository, parent)
            .await
            .expect("Failed to store the current anchor");
        store_anchor_as_on_disk(&instance, ANCHOR_STAGED, staged).await;

        let error = commit(&instance, "Second")
            .await
            .expect_err("The branch latest is not this commit's to complete");
        assert!(
            error.is_branch_advanced(),
            "Expected BranchAdvanced, got {error}"
        );
        assert_eq!(current(&instance).await, parent);
        assert_eq!(latest(&instance).await, committed);
    })
    .await;
}

/// An intent a commit left behind, which a writer that does not know the intent keeps as it moves
/// the current anchor, names no commit cut short where the current anchor stands on a revision
/// the intent's commit was not made over.
#[tokio::test]
async fn an_intent_left_behind_a_moved_current_anchor_is_not_taken_for_an_interrupted_commit() {
    run("interrupted-commit-stale", &TREE, |instance| async move {
        let (first_parent, _staged, _committed) = commit_change(&instance).await;
        write(&instance, "b.txt", "changed");
        let committed = stage_and_commit(&instance, &instance.path, "Third").await;
        cut_short(&instance, committed, first_parent, Hash::default()).await;
        write(&instance, "c.txt", "added");
        stage(&instance, &instance.path.join("c.txt")).await;

        let error = commit(&instance, "Fourth")
            .await
            .expect_err("The intent names no commit cut short");
        assert!(
            error.is_branch_advanced(),
            "Expected BranchAdvanced, got {error}"
        );
        assert_eq!(current(&instance).await, first_parent);
        assert_eq!(latest(&instance).await, committed);
        assert_eq!(intent(&instance).await, Hash::default());
    })
    .await;
}

/// An intent the branch latest does not name, as a commit cut short before its branch latest
/// reached disk leaves it, leaves the next commit to run as it would without one.
#[tokio::test]
async fn an_intent_the_branch_latest_does_not_name_leaves_the_commit_to_run() {
    run(
        "interrupted-commit-unpublished",
        &TREE,
        |instance| async move {
            let parent = current(&instance).await;
            write(&instance, "a.txt", "changed");
            stage(&instance, &instance.path).await;
            let unpublished = staged(&instance).await.expect("A staged revision");
            store_anchor_as_on_disk(&instance, ANCHOR_COMMITTING, unpublished).await;

            let committed = commit(&instance, "Second").await.expect("The commit runs");
            assert_eq!(
                revision_state(&instance, committed).await.parent_self(),
                parent
            );
            assert_eq!(current(&instance).await, committed);
            assert_eq!(intent(&instance).await, Hash::default());
        },
    )
    .await;
}

/// An amend after a commit cut short amends the revision the commit committed, rather than the
/// revision the current anchor was left on.
#[tokio::test]
async fn an_amend_after_a_commit_was_cut_short_amends_its_revision() {
    run("interrupted-commit-amend", &TREE, |instance| async move {
        let (parent, staged, committed) = commit_change(&instance).await;
        cut_short(&instance, committed, parent, staged).await;

        let amended = amend_revision_boxed(
            instance.repository.clone(),
            &instance.write_token,
            AmendRevisionOptions {
                message: Some(String::from("Amended")),
            },
        )
        .await
        .expect("The amend succeeds");
        assert_eq!(
            revision_state(&instance, amended).await.parent_self(),
            parent
        );
        assert_eq!(current(&instance).await, amended);
        assert_eq!(latest(&instance).await, amended);
        assert_eq!(intent(&instance).await, Hash::default());
    })
    .await;
}

/// Requires a commit after a commit of `committed` cut short, with something staged after it, to
/// fail with `BranchAdvanced` and leave the intent, the current anchor and the staged anchor as
/// they were.
async fn assert_left_as_is(instance: &Instance, committed: Hash) {
    let current_before = current(instance).await;
    let staged_before = staged(instance).await;
    let error = commit(instance, "Next")
        .await
        .expect_err("Something was staged after the commit was cut short");
    assert!(
        error.is_branch_advanced(),
        "Expected BranchAdvanced, got {error}"
    );
    assert_eq!(intent(instance).await, committed);
    assert_eq!(current(instance).await, current_before);
    assert_eq!(staged(instance).await, staged_before);
    assert_eq!(latest(instance).await, committed);
}

/// An add staged after a commit was cut short leaves the commit unfinished.
#[tokio::test]
async fn an_add_staged_after_a_commit_was_cut_short_leaves_it_unfinished() {
    run(
        "interrupted-commit-add-since",
        &TREE,
        |instance| async move {
            let (parent, read, committed) = commit_change(&instance).await;
            cut_short(&instance, committed, parent, read).await;
            write(&instance, "c.txt", "added");
            stage(&instance, &instance.path.join("c.txt")).await;

            assert_left_as_is(&instance, committed).await;
        },
    )
    .await;
}

/// An add staged after a commit was cut short, where the current anchor then reached disk on the
/// commit's revision and the staged state is still staged over the revision before it, leaves the
/// commit unfinished.
#[tokio::test]
async fn an_add_staged_with_the_current_anchor_on_the_commit_leaves_it_unfinished() {
    run(
        "interrupted-commit-add-since-current",
        &TREE,
        |instance| async move {
            let (parent, read, committed) = commit_change(&instance).await;
            cut_short(&instance, committed, parent, read).await;
            write(&instance, "c.txt", "added");
            stage(&instance, &instance.path.join("c.txt")).await;
            store_anchor_as_on_disk(&instance, ANCHOR_CURRENT, committed).await;

            assert_left_as_is(&instance, committed).await;
        },
    )
    .await;
}

/// A forced commit after an add was staged since a commit was cut short clears the intent and
/// commits over the anchors as they are, as a forced commit did before commits recorded an intent.
#[tokio::test]
async fn a_forced_commit_over_an_add_staged_after_a_commit_was_cut_short_clears_the_intent() {
    run(
        "interrupted-commit-add-since-forced",
        &TREE,
        |instance| async move {
            let (parent, read, committed) = commit_change(&instance).await;
            cut_short(&instance, committed, parent, read).await;
            write(&instance, "c.txt", "added");
            stage(&instance, &instance.path.join("c.txt")).await;

            let forced = commit_under(
                &instance,
                LoreGlobalArgs {
                    force: 1,
                    ..LoreGlobalArgs::default()
                },
            )
            .await;
            forced.expect("A forced commit runs over the anchors as they are");
            assert_eq!(intent(&instance).await, Hash::default());
        },
    )
    .await;
}

/// A dry run of a commit after a commit was cut short, with nothing staged since, fails with
/// `BranchAdvanced` and leaves the intent, the anchors and the branch latest as they are.
#[tokio::test]
async fn a_dry_run_after_a_commit_was_cut_short_leaves_it_as_it_is() {
    run("interrupted-commit-dry-run", &TREE, |instance| async move {
        let (parent, read, committed) = commit_change(&instance).await;
        cut_short(&instance, committed, parent, read).await;

        let error = commit_under(
            &instance,
            LoreGlobalArgs {
                dry_run: 1,
                ..LoreGlobalArgs::default()
            },
        )
        .await
        .expect_err("The anchors stay behind the branch latest");
        assert!(
            error.is_branch_advanced(),
            "Expected BranchAdvanced, got {error}"
        );
        assert_eq!(intent(&instance).await, committed);
        assert_eq!(current(&instance).await, parent);
        assert_eq!(staged(&instance).await, Some(read));
        assert_eq!(latest(&instance).await, committed);
    })
    .await;
}

/// A dry run of a commit keeps an intent that names no commit cut short, which the real run
/// clears.
#[tokio::test]
async fn a_dry_run_keeps_an_intent_that_names_no_commit_cut_short() {
    run(
        "interrupted-commit-dry-run-stale",
        &TREE,
        |instance| async move {
            let (parent, _, _) = commit_change(&instance).await;
            store_anchor_as_on_disk(&instance, ANCHOR_COMMITTING, parent).await;

            let _ = commit_under(
                &instance,
                LoreGlobalArgs {
                    dry_run: 1,
                    ..LoreGlobalArgs::default()
                },
            )
            .await;
            assert_eq!(intent(&instance).await, parent);
        },
    )
    .await;
}

/// Runs a commit of `instance` under an execution context with `globals`.
async fn commit_under(
    instance: &Instance,
    globals: LoreGlobalArgs,
) -> Result<Hash, commit::CommitError> {
    let context = Arc::new(ExecutionContext::new_client(
        globals.set_offline(),
        EventDispatcher::no_dispatch(),
    ));
    context.set_user_id("operator@example.com").await;
    let repository = instance.repository.clone();
    let token = instance.write_token.share();
    LORE_CONTEXT
        .scope(context, async move {
            commit::commit_boxed(repository, &token, CommitOptions::new(String::from("Next"))).await
        })
        .await
}

/// An amend cut short before its current anchor reached disk, the current anchor still on the
/// revision it replaces, is completed.
#[tokio::test]
async fn an_amend_killed_before_its_current_anchor_reached_disk_is_completed() {
    run("interrupted-commit-amended", &TREE, |instance| async move {
        let (_, _, committed) = commit_change(&instance).await;
        let amended = amend_revision_boxed(
            instance.repository.clone(),
            &instance.write_token,
            AmendRevisionOptions {
                message: Some(String::from("Amended")),
            },
        )
        .await
        .expect("The amend succeeds");
        cut_short(&instance, amended, committed, Hash::default()).await;

        assert_completed(&instance, amended).await;
    })
    .await;
}

/// An amend cut short before its current anchor reached disk, with the staged state of dirty
/// nodes the commit before it left over the revision it replaces, is completed, the dirty node
/// carried over.
#[tokio::test]
async fn an_amend_killed_over_a_staged_state_of_dirty_nodes_is_completed() {
    run(
        "interrupted-commit-amended-dirty",
        &TREE,
        |instance| async move {
            write(&instance, "b.txt", "dirty");
            lore_revision::file::dirty::dirty(
                instance.repository.clone(),
                LoreArray::from_vec(vec![LoreString::from(&instance.path.join("b.txt"))]),
            )
            .await
            .expect("Failed to mark the file dirty");
            write(&instance, "a.txt", "changed");
            stage(&instance, &instance.path.join("a.txt")).await;
            let committed = commit(&instance, "Second")
                .await
                .expect("The commit succeeds");
            let reparented = staged(&instance)
                .await
                .expect("A staged anchor of dirty nodes");
            let amended = amend_revision_boxed(
                instance.repository.clone(),
                &instance.write_token,
                AmendRevisionOptions {
                    message: Some(String::from("Amended")),
                },
            )
            .await
            .expect("The amend succeeds");
            cut_short(&instance, amended, committed, reparented).await;

            let error = commit(&instance, "Next")
                .await
                .expect_err("The interrupted amend left only a dirty file");
            assert!(
                error.is_nothing_staged(),
                "Expected NothingStaged, got {error}"
            );
            assert_eq!(current(&instance).await, amended);
            assert_eq!(intent(&instance).await, Hash::default());
            let carried = staged(&instance)
                .await
                .expect("The dirty node carried over");
            assert_eq!(
                revision_state(&instance, carried).await.parent_self(),
                amended
            );
        },
    )
    .await;
}

/// A move staged after a commit was cut short leaves the commit unfinished.
#[tokio::test]
async fn a_move_staged_after_a_commit_was_cut_short_leaves_it_unfinished() {
    run(
        "interrupted-commit-move-since",
        &TREE,
        |instance| async move {
            let (parent, read, committed) = commit_change(&instance).await;
            cut_short(&instance, committed, parent, read).await;
            let path = |name: &str| instance.path.join(name);
            std::fs::rename(path("b.txt"), path("c.txt")).expect("rename");
            lore_revision::file::stage::stage_move(
                instance.repository.clone(),
                &instance.write_token,
                path("b.txt").to_string_lossy().into_owned(),
                path("c.txt").to_string_lossy().into_owned(),
                lore_revision::stage::StageOptions::default(),
            )
            .await
            .expect("Failed to stage the move");

            assert_left_as_is(&instance, committed).await;
        },
    )
    .await;
}

/// A merge staged after a commit was cut short, which stages other parents, leaves the commit
/// unfinished.
#[tokio::test]
async fn a_merge_staged_after_a_commit_was_cut_short_leaves_it_unfinished() {
    run(
        "interrupted-commit-merge-since",
        &TREE,
        |instance| async move {
            create_branch(&instance, "side").await;
            switch_to(&instance, "side").await;
            write(&instance, "b.txt", "side");
            let side = stage_and_commit(&instance, &instance.path, "Side").await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            let (parent, read, committed) = commit_change(&instance).await;
            let merge = revision_state(&instance, read).await;
            merge.set_parent_other(side);
            let staged_merge = merge
                .serialize(instance.repository.clone(), &instance.write_token)
                .await
                .expect("Failed to serialize the merge");
            cut_short(&instance, committed, parent, staged_merge).await;

            assert_left_as_is(&instance, committed).await;
        },
    )
    .await;
}

/// The first commit of a branch, made over no revision, cut short with nothing staged after it,
/// is completed.
#[tokio::test]
async fn a_first_commit_killed_before_its_current_anchor_reached_disk_is_completed() {
    run("interrupted-commit-first", &TREE, |instance| async move {
        let first = current(&instance).await;
        let parent = revision_state(&instance, first).await.parent_self();
        assert!(
            parent.is_zero(),
            "The first commit is made over no revision"
        );
        cut_short(&instance, first, parent, Hash::default()).await;

        assert_completed(&instance, first).await;
    })
    .await;
}

/// A current anchor that cannot be read fails the commit and keeps the intent, rather than
/// reading as a current anchor on no revision.
#[tokio::test]
async fn a_current_anchor_that_cannot_be_read_keeps_the_intent() {
    run(
        "interrupted-commit-unreadable-current",
        &TREE,
        |instance| async move {
            let (parent, read, committed) = commit_change(&instance).await;
            cut_short(&instance, committed, parent, read).await;
            let keys = Keys::of(&instance);
            let committing = failing_load_of(&instance, keys.current.0);

            commit::commit_boxed(
                committing,
                &instance.write_token,
                CommitOptions::new(String::from("Third")),
            )
            .await
            .expect_err("The current anchor cannot be read");
            assert_eq!(intent(&instance).await, committed);
        },
    )
    .await;
}

/// A staged anchor that cannot be read fails the commit and keeps the intent and the current
/// anchor, rather than reading as no staged state and finishing the commit over work that may have
/// been staged after it.
#[tokio::test]
async fn a_staged_anchor_that_cannot_be_read_keeps_the_intent() {
    run(
        "interrupted-commit-unreadable-staged",
        &TREE,
        |instance| async move {
            let (parent, read, committed) = commit_change(&instance).await;
            cut_short(&instance, committed, parent, read).await;
            let keys = Keys::of(&instance);
            let committing = failing_load_of(&instance, keys.staged.0);

            commit::commit_boxed(
                committing,
                &instance.write_token,
                CommitOptions::new(String::from("Third")),
            )
            .await
            .expect_err("The staged anchor cannot be read");
            assert_eq!(intent(&instance).await, committed);
            assert_eq!(current(&instance).await, parent);
        },
    )
    .await;
}

/// A current branch that cannot be read fails the commit and keeps the intent, rather than
/// reading the branch from the legacy anchor file or as no branch.
#[tokio::test]
async fn a_current_branch_that_cannot_be_read_keeps_the_intent() {
    run(
        "interrupted-commit-unreadable-branch",
        &TREE,
        |instance| async move {
            let (parent, read, committed) = commit_change(&instance).await;
            cut_short(&instance, committed, parent, read).await;
            let repository = &instance.repository;
            let (branch_key, _) = anchor_key(
                repository.salt(),
                ANCHOR_CURRENT_BRANCH,
                repository.instance_id,
            );
            let committing = failing_load_of(&instance, branch_key);

            commit::commit_boxed(
                committing,
                &instance.write_token,
                CommitOptions::new(String::from("Third")),
            )
            .await
            .expect_err("The current branch cannot be read");
            assert_eq!(intent(&instance).await, committed);
        },
    )
    .await;
}

/// A commit intent that cannot be read fails the commit, rather than reading as no intent.
#[tokio::test]
async fn an_intent_that_cannot_be_read_fails_the_commit() {
    run(
        "interrupted-commit-unreadable-intent",
        &TREE,
        |instance| async move {
            write(&instance, "a.txt", "changed");
            stage(&instance, &instance.path).await;
            let keys = Keys::of(&instance);
            let committing = failing_load_of(&instance, keys.intent.0);

            commit::commit_boxed(
                committing,
                &instance.write_token,
                CommitOptions::new(String::from("Second")),
            )
            .await
            .expect_err("The commit intent cannot be read");
        },
    )
    .await;
}

/// One write of a key a [`FlushLog`] watches: the value written, and what the store's files held
/// of every key it watches when it was written.
struct Write {
    key: Hash,
    value: Hash,
    on_disk: HashMap<Hash, Hash>,
}

/// What a store's files hold of the keys it watches, taking a write at the next flush as the
/// local store's files do, and each write of a watched key.
struct FlushLog {
    written: parking_lot::Mutex<HashMap<Hash, Hash>>,
    on_disk: parking_lot::Mutex<HashMap<Hash, Hash>>,
    writes: parking_lot::Mutex<Vec<Write>>,
}

impl FlushLog {
    fn record(&self, key: Hash, value: Hash) {
        let mut written = self.written.lock();
        if let Some(slot) = written.get_mut(&key) {
            *slot = value;
            self.writes.lock().push(Write {
                key,
                value,
                on_disk: self.on_disk.lock().clone(),
            });
        }
    }

    fn flushed(&self) {
        let written = self.written.lock().clone();
        *self.on_disk.lock() = written;
    }

    /// What the files held of every watched key when `key` was first written `value`.
    fn on_disk_at(&self, key: Hash, value: Hash) -> HashMap<Hash, Hash> {
        self.writes
            .lock()
            .iter()
            .find(|write| write.key == key && write.value == value)
            .map(|write| write.on_disk.clone())
            .expect("The key was written the value")
    }

    /// The value `key` was last written, and what the files held of every watched key then.
    fn on_disk_at_last(&self, key: Hash) -> (Hash, HashMap<Hash, Hash>) {
        self.writes
            .lock()
            .iter()
            .rev()
            .find(|write| write.key == key)
            .map(|write| (write.value, write.on_disk.clone()))
            .expect("The key was written")
    }
}

/// The keys of an instance's commit intent, current anchor and staged anchor, and of its main
/// branch's latest, with their key types.
struct Keys {
    intent: (Hash, KeyType),
    current: (Hash, KeyType),
    staged: (Hash, KeyType),
    latest: (Hash, KeyType),
}

impl Keys {
    fn of(instance: &Instance) -> Self {
        let repository = &instance.repository;
        let salt = repository.salt();
        let anchor = |function| anchor_key(salt, function, repository.instance_id);
        Self {
            intent: anchor(ANCHOR_COMMITTING),
            current: anchor(ANCHOR_CURRENT),
            staged: anchor(ANCHOR_STAGED),
            latest: branch::mutable_key(salt, branch::LATEST, repository.id, instance.main),
        }
    }
}

/// A context of `instance`'s repository over `store`, a store over its mutable store.
fn over(instance: &Instance, store: Arc<HookedStore>) -> Arc<RepositoryContext> {
    let repository = &instance.repository;
    Arc::new(
        RepositoryContext::new(
            default_repository_creation_args(repository.immutable_store(), store)
                .with_path(&instance.path)
                .with_id(repository.id)
                .with_instance_id(repository.instance_id),
        )
        .with_write_token(instance.write_token.share()),
    )
}

fn base_store(instance: &Instance) -> Arc<dyn MutableStore> {
    instance
        .repository
        .try_mutable_store_arc()
        .expect("A writable repository")
}

/// A log of what `instance`'s mutable store holds on disk of `keys`, and a context of the
/// instance's repository over a store that keeps it.
async fn over_store(
    instance: &Instance,
    keys: &[(Hash, KeyType)],
) -> (Arc<FlushLog>, Arc<RepositoryContext>) {
    let base = base_store(instance);
    let mut held = HashMap::new();
    for &(key, key_type) in keys {
        let value = base
            .clone()
            .load(instance.repository.id, key, key_type)
            .await
            .unwrap_or_default();
        held.insert(key, value);
    }
    let log = Arc::new(FlushLog {
        written: parking_lot::Mutex::new(held.clone()),
        on_disk: parking_lot::Mutex::new(held),
        writes: parking_lot::Mutex::default(),
    });
    let recording = log.clone();
    let flushing = log.clone();
    let store = HookedStore::wrap_flushing(
        base,
        |_, _, _| Ok(()),
        answered(move |inner, partition, call, key, key_type| {
            let log = recording.clone();
            async move {
                if matches!(call, StoreCall::Store | StoreCall::CompareAndSwap)
                    && log.written.lock().contains_key(&key)
                {
                    // A key stored zero is removed, and reads as absent.
                    let value = match inner.load(partition, key, key_type).await {
                        Err(err) if err.is_address_not_found() => Hash::default(),
                        loaded => loaded?,
                    };
                    log.record(key, value);
                }
                Ok(())
            }
        }),
        move || flushing.flushed(),
    );
    (log, over(instance, store))
}

/// A context of `instance`'s repository over a store whose loads of `key` fail, as a store that
/// cannot read it fails them.
fn failing_load_of(instance: &Instance, key: Hash) -> Arc<RepositoryContext> {
    let store = HookedStore::wrap(
        base_store(instance),
        move |call, loaded, _| match call {
            StoreCall::Load if loaded == key => Err(StoreError::internal("The key cannot be read")),
            _ => Ok(()),
        },
        None,
    );
    over(instance, store)
}

/// A commit has its intent on disk before it swaps the branch latest, its branch latest on disk
/// before it writes the current anchor, and its current and staged anchors on disk before it
/// clears the intent.
#[tokio::test]
async fn a_commit_writes_its_anchors_only_once_its_branch_latest_is_on_disk() {
    run("interrupted-commit-order", &TREE, |instance| async move {
        write(&instance, "a.txt", "changed");
        stage(&instance, &instance.path).await;
        let keys = Keys::of(&instance);
        let (store, committing) = over_store(
            &instance,
            &[keys.intent, keys.current, keys.staged, keys.latest],
        )
        .await;

        let signature = commit::commit_boxed(
            committing,
            &instance.write_token,
            CommitOptions::new(String::from("Second")),
        )
        .await
        .expect("The commit succeeds");

        assert_eq!(
            store.on_disk_at(keys.latest.0, signature)[&keys.intent.0],
            signature
        );
        assert_eq!(
            store.on_disk_at(keys.current.0, signature)[&keys.latest.0],
            signature
        );
        let (cleared, on_disk) = store.on_disk_at_last(keys.intent.0);
        assert_eq!(cleared, Hash::default());
        assert_eq!(on_disk[&keys.current.0], signature);
        assert_eq!(
            on_disk[&keys.staged.0],
            store.written.lock()[&keys.staged.0],
            "The intent was cleared before the staged anchor reached disk"
        );
    })
    .await;
}

/// A write of the current anchor that finds a commit intent clears it on disk before it writes
/// the anchor.
#[tokio::test]
async fn a_current_anchor_write_clears_the_commit_intent_on_disk_first() {
    run("interrupted-commit-cleared", &TREE, |instance| async move {
        let (parent, _staged, committed) = commit_change(&instance).await;
        let keys = Keys::of(&instance);
        let (store, moving) = over_store(&instance, &[keys.intent, keys.current]).await;
        let hooked = moving
            .try_mutable_store_arc()
            .expect("A writable repository");
        hooked
            .clone()
            .store(
                instance.repository.id,
                keys.intent.0,
                committed,
                keys.intent.1,
            )
            .await
            .expect("Failed to store the intent");
        hooked
            .flush(FlushOptions::default())
            .await
            .expect("Failed to flush the intent");

        lore_revision::instance::store_current_anchor(&moving, parent)
            .await
            .expect("Failed to store the current anchor");

        assert_eq!(
            store.on_disk_at(keys.current.0, parent)[&keys.intent.0],
            Hash::default()
        );
    })
    .await;
}

/// A link the interrupted commit committed, staged with the same pin, is the commit's own; one
/// staged with another pin was staged after it.
#[tokio::test]
async fn a_staged_link_is_the_commits_own_only_with_the_pin_it_committed() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable, mutable, _execution) =
                test_store_create().await.expect("making test stores");
            let repository = in_memory::repository(immutable, mutable);
            let link_repository = RepositoryId::from(uuid::Uuid::now_v7());
            let pin = in_memory::linked_revision(&repository, link_repository).await;
            let linking = async |pin| {
                let state = State::new();
                in_memory::add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    in_memory::link_node("lib", link_repository, pin),
                    "lib",
                )
                .await;
                state
            };
            let staged = linking(pin).await;
            let committed = in_memory::revision_state(
                &repository,
                in_memory::commit(
                    &repository,
                    &staged,
                    Hash::default(),
                    in_memory::branch_id(),
                )
                .await,
            )
            .await;
            assert!(
                commit::staged_by_the_commit(
                    &repository,
                    &committed,
                    Hash::default(),
                    Some(&staged)
                )
                .await
                .expect("Failed to compare the staged state"),
                "The link staged with the pin the commit committed is its own"
            );

            let repinned =
                linking(in_memory::linked_revision(&repository, link_repository).await).await;
            assert!(
                !commit::staged_by_the_commit(
                    &repository,
                    &committed,
                    Hash::default(),
                    Some(&repinned)
                )
                .await
                .expect("Failed to compare the staged state"),
                "A link staged with another pin was staged after the commit"
            );
        })
        .await;
}
