// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! The history a 3-way diff reads for its conflicts: whether a merge joins its two sides above the
//! base, and the walk of a history line for where each of the paths its first revision lacks was
//! last held. Also the deletes of directories the diff drops for its conflicts.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::change::FileAction;
use lore_revision::change::NodeChange;
use lore_revision::lore::RepositoryId;
use lore_revision::node::Node;
use lore_revision::node::ROOT_NODE;
use lore_revision::revision;
use lore_revision::revision::LinkSettlement;
use lore_revision::revision::MergesAcross;
use lore_revision::revision::deletions::Deletions;
use lore_revision::revision::deletions::InOrder;
use lore_revision::revision::deletions::LastHeld;
use lore_revision::revision::deletions::LineWalks;
use lore_revision::revision::deletions::walk_line;
use lore_revision::state::State;

use super::instance::Edit;
use super::instance::Instance;
use super::instance::commit_edits;
use super::instance::create_branch;
use super::instance::current;
use super::instance::merge_branch;
use super::instance::revision_state;
use super::instance::run;
use super::instance::switch_to;
use super::stale_directory;
use super::stale_directory::Revisions;
use crate::branch::write_line;
use crate::helper::in_memory;
use crate::helper::in_memory::add;
use crate::helper::in_memory::directory;
use crate::helper::in_memory::file;
use crate::helper::in_memory::link_node;
use crate::helper::setup_test_execution;
use crate::paged_remote::anchors;
use crate::paged_remote::branch_metadata;
use crate::paged_remote::client_on;
use crate::paged_remote::client_stores;
use crate::paged_remote::counted_client_stores;
use crate::paged_remote::paging_remote;
use crate::paged_remote::remote_line;
use crate::paged_remote::signature_of;

/// The files every fixture starts from, each holding its own path.
const TREE: [&str; 6] = [
    "Dir/a.txt",
    "Dir/b.txt",
    "Dir/Deep/c.txt",
    "Gone/d.txt",
    "Other/e.txt",
    "Top.txt",
];

/// More files than a comparison looks up at once, in more directories than it does.
static MANY: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    (0..80)
        .map(|index| &*Box::leak(format!("Many/Dir{}/f{index}.txt", index % 70).into_boxed_str()))
        .collect()
});

/// Where `revision` names `path`, and its revision number.
async fn held(instance: &Instance, revision: Hash, path: &str) -> (u64, u32) {
    let state = revision_state(instance, revision).await;
    let link = state
        .find_node_link(instance.repository.clone(), path)
        .await
        .expect("The revision holds the path");
    (state.revision_number(), link.node)
}

/// Walks the line from `start` for `paths` down to `bound`, with `window` comparisons in flight.
async fn walk(
    instance: &Instance,
    start: Hash,
    paths: &[&str],
    bound: u64,
    window: usize,
) -> BTreeMap<String, Option<LastHeld>> {
    walk_line(
        instance.repository.clone(),
        revision_state(instance, start).await,
        paths.iter().map(|path| path.to_string()).collect(),
        bound,
        &LineWalks::new(window),
    )
    .await
    .expect("The line can be read")
}

/// What the walk answers for a path last held in `held_in` and deleted in `removed_in`.
async fn last_held(instance: &Instance, held_in: Hash, removed_in: Hash, path: &str) -> LastHeld {
    let (held_number, node) = held(instance, held_in, path).await;
    LastHeld {
        removed_in,
        removed_in_number: revision_state(instance, removed_in).await.revision_number(),
        held_number,
        node,
    }
}

/// A file deleted, added again and deleted once more is found where the line last held it, not
/// where it was first deleted.
#[tokio::test]
async fn the_walk_finds_the_newest_revision_that_deleted_a_path() {
    run(
        "the_walk_finds_the_newest_revision_that_deleted_a_path",
        &TREE,
        |instance| async move {
            let first = current(&instance).await;
            commit_edits(&instance, &[Edit::Delete("Dir/a.txt")], "Delete").await;
            commit_edits(&instance, &[Edit::Write("Dir/a.txt", "again")], "Add again").await;
            let held_in =
                commit_edits(&instance, &[Edit::Write("Other/e.txt", "e")], "Other").await;
            let removed_in =
                commit_edits(&instance, &[Edit::Delete("Dir/a.txt")], "Delete again").await;
            let tip = commit_edits(&instance, &[Edit::Write("Top.txt", "top")], "Top").await;
            let bound = revision_state(&instance, first).await.revision_number();

            let found = walk(&instance, tip, &["Dir/a.txt"], bound, 4).await;

            assert_eq!(
                found,
                BTreeMap::from([(
                    "Dir/a.txt".to_string(),
                    Some(last_held(&instance, held_in, removed_in, "Dir/a.txt").await)
                )])
            );
        },
    )
    .await;
}

/// The walk checks the first revision numbered at or below the bound, and nothing older.
#[tokio::test]
async fn the_walk_ends_at_the_first_revision_at_or_below_the_bound() {
    run(
        "the_walk_ends_at_the_first_revision_at_or_below_the_bound",
        &TREE,
        |instance| async move {
            let first = current(&instance).await;
            let removed_in = commit_edits(&instance, &[Edit::Delete("Dir/b.txt")], "Delete").await;
            let tip = commit_edits(&instance, &[Edit::Write("Top.txt", "top")], "Top").await;
            let first_number = revision_state(&instance, first).await.revision_number();
            let removed_number = revision_state(&instance, removed_in)
                .await
                .revision_number();

            let at_first = walk(&instance, tip, &["Dir/b.txt"], first_number, 4).await;
            let above_first = walk(&instance, tip, &["Dir/b.txt"], removed_number, 4).await;

            assert_eq!(
                at_first["Dir/b.txt"],
                Some(last_held(&instance, first, removed_in, "Dir/b.txt").await),
                "The revision at the bound is checked"
            );
            assert_eq!(
                above_first["Dir/b.txt"], None,
                "The walk stops at the revision that deleted the file, the first at the bound"
            );
        },
    )
    .await;
}

/// A file added below a staged directory that keeps the address of the directory in its first
/// parent, and deleted in the revision after it, which holds that address clean, is found where
/// the line last held it.
#[tokio::test]
async fn the_walk_finds_a_path_below_a_staged_directory_with_a_stale_address() {
    run(
        "the_walk_finds_a_path_below_a_staged_directory_with_a_stale_address",
        &stale_directory::TREE,
        |instance| async move {
            let Revisions { stale, deleted } = stale_directory::revisions(&instance).await;
            let bound = revision_state(&instance, stale).await.revision_number();

            let found = walk(&instance, deleted, &["Dir/x.txt"], bound, 4).await;

            assert_eq!(
                found["Dir/x.txt"],
                Some(last_held(&instance, stale, deleted, "Dir/x.txt").await)
            );
        },
    )
    .await;
}

/// One walk for many paths finds each where the line last held it, with fewer comparisons in
/// flight than the line has revisions: files deleted in different revisions, a file deleted with
/// its whole directory, a file the start revision still holds, and a path the line never held.
#[tokio::test]
async fn one_walk_finds_each_path_where_the_line_last_held_it() {
    run(
        "one_walk_finds_each_path_where_the_line_last_held_it",
        &TREE,
        |instance| async move {
            let first = current(&instance).await;
            let c_removed =
                commit_edits(&instance, &[Edit::Delete("Dir/Deep/c.txt")], "Delete c").await;
            let other = commit_edits(&instance, &[Edit::Write("Other/e.txt", "e")], "Other").await;
            let gone_removed =
                commit_edits(&instance, &[Edit::Delete("Gone")], "Delete a directory").await;
            let a_removed = commit_edits(
                &instance,
                &[Edit::Delete("Dir/a.txt"), Edit::Write("Dir/b.txt", "b")],
                "Delete a",
            )
            .await;
            let tip = commit_edits(&instance, &[Edit::Write("Top.txt", "top")], "Top").await;
            let bound = revision_state(&instance, first).await.revision_number();
            let paths = [
                "Dir/a.txt",
                "Dir/Deep/c.txt",
                "Gone/d.txt",
                "Dir/b.txt",
                "Never/there.txt",
            ];

            let found = walk(&instance, tip, &paths, bound, 2).await;

            let expected = BTreeMap::from([
                (
                    "Dir/a.txt".to_string(),
                    Some(last_held(&instance, gone_removed, a_removed, "Dir/a.txt").await),
                ),
                (
                    "Dir/Deep/c.txt".to_string(),
                    Some(last_held(&instance, first, c_removed, "Dir/Deep/c.txt").await),
                ),
                (
                    "Gone/d.txt".to_string(),
                    Some(last_held(&instance, other, gone_removed, "Gone/d.txt").await),
                ),
                (
                    "Dir/b.txt".to_string(),
                    Some(last_held(&instance, tip, tip, "Dir/b.txt").await),
                ),
                ("Never/there.txt".to_string(), None),
            ]);
            assert_eq!(found, expected);
        },
    )
    .await;
}

/// Files in more directories than a comparison looks up at once, deleted in two revisions, are
/// each found where the line last held them.
#[tokio::test]
async fn the_walk_finds_more_paths_than_it_looks_up_at_once() {
    run(
        "the_walk_finds_more_paths_than_it_looks_up_at_once",
        &MANY,
        |instance| async move {
            let first = current(&instance).await;
            let (early, late) = MANY.split_at(40);
            let edits: Vec<Edit> = early.iter().map(|path| Edit::Delete(path)).collect();
            let early_removed = commit_edits(&instance, &edits, "Delete some").await;
            let late_removed = commit_edits(&instance, &[Edit::Delete("Many")], "Delete all").await;
            let bound = revision_state(&instance, first).await.revision_number();

            let found = walk(&instance, late_removed, &MANY, bound, 2).await;

            let mut expected = BTreeMap::new();
            for path in early {
                expected.insert(
                    path.to_string(),
                    Some(last_held(&instance, first, early_removed, path).await),
                );
            }
            for path in late {
                expected.insert(
                    path.to_string(),
                    Some(last_held(&instance, early_removed, late_removed, path).await),
                );
            }
            assert_eq!(found, expected);
        },
    )
    .await;
}

/// A revision whose tree cannot be read fails the walk, rather than counting as one that does not
/// hold the path, which would answer the deletion as older than the line can tell.
#[tokio::test]
async fn a_revision_the_walk_cannot_read_fails_it() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (stores, counting) = counted_client_stores().await;
            let repository =
                in_memory::repository(stores.immutable.clone(), stores.mutable.clone());
            let branch = in_memory::branch_id();
            let state = State::new();
            let dir = add(
                &state,
                repository.clone(),
                ROOT_NODE,
                directory("Dir"),
                "Dir",
            )
            .await;
            add(&state, repository.clone(), dir, file("a.txt"), "a.txt").await;
            let held = in_memory::commit(&repository, &state, Hash::default(), branch).await;
            let state = in_memory::revision_state(&repository, held).await;
            let a = state
                .find_node_link(repository.clone(), "Dir/a.txt")
                .await
                .expect("resolving the file")
                .node;
            state
                .node_delete(repository.clone(), a)
                .await
                .expect("deleting the file");
            let removed = in_memory::commit(&repository, &state, held, branch).await;
            let tree = in_memory::revision_state(&repository, held)
                .await
                .tree(repository.clone())
                .await
                .expect("reading the tree");
            counting.fail_reads_of([tree.hash_node]);

            let walked = walk_line(
                repository.clone(),
                in_memory::revision_state(&repository, removed).await,
                vec!["Dir/a.txt".to_string()],
                0,
                &LineWalks::new(1),
            )
            .await;

            assert!(walked.is_err(), "{walked:?}");
        })
        .await;
}

/// A directory becomes a link to a repository that cannot be read, and a directory again without
/// the file it held. No path below the link is held in the line's repository, and the walk reads
/// nothing the link names, so it finds the file where the line last held it before the link.
#[tokio::test]
async fn a_path_below_a_link_is_not_held_and_the_link_is_not_read() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let stores = client_stores().await;
            let repository =
                in_memory::repository(stores.immutable.clone(), stores.mutable.clone());
            let branch = in_memory::branch_id();
            let tree = |foo: Node, file_name: &'static str| {
                let repository = repository.clone();
                async move {
                    let state = State::new();
                    let plugins = add(
                        &state,
                        repository.clone(),
                        ROOT_NODE,
                        directory("Plugins"),
                        "Plugins",
                    )
                    .await;
                    let foo = add(&state, repository.clone(), plugins, foo, "Foo").await;
                    if foo_is_directory(&state, &repository, foo).await {
                        add(&state, repository, foo, file(file_name), file_name).await;
                    }
                    state
                }
            };
            let held = in_memory::commit(
                &repository,
                &tree(directory("Foo"), "a.txt").await,
                Hash::default(),
                branch,
            )
            .await;
            let unreadable = link_node(
                "Foo",
                RepositoryId::from(uuid::Uuid::now_v7()),
                Hash::from([0x77; 32]),
            );
            let linked =
                in_memory::commit(&repository, &tree(unreadable, "").await, held, branch).await;
            let back = in_memory::commit(
                &repository,
                &tree(directory("Foo"), "b.txt").await,
                linked,
                branch,
            )
            .await;
            let held_state = in_memory::revision_state(&repository, held).await;
            let node = held_state
                .find_node_link(repository.clone(), "Plugins/Foo/a.txt")
                .await
                .expect("The first revision holds the file")
                .node;

            let walked = walk_line(
                repository.clone(),
                in_memory::revision_state(&repository, back).await,
                vec!["Plugins/Foo/a.txt".to_string()],
                0,
                &LineWalks::new(2),
            )
            .await
            .expect("The walk reads nothing the link names");

            assert_eq!(
                walked,
                BTreeMap::from([(
                    "Plugins/Foo/a.txt".to_string(),
                    Some(LastHeld {
                        removed_in: linked,
                        removed_in_number: in_memory::revision_state(&repository, linked)
                            .await
                            .revision_number(),
                        held_number: held_state.revision_number(),
                        node,
                    })
                )])
            );
        })
        .await;
}

/// Whether `node` of `state` is a directory, rather than a link.
async fn foo_is_directory(
    state: &State,
    repository: &std::sync::Arc<lore_revision::repository::RepositoryContext>,
    node: u32,
) -> bool {
    state
        .node(repository.clone(), node)
        .await
        .expect("The node was just added")
        .is_directory()
}

/// A walk from a local revision down a long line that only the remote holds reads its first two
/// pages by hash signature, and every page below them by revision number on the branch of the
/// line, requested ahead of it.
#[tokio::test]
async fn a_long_remote_line_is_read_ahead_by_number() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let stores = client_stores().await;
            let branch = in_memory::branch_id();
            let line = remote_line(1000, branch_metadata(&stores, branch).await).await;
            let remote = std::sync::Arc::new(paging_remote(branch, &line));
            let start = write_line(&stores.writer, line[999].signature, 1001, 1).await[0];
            let client = client_on(&stores, remote.clone());
            let start = State::deserialize(client.clone(), start)
                .await
                .expect("reading the local revision");

            let walked = walk_line(
                client,
                start,
                vec!["Dir/a.txt".to_string()],
                0,
                &LineWalks::new(2),
            )
            .await
            .expect("The line can be read");

            assert_eq!(walked, BTreeMap::from([("Dir/a.txt".to_string(), None)]));
            assert_eq!(
                anchors(&remote),
                (
                    vec![signature_of(&line, 1000), signature_of(&line, 900)],
                    vec![800, 700, 600, 500, 400, 300, 200, 100],
                )
            );
        })
        .await;
}

/// Comparisons that end out of order are taken in line order, each once, and none past one that
/// has not ended, so a path is found in the newest revision holding it.
#[test]
fn comparisons_are_taken_in_line_order() {
    let mut in_order = InOrder::new();
    let mut take = |index, result| {
        let mut taken = Vec::new();
        in_order.take(index, result, |result| taken.push(result));
        taken
    };

    let early = [take(2, 'c'), take(1, 'b')];
    let first = take(0, 'a');
    let gap = take(4, 'e');
    let filled = take(3, 'd');

    assert!(early.iter().all(Vec::is_empty), "Nothing before the first");
    assert_eq!(first, ['a', 'b', 'c']);
    assert!(gap.is_empty(), "Nothing past the gap");
    assert_eq!(filled, ['d', 'e']);
    assert_eq!(in_order.taken(), 5);
}

/// Each side deletes a file the other edits, so the walk reads both sides' lines, sharing a single
/// comparison in flight between them, and finds each deletion where its own line last held the
/// file.
#[tokio::test]
async fn two_lines_sharing_one_comparison_each_find_their_deletions() {
    run(
        "two_lines_sharing_one_comparison_each_find_their_deletions",
        &TREE,
        |instance| async move {
            let first = current(&instance).await;
            create_branch(&instance, "feature").await;
            let feature_held =
                commit_edits(&instance, &[Edit::Write("Dir/a.txt", "feature")], "Edit a").await;
            let feature_removed =
                commit_edits(&instance, &[Edit::Delete("Dir/b.txt")], "Delete b").await;
            let feature =
                commit_edits(&instance, &[Edit::Write("Other/e.txt", "e")], "Other").await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            let main_removed =
                commit_edits(&instance, &[Edit::Delete("Dir/a.txt")], "Delete a").await;
            let main = commit_edits(&instance, &[Edit::Write("Dir/b.txt", "main")], "Edit b").await;
            let bound = revision_state(&instance, first).await.revision_number();
            let conflicts =
                diff3_conflicts(&instance, first, main, feature, MergesAcross::Unknown).await;

            let deletions = Deletions::walk(&conflicts, instance.repository.id, bound, 1)
                .await
                .expect("The lines can be read");

            assert_eq!(conflicts.len(), 2, "Both files conflict: {conflicts:?}");
            let main_state = revision_state(&instance, main).await;
            let feature_state = revision_state(&instance, feature).await;
            assert_eq!(
                deletions
                    .last_held(&instance.repository, &main_state, "Dir/a.txt")
                    .expect("Main's line was walked"),
                Some(last_held(&instance, first, main_removed, "Dir/a.txt").await)
            );
            assert_eq!(
                deletions
                    .last_held(&instance.repository, &feature_state, "Dir/b.txt")
                    .expect("The feature branch's line was walked"),
                Some(last_held(&instance, feature_held, feature_removed, "Dir/b.txt").await)
            );
        },
    )
    .await;
}

/// The files of the fixture of a deleted directory holding a conflict. `Dir-x` sorts between `Dir`
/// and `Dir/`, so a search for the paths below `Dir` that starts at `Dir` meets it first.
const DELETED_DIRECTORIES: [&str; 5] = [
    "Dir/a.txt",
    "Dir/Deep/c.txt",
    "Dir-x/e.txt",
    "Gone/d.txt",
    "Top.txt",
];

/// The feature branch deletes three directories, and main edits a file in two of them. The edits
/// conflict with the deletions, and the diff drops the delete of each directory holding a
/// conflict, at any depth, so the conflict stands for its path; the delete of the directory
/// holding none stays.
#[tokio::test]
async fn the_delete_of_a_directory_holding_a_conflict_is_dropped() {
    run(
        "the_delete_of_a_directory_holding_a_conflict_is_dropped",
        &DELETED_DIRECTORIES,
        |instance| async move {
            create_branch(&instance, "feature").await;
            let feature = commit_edits(
                &instance,
                &[
                    Edit::Delete("Dir"),
                    Edit::Delete("Dir-x"),
                    Edit::Delete("Gone"),
                ],
                "Delete",
            )
            .await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            let main = commit_edits(
                &instance,
                &[
                    Edit::Write("Dir/Deep/c.txt", "main"),
                    Edit::Write("Dir-x/e.txt", "main"),
                ],
                "Edit",
            )
            .await;
            let feature_id = branch::resolve(instance.repository.clone(), "feature")
                .await
                .expect("Failed to resolve the feature branch")
                .id;

            let diff = branch::diff3_collect(
                instance.repository.clone(),
                feature_id,
                feature,
                instance.main,
                main,
                None,
                false,
                false,
            )
            .await
            .expect("Failed to diff the branches");

            let mut conflicts: Vec<&str> = diff
                .conflicts
                .iter()
                .map(|(from, _)| from.path().as_str())
                .collect();
            conflicts.sort_unstable();
            assert_eq!(conflicts, ["Dir-x/e.txt", "Dir/Deep/c.txt"]);
            let deleted_directories: Vec<&str> = diff
                .changes
                .iter()
                .filter(|change| {
                    change.action == FileAction::Delete && change.from.flags.is_directory()
                })
                .map(|change| change.path().as_str())
                .collect();
            assert_eq!(
                deleted_directories,
                ["Gone"],
                "Only the delete of the directory holding no conflict stays"
            );
        },
    )
    .await;
}

/// The feature branch edits a file and main merges the edit, while the feature branch merges an
/// earlier main revision, kept on a branch of its own. The base of the two is that main revision,
/// which holds the file as it was, so once main deletes the file, the two sides changed it since.
/// Main's history shows it took the edit before deleting the file, so the deletion stands without
/// a conflict, unless the diff is told no merge joins the two sides, when the conflict stands.
#[tokio::test]
async fn a_deletion_of_a_file_whose_edit_the_deleting_side_merged_is_no_conflict() {
    run(
        "a_deletion_of_a_file_whose_edit_the_deleting_side_merged_is_no_conflict",
        &TREE,
        |instance| async move {
            create_branch(&instance, "feature").await;
            commit_edits(&instance, &[Edit::Write("Dir/a.txt", "feature")], "Edit").await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            commit_edits(&instance, &[Edit::Write("Top.txt", "one")], "Main one").await;
            let earlier =
                commit_edits(&instance, &[Edit::Write("Top.txt", "two")], "Main two").await;
            create_branch(&instance, "earlier").await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            assert!(
                merge_branch(&instance, "feature").await,
                "Main takes the edit"
            );
            let main = commit_edits(&instance, &[Edit::Delete("Dir/a.txt")], "Delete").await;
            switch_to(&instance, "feature").await;
            assert!(
                merge_branch(&instance, "earlier").await,
                "The feature branch takes the earlier main revision"
            );
            let feature = current(&instance).await;
            let feature_id = branch::resolve(instance.repository.clone(), "feature")
                .await
                .expect("Failed to resolve the feature branch")
                .id;

            let diff = branch::diff3_collect(
                instance.repository.clone(),
                instance.main,
                main,
                feature_id,
                feature,
                None,
                false,
                false,
            )
            .await
            .expect("Failed to diff the branches");
            let told_none = diff3_conflicts(
                &instance,
                earlier,
                main,
                feature,
                MergesAcross::NoneAboveBase,
            )
            .await
            .len();

            assert!(
                diff.conflicts.is_empty(),
                "No conflict remains: {:?}",
                diff.conflicts
            );
            assert!(
                diff.changes
                    .iter()
                    .any(|change| change.path().as_str() == "Dir/a.txt"
                        && change.action == FileAction::Delete),
                "Main's deletion is merged: {:?}",
                diff.changes
            );
            assert_eq!(
                told_none, 1,
                "Told that no merge joins the sides, the diff leaves the conflict standing"
            );
        },
    )
    .await;
}

/// The conflicts of the 3-way diff of `source` and `target` from `base`, told `merges`.
async fn diff3_conflicts(
    instance: &Instance,
    base: Hash,
    source: Hash,
    target: Hash,
    merges: MergesAcross,
) -> Vec<(NodeChange, NodeChange)> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let diff = revision::diff3_with_source_cap(
        instance.repository.clone(),
        base,
        source,
        target,
        None,
        false,
        None,
        None,
        None,
        merges,
        LinkSettlement::Tree,
        tx,
    );
    let collect = async move {
        let mut conflicts = Vec::new();
        while let Some(item) = rx.recv().await {
            if let Ok(revision::DiffItem::Conflict(conflict)) = item {
                conflicts.push(*conflict);
            }
        }
        conflicts
    };
    let (diffed, conflicts) = tokio::join!(diff, collect);
    diffed.expect("Failed to diff the revisions");
    conflicts
}

/// Which merges the search for the base of a branch diff from `source` on `source_branch` to
/// `target` on `target_branch` proved.
async fn merges(
    instance: &Instance,
    source_branch: lore_base::types::BranchId,
    source: Hash,
    target_branch: lore_base::types::BranchId,
    target: Hash,
) -> MergesAcross {
    branch::resolve_diff3_base_and_merges(
        instance.repository.clone(),
        source_branch,
        source,
        target_branch,
        target,
    )
    .await
    .expect("Failed to resolve the base")
    .1
}

/// The search for the base proves that no merge joins two branches that never merged, and proves
/// nothing once one merged the other, or between a branch and itself.
#[tokio::test]
async fn the_base_search_proves_no_merge_only_between_branches_that_never_merged() {
    run(
        "the_base_search_proves_no_merge_only_between_branches_that_never_merged",
        &TREE,
        |instance| async move {
            let first = current(&instance).await;
            create_branch(&instance, "feature").await;
            let feature =
                commit_edits(&instance, &[Edit::Write("Dir/a.txt", "feature")], "Edit").await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            let main = commit_edits(&instance, &[Edit::Write("Top.txt", "one")], "Main one").await;
            let feature_id = branch::resolve(instance.repository.clone(), "feature")
                .await
                .expect("Failed to resolve the feature branch")
                .id;

            let never = merges(&instance, instance.main, main, feature_id, feature).await;
            let itself = merges(&instance, instance.main, main, instance.main, first).await;
            assert!(
                merge_branch(&instance, "feature").await,
                "Main takes the edit"
            );
            let merged = current(&instance).await;
            let joined = merges(&instance, feature_id, feature, instance.main, merged).await;

            assert_eq!(
                never,
                MergesAcross::NoneAboveBase,
                "The branches never merged"
            );
            assert_eq!(itself, MergesAcross::Unknown, "A branch merges into itself");
            assert_eq!(
                joined,
                MergesAcross::Unknown,
                "Main merged the feature branch"
            );
        },
    )
    .await;
}
