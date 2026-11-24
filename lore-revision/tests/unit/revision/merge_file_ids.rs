// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! `lore branch merge`, the merge a diverged sync runs, cherry-pick, the server's fast-forward
//! merge and `lore branch diff` of a side that deleted a file and added it again with the same
//! content, which makes the added file a new one with a file ID of its own. The added file stands
//! where the other side left the file alone, and gives way to any change the other side made of it.
//! A file ID a side holds only because an earlier merge kept it is no change of that side.

use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::branch::BranchLatestStatus;
use lore_revision::diff::diff_revision_paths;
use lore_revision::lore::RepositoryId;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision::diff3_collect;
use lore_revision::state::State;
use lore_revision::util::collect_stream::collect_stream_with_summary;

use super::fast_forward_merge::fast_forward_merge;
use super::instance::Edit;
use super::instance::Instance;
use super::instance::cherry_pick;
use super::instance::commit_edits;
use super::instance::create_branch;
use super::instance::current;
use super::instance::exclude;
use super::instance::merge_branch;
use super::instance::node_at;
use super::instance::run;
use super::instance::start_merge;
use super::instance::switch_to;
use crate::fs::filesystem_provider::setup_test_execution;
use crate::fs::filesystem_provider::test_store_create;
use crate::helper::in_memory;
use crate::helper::in_memory::add;
use crate::helper::in_memory::branch_id;
use crate::helper::in_memory::file;
use crate::helper::in_memory::link_node;
use crate::helper::in_memory::linked_revision;

/// The files every fixture starts from, each holding its own path.
const TREE: [&str; 4] = ["Dir/a.txt", "Dir/Sub/c.txt", "Dir/Sub/d.txt", "Top.txt"];

/// The file the main branch deletes and adds again with the content it held.
const ADDED_AGAIN: &str = "Dir/Sub/c.txt";

/// Where the merge finds the file the main branch added again.
#[derive(Clone, Copy, Debug)]
enum Placement {
    /// In the working tree.
    OnDisk,
    /// Outside the view, which keeps `Dir/Sub` out of the working tree.
    OutOfView,
}

/// The paths of the changes a diff of the revisions `from` and `to` reports, of the whole tree.
async fn changed_paths(repository: &Arc<RepositoryContext>, from: Hash, to: Hash) -> Vec<String> {
    let (from, to) = (
        State::deserialize(repository.clone(), from)
            .await
            .expect("Failed to read the revision"),
        State::deserialize(repository.clone(), to)
            .await
            .expect("Failed to read the revision"),
    );
    let repository = repository.clone();
    let (_, changes) =
        collect_stream_with_summary(|tx| diff_revision_paths(repository, from, to, None, tx))
            .await
            .expect("Failed to diff the revisions");
    changes
        .iter()
        .map(|change| change.path().as_str().to_string())
        .collect()
}

/// What the side a change is joined onto made of [`ADDED_AGAIN`].
#[derive(Clone, Copy, Debug)]
enum Other {
    /// Nothing: it edited the file at the path given instead.
    LeavesIt(&'static str),
    /// Edited it.
    EditsIt,
    /// Deleted it and added it again with the same content.
    AddsItAgain,
}

impl Other {
    /// Commits the change on the current branch, answering the revision.
    async fn commit(self, instance: &Instance) -> Hash {
        match self {
            Other::LeavesIt(path) => {
                commit_edits(instance, &[Edit::Write(path, "other")], "Other").await
            }
            Other::EditsIt => {
                commit_edits(instance, &[Edit::Write(ADDED_AGAIN, "edited")], "Edit").await
            }
            Other::AddsItAgain => delete_and_add_again(instance).await,
        }
    }

    /// The file a join holds at [`ADDED_AGAIN`]: `incoming`, the file the joined side added
    /// again, where this side left the path alone, and `own`, the file this side holds, where it
    /// changed it.
    fn joined(self, incoming: Address, own: Address) -> Address {
        match self {
            Other::LeavesIt(_) => incoming,
            Other::EditsIt | Other::AddsItAgain => own,
        }
    }
}

/// Deletes [`ADDED_AGAIN`] and adds it again with the content it held, in two revisions on the
/// current branch, and answers the second.
async fn delete_and_add_again(instance: &Instance) -> Hash {
    let deleted_from = current(instance).await;
    commit_edits(instance, &[Edit::Delete(ADDED_AGAIN)], "Delete").await;
    let added = commit_edits(
        instance,
        &[Edit::Write(ADDED_AGAIN, ADDED_AGAIN)],
        "Add again",
    )
    .await;
    let (old, new) = (
        node_at(instance, deleted_from, ADDED_AGAIN).await.address,
        node_at(instance, added, ADDED_AGAIN).await.address,
    );
    assert_eq!(old.hash, new.hash, "the same content");
    assert_ne!(old.context, new.context, "a new file ID");
    added
}

/// Merges the main branch, which deleted [`ADDED_AGAIN`] and then added it again with the same
/// content, into a branch making `other`'s change, with the file where `placement` puts it, and
/// checks that the merge revision holds the file [`Other::joined`] names. Where it holds the added
/// file, a diff of the main branch's latest and the merge reports no change of it.
async fn merge_of_a_file_added_again(name: &str, placement: Placement, other: Other) {
    run(name, &TREE, move |mut instance| async move {
        let unfiltered = instance.repository.clone();
        create_branch(&instance, "feature").await;
        let feature = other.commit(&instance).await;
        switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
        let main = delete_and_add_again(&instance).await;
        switch_to(&instance, "feature").await;
        if let Placement::OutOfView = placement {
            exclude(&mut instance, "Dir/Sub");
        }

        assert!(
            merge_branch(&instance, branch::DEFAULT_DEFAULT_NAME).await,
            "{other:?}: the merge did not commit"
        );

        let merge = current(&instance).await;
        let (incoming, own) = (
            node_at(&instance, main, ADDED_AGAIN).await.address,
            node_at(&instance, feature, ADDED_AGAIN).await.address,
        );
        assert_eq!(
            node_at(&instance, merge, ADDED_AGAIN).await.address,
            other.joined(incoming, own),
            "{other:?}"
        );
        if let Other::LeavesIt(_) = other {
            let changed = changed_paths(&unfiltered, main, merge).await;
            assert!(
                !changed.iter().any(|path| path == ADDED_AGAIN),
                "the diff of main and the merge reports {changed:?}"
            );
        }
    })
    .await;
}

/// A branch that never changed a file in the working tree takes the file the merged branch added
/// again.
#[tokio::test(flavor = "multi_thread")]
async fn a_merge_takes_a_file_added_again_on_disk() {
    merge_of_a_file_added_again(
        "a_merge_takes_a_file_added_again_on_disk",
        Placement::OnDisk,
        Other::LeavesIt("Top.txt"),
    )
    .await;
}

/// A branch that changed nothing in a directory outside the view takes the file the merged branch
/// added again there.
#[tokio::test(flavor = "multi_thread")]
async fn a_merge_takes_a_file_added_again_out_of_view() {
    merge_of_a_file_added_again(
        "a_merge_takes_a_file_added_again_out_of_view",
        Placement::OutOfView,
        Other::LeavesIt("Top.txt"),
    )
    .await;
}

/// A branch that changed another file in a directory outside the view takes the file the merged
/// branch added again there.
#[tokio::test(flavor = "multi_thread")]
async fn a_merge_takes_a_file_added_again_out_of_view_beside_an_edit() {
    merge_of_a_file_added_again(
        "a_merge_takes_a_file_added_again_out_of_view_beside_an_edit",
        Placement::OutOfView,
        Other::LeavesIt("Dir/Sub/d.txt"),
    )
    .await;
}

/// A branch that edited a file keeps its edit and its file against the merged branch's delete and
/// add of the file with the content it held, with no conflict, on disk and outside the view.
#[tokio::test(flavor = "multi_thread")]
async fn a_merge_keeps_an_edit_against_a_file_added_again() {
    for placement in [Placement::OnDisk, Placement::OutOfView] {
        merge_of_a_file_added_again(
            "a_merge_keeps_an_edit_against_a_file_added_again",
            placement,
            Other::EditsIt,
        )
        .await;
    }
}

/// A branch that deleted a file and added it again, as the merged branch did, keeps its own file,
/// on disk and outside the view.
#[tokio::test(flavor = "multi_thread")]
async fn a_merge_keeps_its_own_file_added_again() {
    for placement in [Placement::OnDisk, Placement::OutOfView] {
        merge_of_a_file_added_again(
            "a_merge_keeps_its_own_file_added_again",
            placement,
            Other::AddsItAgain,
        )
        .await;
    }
}

/// A merge into the main branch, which deleted a file and added it again with the content it held,
/// of a branch that edited the file takes the edit and the branch's file, on disk and outside the
/// view.
#[tokio::test(flavor = "multi_thread")]
async fn a_merge_of_an_edit_takes_it_against_a_file_added_again() {
    for placement in [Placement::OnDisk, Placement::OutOfView] {
        run(
            "a_merge_of_an_edit_takes_it_against_a_file_added_again",
            &TREE,
            move |mut instance| async move {
                create_branch(&instance, "feature").await;
                let feature = Other::EditsIt.commit(&instance).await;
                switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
                delete_and_add_again(&instance).await;
                if let Placement::OutOfView = placement {
                    exclude(&mut instance, "Dir/Sub");
                }

                assert!(
                    merge_branch(&instance, "feature").await,
                    "{placement:?}: the merge did not commit"
                );

                let merge = current(&instance).await;
                assert_eq!(
                    node_at(&instance, merge, ADDED_AGAIN).await.address,
                    node_at(&instance, feature, ADDED_AGAIN).await.address,
                    "{placement:?}"
                );
            },
        )
        .await;
    }
}

/// Commits on `branch`, the current branch, a revision whose only change replaces [`ADDED_AGAIN`]
/// with a new file holding the same content, and answers it: a branch made from it deletes the
/// file and adds it again, and `branch` cherry-picks the add.
async fn commit_replacement(instance: &Instance, branch: &str) -> Hash {
    let parent = current(instance).await;
    create_branch(instance, "added-again").await;
    let added = delete_and_add_again(instance).await;
    switch_to(instance, branch).await;
    let replacement = cherry_pick(instance, added, "Replace").await;
    let (old, new) = (
        node_at(instance, parent, ADDED_AGAIN).await.address,
        node_at(instance, replacement, ADDED_AGAIN).await.address,
    );
    assert_eq!(old.hash, new.hash, "the same content");
    assert_ne!(old.context, new.context, "a new file ID");
    replacement
}

/// A cherry-pick of a revision that replaced a file with a new one holding the same content, onto a
/// branch making `other`'s change, commits the file [`Other::joined`] names.
async fn cherry_pick_of_a_file_added_again(name: &str, other: Other) {
    run(name, &TREE, move |instance| async move {
        create_branch(&instance, "replaced").await;
        let replacement = commit_replacement(&instance, "replaced").await;
        switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
        let main = other.commit(&instance).await;

        let picked = cherry_pick(&instance, replacement, "Pick").await;

        let (incoming, own) = (
            node_at(&instance, replacement, ADDED_AGAIN).await.address,
            node_at(&instance, main, ADDED_AGAIN).await.address,
        );
        assert_eq!(
            node_at(&instance, picked, ADDED_AGAIN).await.address,
            other.joined(incoming, own),
            "{other:?}"
        );
    })
    .await;
}

/// A cherry-pick onto a branch that never changed the file takes the new file.
#[tokio::test(flavor = "multi_thread")]
async fn a_cherry_pick_takes_a_file_added_again() {
    cherry_pick_of_a_file_added_again(
        "a_cherry_pick_takes_a_file_added_again",
        Other::LeavesIt("Top.txt"),
    )
    .await;
}

/// A cherry-pick onto a branch that edited the file, or deleted it and added it again, keeps that
/// branch's file.
#[tokio::test(flavor = "multi_thread")]
async fn a_cherry_pick_keeps_a_change_against_a_file_added_again() {
    for other in [Other::EditsIt, Other::AddsItAgain] {
        cherry_pick_of_a_file_added_again(
            "a_cherry_pick_keeps_a_change_against_a_file_added_again",
            other,
        )
        .await;
    }
}

/// The server's fast-forward merge of a pushed revision that replaced a file with a new one holding
/// the same content, onto a latest making `other`'s change, stores the file [`Other::joined`]
/// names.
async fn fast_forward_merge_of_a_file_added_again(name: &str, other: Other) {
    run(name, &TREE, move |instance| async move {
        let base = current(&instance).await;
        create_branch(&instance, "feature").await;
        let replacement = commit_replacement(&instance, "feature").await;
        switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
        let latest = other.commit(&instance).await;

        let merged = fast_forward_merge(&instance, base, replacement, latest, &[]).await;

        let (incoming, own) = (
            node_at(&instance, replacement, ADDED_AGAIN).await.address,
            node_at(&instance, latest, ADDED_AGAIN).await.address,
        );
        assert_eq!(
            node_at(&instance, merged, ADDED_AGAIN).await.address,
            other.joined(incoming, own),
            "{other:?}"
        );
    })
    .await;
}

/// A fast-forward merge onto a latest that never changed the file takes the new file.
#[tokio::test(flavor = "multi_thread")]
async fn a_fast_forward_merge_takes_a_file_added_again() {
    fast_forward_merge_of_a_file_added_again(
        "a_fast_forward_merge_takes_a_file_added_again",
        Other::LeavesIt("Top.txt"),
    )
    .await;
}

/// Merges `source` into `target`, the latest of `branch`, both made on `base`, as a client that
/// dropped every change of a file's ID alone did: [`ADDED_AGAIN`] keeps the file `target` holds.
/// Stores the merge as the latest of `branch` and answers it.
async fn old_client_merge(
    instance: &Instance,
    branch: &str,
    base: Hash,
    source: Hash,
    target: Hash,
) -> Hash {
    let repository = instance.repository.clone();
    let merged = fast_forward_merge(instance, base, source, target, &[ADDED_AGAIN]).await;
    assert_eq!(
        node_at(instance, merged, ADDED_AGAIN).await.address,
        node_at(instance, target, ADDED_AGAIN).await.address,
        "the merge keeps the file of the branch merged into"
    );
    let id = branch::resolve(repository.clone(), branch)
        .await
        .expect("Failed to resolve the branch")
        .id;
    branch::store_latest(
        repository,
        id,
        target,
        merged,
        BranchLatestStatus::Divergent,
    )
    .await
    .expect("Failed to store the merge");
    merged
}

/// Makes `feature` a branch that drifted from the main branch: it edits `Top.txt`, then merges the
/// main branch after that deleted [`ADDED_AGAIN`] and added it again, as an
/// [`old_client_merge`]. Answers the main branch's revision the drifted merge merged and the
/// drifted merge, with the instance on the main branch.
async fn drifted_feature(instance: &Instance) -> (Hash, Hash) {
    let base = current(instance).await;
    create_branch(instance, "feature").await;
    let feature = commit_edits(instance, &[Edit::Write("Top.txt", "feature")], "Feature").await;
    switch_to(instance, branch::DEFAULT_DEFAULT_NAME).await;
    let main = delete_and_add_again(instance).await;
    let drifted = old_client_merge(instance, "feature", base, main, feature).await;
    (main, drifted)
}

/// A merge into the main branch of a branch that drifted from it keeps the main branch's file: the
/// branch's old file ID is no change its own history made after the main branch replaced the file,
/// whether or not the main branch moved since the drifted merge.
#[tokio::test(flavor = "multi_thread")]
async fn a_merge_of_a_drifted_branch_keeps_the_merged_into_file() {
    for moved in [true, false] {
        run(
            "a_merge_of_a_drifted_branch_keeps_the_merged_into_file",
            &TREE,
            move |instance| async move {
                let (main, _) = drifted_feature(&instance).await;
                if moved {
                    commit_edits(&instance, &[Edit::Write("Dir/a.txt", "main")], "Main").await;
                }

                assert!(
                    merge_branch(&instance, "feature").await,
                    "moved {moved}: the merge did not commit"
                );

                assert_eq!(
                    node_at(&instance, current(&instance).await, ADDED_AGAIN)
                        .await
                        .address,
                    node_at(&instance, main, ADDED_AGAIN).await.address,
                    "moved {moved}"
                );
            },
        )
        .await;
    }
}

/// A merge of the main branch, which deleted a file and added it again a second time, into a branch
/// that drifted from it takes the main branch's new file: the branch's old file ID is no change of
/// its own, so the main branch's stands as where the branch left the file alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_drifted_branch_takes_a_file_added_again_once_more() {
    run(
        "a_drifted_branch_takes_a_file_added_again_once_more",
        &TREE,
        |instance| async move {
            drifted_feature(&instance).await;
            let main = delete_and_add_again(&instance).await;
            switch_to(&instance, "feature").await;

            assert!(
                merge_branch(&instance, branch::DEFAULT_DEFAULT_NAME).await,
                "the merge did not commit"
            );

            assert_eq!(
                node_at(&instance, current(&instance).await, ADDED_AGAIN)
                    .await
                    .address,
                node_at(&instance, main, ADDED_AGAIN).await.address
            );
        },
    )
    .await;
}

/// A 3-way diff measures a file below a link against the linked repository's base: where the
/// source's pin holds a file the linked repository's history kept, last changed in a revision of
/// the base pin's history, and the base pin deleted it and added it again, the diff lists no change
/// of the file.
#[tokio::test]
async fn a_file_id_kept_below_a_link_is_measured_against_the_linked_base() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (immutable, mutable, _execution) =
                test_store_create().await.expect("making test stores");
            let repository = in_memory::repository(immutable, mutable);
            let linked_id = RepositoryId::from(uuid::Uuid::now_v7());
            let linked = repository.to_link_context(linked_id).await;
            let first = linked_revision(&repository, linked_id).await;
            let edit = async |parent: Hash, edit: &dyn Fn(&Arc<State>) -> Option<&'static str>| {
                let state = in_memory::revision_state(&linked, parent).await;
                if let Some(name) = edit(&state) {
                    add(&state, linked.clone(), ROOT_NODE, file(name), name).await;
                }
                in_memory::commit(&linked, &state, parent, branch_id()).await
            };
            let deleted = {
                let state = in_memory::revision_state(&linked, first).await;
                let node = state
                    .find_node_link(linked.clone(), "l.txt")
                    .await
                    .expect("resolving the file")
                    .node;
                state
                    .node_delete(linked.clone(), node)
                    .await
                    .expect("deleting the file");
                in_memory::commit(&linked, &state, first, branch_id()).await
            };
            let added_again = edit(deleted, &|_| Some("l.txt")).await;
            let kept = edit(first, &|_| Some("k.txt")).await;
            let pinned = async |pin: Hash, other: bool| {
                let state = State::new();
                add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    link_node("Mount", linked_id, pin),
                    "Mount",
                )
                .await;
                if other {
                    add(
                        &state,
                        repository.clone(),
                        ROOT_NODE,
                        file("o.txt"),
                        "o.txt",
                    )
                    .await;
                }
                in_memory::commit(&repository, &state, Hash::default(), branch_id()).await
            };
            let base = pinned(added_again, false).await;
            let source = pinned(kept, false).await;
            let target = pinned(added_again, true).await;

            let diff = diff3_collect(repository.clone(), base, source, target, None, false)
                .await
                .expect("diffing the revisions");

            let paths: Vec<&str> = diff
                .changes
                .iter()
                .map(|change| change.path().as_str())
                .collect();
            assert!(paths.contains(&"Mount/k.txt"), "{paths:?}");
            assert!(!paths.contains(&"Mount/l.txt"), "{paths:?}");
        })
        .await;
}

/// The merge a diverged sync runs, of the main branch's latest that a fast-forward moved to a
/// drifted merge, into a local revision on the main branch, stages the local revision's file.
#[tokio::test(flavor = "multi_thread")]
async fn a_diverged_sync_onto_a_drifted_latest_stages_the_local_file() {
    run(
        "a_diverged_sync_onto_a_drifted_latest_stages_the_local_file",
        &TREE,
        |instance| async move {
            let (main, drifted) = drifted_feature(&instance).await;
            let local = commit_edits(&instance, &[Edit::Write("Dir/a.txt", "main")], "Main").await;
            branch::store_latest(
                instance.repository.clone(),
                instance.main,
                local,
                drifted,
                BranchLatestStatus::Convergent,
            )
            .await
            .expect("Failed to store the fetched latest");

            let staged = start_merge(&instance, branch::DEFAULT_DEFAULT_NAME, true)
                .await
                .expect("Failed to merge");

            assert_eq!(
                node_at(&instance, staged, ADDED_AGAIN).await.address,
                node_at(&instance, main, ADDED_AGAIN).await.address
            );
        },
    )
    .await;
}

/// Two branches that each deleted a file and added it again converge: a merge of the main branch
/// into the other keeps the other's file, and a merge of the other back into the main branch, which
/// moved since, takes it, as the other's history replaced the file after the base.
#[tokio::test(flavor = "multi_thread")]
async fn two_files_added_again_converge_on_a_merge_back() {
    run(
        "two_files_added_again_converge_on_a_merge_back",
        &TREE,
        |instance| async move {
            create_branch(&instance, "feature").await;
            let feature = delete_and_add_again(&instance).await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            delete_and_add_again(&instance).await;
            switch_to(&instance, "feature").await;
            assert!(
                merge_branch(&instance, branch::DEFAULT_DEFAULT_NAME).await,
                "the merge into feature did not commit"
            );
            let own = node_at(&instance, feature, ADDED_AGAIN).await.address;
            assert_eq!(
                node_at(&instance, current(&instance).await, ADDED_AGAIN)
                    .await
                    .address,
                own
            );
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            commit_edits(&instance, &[Edit::Write("Dir/a.txt", "main")], "Main").await;

            assert!(
                merge_branch(&instance, "feature").await,
                "the merge into main did not commit"
            );

            assert_eq!(
                node_at(&instance, current(&instance).await, ADDED_AGAIN)
                    .await
                    .address,
                own
            );
        },
    )
    .await;
}

/// A merge of the main branch into a branch that deleted a file and added it again keeps the
/// branch's file, where an [`old_client_merge`] of the branch into the main branch kept the main
/// branch's old file: the base is the branch's revision the main branch merged, and the old file
/// was last changed in a revision of its history.
#[tokio::test(flavor = "multi_thread")]
async fn a_merge_keeps_a_file_added_again_that_an_old_merge_of_it_left_out() {
    run(
        "a_merge_keeps_a_file_added_again_that_an_old_merge_of_it_left_out",
        &TREE,
        |instance| async move {
            let base = current(&instance).await;
            create_branch(&instance, "feature").await;
            let added = delete_and_add_again(&instance).await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            let main = commit_edits(&instance, &[Edit::Write("Dir/a.txt", "main")], "Main").await;
            old_client_merge(&instance, branch::DEFAULT_DEFAULT_NAME, base, added, main).await;
            switch_to(&instance, "feature").await;

            assert!(
                merge_branch(&instance, branch::DEFAULT_DEFAULT_NAME).await,
                "the merge did not commit"
            );

            assert_eq!(
                node_at(&instance, current(&instance).await, ADDED_AGAIN)
                    .await
                    .address,
                node_at(&instance, added, ADDED_AGAIN).await.address
            );
        },
    )
    .await;
}

/// As [`a_merge_keeps_a_file_added_again_that_an_old_merge_of_it_left_out`], where the main
/// branch's old file was last changed after the branch was made, in a revision the branch reaches
/// only through an earlier merge of the main branch: the base reaches it through that merge's
/// second parent, so the branch keeps its own file.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_last_changed_in_a_revision_merged_into_the_base_is_in_its_history() {
    run(
        "a_file_last_changed_in_a_revision_merged_into_the_base_is_in_its_history",
        &TREE,
        |instance| async move {
            create_branch(&instance, "feature").await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            commit_edits(&instance, &[Edit::Write(ADDED_AGAIN, "edited")], "Edit").await;
            let edited_back = commit_edits(
                &instance,
                &[Edit::Write(ADDED_AGAIN, ADDED_AGAIN)],
                "Edit back",
            )
            .await;
            switch_to(&instance, "feature").await;
            commit_edits(&instance, &[Edit::Write("Top.txt", "feature")], "Feature").await;
            assert!(
                merge_branch(&instance, branch::DEFAULT_DEFAULT_NAME).await,
                "the first merge did not commit"
            );
            let added = delete_and_add_again(&instance).await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            let main = commit_edits(&instance, &[Edit::Write("Dir/a.txt", "main")], "Main").await;
            old_client_merge(
                &instance,
                branch::DEFAULT_DEFAULT_NAME,
                edited_back,
                added,
                main,
            )
            .await;
            switch_to(&instance, "feature").await;

            assert!(
                merge_branch(&instance, branch::DEFAULT_DEFAULT_NAME).await,
                "the merge did not commit"
            );

            assert_eq!(
                node_at(&instance, current(&instance).await, ADDED_AGAIN)
                    .await
                    .address,
                node_at(&instance, added, ADDED_AGAIN).await.address
            );
        },
    )
    .await;
}

/// The paths of the changes `lore branch diff` reports of the branch `source` against `target`.
async fn branch_diff_paths(instance: &Instance, source: &str, target: &str) -> Vec<String> {
    let repository = instance.repository.clone();
    let side = async |name: &str| {
        let branch = branch::resolve(repository.clone(), name)
            .await
            .expect("Failed to resolve the branch");
        let latest = branch::load_latest(repository.clone(), branch.id)
            .await
            .expect("Failed to load the latest");
        (branch.id, latest)
    };
    let ((source_branch, source_latest), (target_branch, target_latest)) =
        (side(source).await, side(target).await);
    let diff = branch::diff3_collect(
        repository.clone(),
        source_branch,
        source_latest,
        target_branch,
        target_latest,
        None,
        false,
        false,
    )
    .await
    .expect("Failed to diff the branches");
    assert!(diff.conflicts.is_empty(), "{:?}", diff.conflicts);
    diff.changes
        .iter()
        .map(|change| change.path().as_str().to_string())
        .collect()
}

/// `lore branch diff` of a branch that drifted from the main branch lists only its own change, as
/// a merge of the branch into the main branch keeps the main branch's file.
#[tokio::test(flavor = "multi_thread")]
async fn a_branch_diff_of_a_drifted_branch_lists_only_its_own_change() {
    run(
        "a_branch_diff_of_a_drifted_branch_lists_only_its_own_change",
        &TREE,
        |instance| async move {
            drifted_feature(&instance).await;
            assert_eq!(
                branch_diff_paths(&instance, "feature", branch::DEFAULT_DEFAULT_NAME).await,
                ["Top.txt"]
            );
        },
    )
    .await;
}

/// `lore branch diff` of a branch that deleted a file and added it again lists the file, as a
/// merge of the branch into the main branch takes the added file.
#[tokio::test(flavor = "multi_thread")]
async fn a_branch_diff_lists_a_file_the_branch_added_again() {
    run(
        "a_branch_diff_lists_a_file_the_branch_added_again",
        &TREE,
        |instance| async move {
            create_branch(&instance, "feature").await;
            delete_and_add_again(&instance).await;
            assert_eq!(
                branch_diff_paths(&instance, "feature", branch::DEFAULT_DEFAULT_NAME).await,
                [ADDED_AGAIN]
            );
        },
    )
    .await;
}

/// After a merge of the main branch into a branch that drifted from it, `lore branch diff` of the
/// branch against the main branch lists only the branch's own change.
#[tokio::test(flavor = "multi_thread")]
async fn a_branch_diff_after_merging_main_into_a_drifted_branch_lists_only_its_own_change() {
    run(
        "a_branch_diff_after_merging_main_into_a_drifted_branch_lists_only_its_own_change",
        &TREE,
        |instance| async move {
            drifted_feature(&instance).await;
            commit_edits(&instance, &[Edit::Write("Dir/a.txt", "main")], "Main").await;
            switch_to(&instance, "feature").await;
            assert!(
                merge_branch(&instance, branch::DEFAULT_DEFAULT_NAME).await,
                "the merge did not commit"
            );

            assert_eq!(
                branch_diff_paths(&instance, "feature", branch::DEFAULT_DEFAULT_NAME).await,
                ["Top.txt"]
            );
        },
    )
    .await;
}

/// One merge measures each file against the base's history on its own: a branch holding one file
/// whose old file ID an old client merge kept, last changed in the base's history, and another it
/// deleted and added again before that merge, numbered below the base, merged into the main branch,
/// keeps the main branch's first file and takes the branch's second.
#[tokio::test(flavor = "multi_thread")]
async fn a_merge_measures_each_file_added_again_against_the_base_history() {
    const ADDED_ON_FEATURE: &str = "Dir/Sub/d.txt";
    run(
        "a_merge_measures_each_file_added_again_against_the_base_history",
        &TREE,
        |instance| async move {
            let base = current(&instance).await;
            create_branch(&instance, "feature").await;
            commit_edits(&instance, &[Edit::Delete(ADDED_ON_FEATURE)], "Delete").await;
            let feature = commit_edits(
                &instance,
                &[Edit::Write(ADDED_ON_FEATURE, ADDED_ON_FEATURE)],
                "Add again",
            )
            .await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            for edit in ["one", "two", "three"] {
                commit_edits(&instance, &[Edit::Write("Dir/a.txt", edit)], "Main").await;
            }
            let main = delete_and_add_again(&instance).await;
            old_client_merge(&instance, "feature", base, main, feature).await;
            commit_edits(&instance, &[Edit::Write("Top.txt", "main")], "Main").await;

            assert!(
                merge_branch(&instance, "feature").await,
                "the merge did not commit"
            );

            let merged = current(&instance).await;
            assert_eq!(
                node_at(&instance, merged, ADDED_AGAIN).await.address,
                node_at(&instance, main, ADDED_AGAIN).await.address,
                "the old file ID the merge kept"
            );
            assert_eq!(
                node_at(&instance, merged, ADDED_ON_FEATURE).await.address,
                node_at(&instance, feature, ADDED_ON_FEATURE).await.address,
                "the file the branch added again"
            );
        },
    )
    .await;
}

/// The merge a diverged sync runs, of the main branch's latest the sync fetched, which deleted
/// [`ADDED_AGAIN`] and added it again, into a local revision on the main branch making `other`'s
/// change, stages the file [`Other::joined`] names.
async fn diverged_sync_of_a_file_added_again(name: &str, other: Other) {
    run(name, &TREE, move |instance| async move {
        create_branch(&instance, "upstream").await;
        let fetched = delete_and_add_again(&instance).await;
        switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
        let local = other.commit(&instance).await;
        branch::store_latest(
            instance.repository.clone(),
            instance.main,
            local,
            fetched,
            BranchLatestStatus::Convergent,
        )
        .await
        .expect("Failed to store the fetched latest");

        let staged = start_merge(&instance, branch::DEFAULT_DEFAULT_NAME, true)
            .await
            .expect("Failed to merge");

        let (incoming, own) = (
            node_at(&instance, fetched, ADDED_AGAIN).await.address,
            node_at(&instance, local, ADDED_AGAIN).await.address,
        );
        assert_eq!(
            node_at(&instance, staged, ADDED_AGAIN).await.address,
            other.joined(incoming, own),
            "{other:?}"
        );
    })
    .await;
}

/// The merge of a diverged sync takes the fetched file added again where the local revision left
/// the file alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_diverged_sync_takes_a_file_added_again() {
    diverged_sync_of_a_file_added_again(
        "a_diverged_sync_takes_a_file_added_again",
        Other::LeavesIt("Top.txt"),
    )
    .await;
}

/// The merge of a diverged sync keeps a local edit of the file, or a local delete and add of it,
/// against the fetched delete and add of the file with the content it held.
#[tokio::test(flavor = "multi_thread")]
async fn a_diverged_sync_keeps_a_local_change_against_a_file_added_again() {
    for other in [Other::EditsIt, Other::AddsItAgain] {
        diverged_sync_of_a_file_added_again(
            "a_diverged_sync_keeps_a_local_change_against_a_file_added_again",
            other,
        )
        .await;
    }
}
