// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_revision::filter;
use lore_revision::filter::Filter;
use lore_revision::filter::FilterInstance;
use lore_revision::filter::parse_filter;
use lore_revision::fs::swfs::frozen_files::FrozenFile;
use lore_revision::fs::swfs::frozen_files::moves_in;
use lore_revision::fs::swfs::modified_tree::Change;
use lore_revision::fs::swfs::modified_tree::Coverage;
use lore_revision::fs::swfs::modified_tree::ModifiedTree;
use lore_revision::fs::swfs::served_tree::ServedTree;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;

use crate::fs::filesystem_provider::test_store_create;
use crate::helper::default_repository_creation_args;

async fn with_tree<F, Fut>(test: F)
where
    F: FnOnce(ModifiedTree) -> Fut,
    Fut: Future<Output = ()>,
{
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    let repository = Arc::new(RepositoryContext::new(default_repository_creation_args(
        immutable_store,
        mutable_store,
    )));
    LORE_CONTEXT
        .scope(execution, test(ModifiedTree::new(repository)))
        .await;
}

async fn node_of(tree: &ModifiedTree, path: &str) -> lore_revision::node::Node {
    tree.state()
        .find_node(tree.repository(), path)
        .await
        .expect("node recorded")
}

async fn sorted_paths(tree: &ModifiedTree) -> Vec<String> {
    let mut paths: Vec<String> = tree
        .paths()
        .await
        .expect("listing modified paths")
        .iter()
        .map(|path| path.as_str().to_owned())
        .collect();
    paths.sort();
    paths
}

#[tokio::test]
async fn an_added_file_is_recorded_dirty_add_with_its_directories() {
    with_tree(async |tree| {
        tree.mark_added("d1/a.txt").await.unwrap();

        let file = node_of(&tree, "d1/a.txt").await;
        assert!(file.is_dirty_add());
        assert!(!file.is_staged(), "the record carries no staged flags");
        assert!(node_of(&tree, "d1").await.is_dirty());
        assert!(tree.is_modified("d1/a.txt").await.unwrap());
        assert_eq!(sorted_paths(&tree).await, vec!["d1/a.txt"]);
    })
    .await;
}

#[tokio::test]
async fn an_unrecorded_path_is_not_modified() {
    with_tree(async |tree| {
        tree.mark_modified("a.txt").await.unwrap();

        assert!(!tree.is_modified("b.txt").await.unwrap());
        assert!(!tree.is_modified("d1/a.txt").await.unwrap());
    })
    .await;
}

#[tokio::test]
async fn a_modified_file_is_recorded_dirty_modify() {
    with_tree(async |tree| {
        tree.mark_modified("a.txt").await.unwrap();

        assert!(node_of(&tree, "a.txt").await.is_dirty_modify());
    })
    .await;
}

#[tokio::test]
async fn a_write_after_a_create_keeps_the_file_an_add() {
    with_tree(async |tree| {
        tree.mark_added("a.txt").await.unwrap();
        tree.mark_modified("a.txt").await.unwrap();

        assert!(node_of(&tree, "a.txt").await.is_dirty_add());
    })
    .await;
}

#[tokio::test]
async fn a_deleted_file_is_recorded_dirty_delete() {
    with_tree(async |tree| {
        tree.mark_deleted("d1/a.txt").await.unwrap();

        assert!(node_of(&tree, "d1/a.txt").await.is_dirty_delete());
        assert_eq!(sorted_paths(&tree).await, vec!["d1/a.txt"]);
    })
    .await;
}

#[tokio::test]
async fn a_move_is_a_delete_of_the_old_path_and_an_add_of_the_new() {
    with_tree(async |tree| {
        tree.mark_moved("d1/a.txt", "d1/c.txt").await.unwrap();

        assert!(node_of(&tree, "d1/a.txt").await.is_dirty_delete());
        assert!(node_of(&tree, "d1/c.txt").await.is_dirty_add());
        assert_eq!(sorted_paths(&tree).await, vec!["d1/a.txt", "d1/c.txt"]);
    })
    .await;
}

#[tokio::test]
async fn clear_forgets_every_recorded_path() {
    with_tree(async |tree| {
        tree.mark_added("a.txt").await.unwrap();
        tree.mark_deleted("d1/b.txt").await.unwrap();

        tree.clear();

        assert!(!tree.is_modified("a.txt").await.unwrap());
        assert!(sorted_paths(&tree).await.is_empty());
    })
    .await;
}

#[tokio::test]
async fn the_record_starts_untrusted_and_trust_is_set_explicitly() {
    with_tree(async |tree| {
        assert!(!tree.is_trusted());

        tree.set_trusted(true);
        assert!(tree.is_trusted());

        tree.set_trusted(false);
        assert!(!tree.is_trusted());
    })
    .await;
}

fn ignoring(rules: &str) -> Arc<Filter> {
    let ignore = parse_filter(rules.as_bytes(), Path::new(".loreignore")).expect("parsing rules");
    Arc::new(filter::from_instances(ignore, FilterInstance::default()).expect("building filter"))
}

#[tokio::test]
async fn record_marks_each_change_as_the_matching_modification() {
    with_tree(async |tree| {
        tree.record(Change::Created {
            path: "a.txt".into(),
            directory: false,
        })
        .await;
        tree.record(Change::Written("b.txt".into())).await;
        tree.record(Change::Deleted {
            path: "c.txt".into(),
            directory: false,
        })
        .await;
        tree.record(Change::Moved {
            from: "d.txt".into(),
            to: "e.txt".into(),
            directory: false,
        })
        .await;

        assert!(node_of(&tree, "a.txt").await.is_dirty_add());
        assert!(node_of(&tree, "b.txt").await.is_dirty_modify());
        assert!(node_of(&tree, "c.txt").await.is_dirty_delete());
        assert!(node_of(&tree, "d.txt").await.is_dirty_delete());
        assert!(node_of(&tree, "e.txt").await.is_dirty_add());
    })
    .await;
}

#[tokio::test]
async fn a_created_directory_is_recorded_without_the_file_flag() {
    with_tree(async |tree| {
        tree.record(Change::Created {
            path: "d1".into(),
            directory: true,
        })
        .await;

        let node = node_of(&tree, "d1").await;
        assert!(node.is_dirty_add());
        assert!(node.is_directory());
    })
    .await;
}

#[tokio::test]
async fn a_change_to_an_ignored_path_leaves_the_record_unchanged() {
    with_tree(async |tree| {
        tree.set_ignore(ignoring("build/\n*.tmp\n"));

        tree.record(Change::Created {
            path: "build/out.bin".into(),
            directory: false,
        })
        .await;
        tree.record(Change::Written("notes.tmp".into())).await;
        tree.record(Change::Deleted {
            path: "build".into(),
            directory: true,
        })
        .await;

        assert!(sorted_paths(&tree).await.is_empty());
        assert!(!tree.is_modified("build/out.bin").await.unwrap());
    })
    .await;
}

#[tokio::test]
async fn a_move_out_of_an_ignored_path_records_only_the_add() {
    with_tree(async |tree| {
        tree.set_ignore(ignoring("build/\n"));

        tree.record(Change::Moved {
            from: "build/out.bin".into(),
            to: "kept.bin".into(),
            directory: false,
        })
        .await;

        assert_eq!(sorted_paths(&tree).await, vec!["kept.bin"]);
    })
    .await;
}

#[tokio::test]
async fn a_move_into_an_ignored_path_records_only_the_delete() {
    with_tree(async |tree| {
        tree.set_ignore(ignoring("build/\n"));

        tree.record(Change::Moved {
            from: "kept.bin".into(),
            to: "build/out.bin".into(),
            directory: false,
        })
        .await;

        assert!(node_of(&tree, "kept.bin").await.is_dirty_delete());
        assert_eq!(sorted_paths(&tree).await, vec!["kept.bin"]);
    })
    .await;
}

#[tokio::test]
async fn the_repository_directory_is_always_ignored() {
    with_tree(async |tree| {
        tree.set_ignore(ignoring(""));

        tree.record(Change::Written(".lore/config".into())).await;

        assert!(sorted_paths(&tree).await.is_empty());
    })
    .await;
}

#[tokio::test]
async fn recording_a_change_never_marks_the_record_trusted() {
    with_tree(async |tree| {
        tree.record(Change::Written("a.txt".into())).await;

        assert!(!tree.is_trusted());
    })
    .await;
}

/// The ignore rules the record filters by are read from the revision, so a change to the file
/// holding them means the record no longer knows what it should skip.
#[tokio::test]
async fn a_change_to_the_ignore_rules_leaves_the_record_untrusted() {
    with_tree(async |tree| {
        for change in [
            Change::Written(".loreignore".into()),
            Change::Created {
                path: ".loreignore".into(),
                directory: false,
            },
            Change::Deleted {
                path: ".loreignore".into(),
                directory: false,
            },
        ] {
            tree.set_trusted(true);

            tree.record(change).await;

            assert!(!tree.is_trusted());
        }
    })
    .await;
}

/// A change of revision moves what the record is measured against, and Lore's own writes to the
/// tree are not reported to it, so the record starts over: it is rebuilt by the next diff.
#[tokio::test]
async fn a_new_revision_empties_the_record_and_leaves_it_untrusted() {
    with_tree(async |tree| {
        let first = Hash::from_u64(1);
        let second = Hash::from_u64(2);
        tree.set_trusted(true);
        tree.record(Change::Written("a.txt".into())).await;

        tree.observe_revision(first).await;
        assert!(
            tree.is_trusted(),
            "the first revision seen is the one the record started against"
        );
        assert!(tree.is_modified("a.txt").await.unwrap());

        tree.observe_revision(first).await;
        assert!(tree.is_trusted());
        assert!(tree.is_modified("a.txt").await.unwrap());

        tree.observe_revision(second).await;
        assert!(!tree.is_trusted());
        assert!(!tree.is_modified("a.txt").await.unwrap());
        assert!(sorted_paths(&tree).await.is_empty());
    })
    .await;
}

/// A path below a directory the record holds as added or deleted is changed with it, though the
/// record names only the directory.
#[tokio::test]
async fn a_path_below_a_recorded_directory_is_covered_by_it() {
    with_tree(async |tree| {
        tree.record(Change::Deleted {
            path: "gone".into(),
            directory: true,
        })
        .await;
        tree.record(Change::Created {
            path: "fresh".into(),
            directory: true,
        })
        .await;
        tree.record(Change::Written("kept/edited.txt".into())).await;

        assert!(tree.covers("gone").await.unwrap());
        assert!(tree.covers("gone/inside/deep.txt").await.unwrap());
        assert!(tree.covers("fresh/x.txt").await.unwrap());
        assert!(tree.covers("kept/edited.txt").await.unwrap());
        assert!(!tree.covers("kept/other.txt").await.unwrap());
        assert!(
            !tree.covers("kept").await.unwrap(),
            "a directory holding a change is not changed"
        );
        assert!(!tree.covers("elsewhere/a.txt").await.unwrap());
    })
    .await;
}

/// SWFS calls its callbacks from threads of its own, so creates in one new directory can reach the
/// record together. Each file must be recorded, and the directory they share recorded once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_creates_in_one_new_directory_are_all_recorded() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    let repository = Arc::new(RepositoryContext::new(default_repository_creation_args(
        immutable_store,
        mutable_store,
    )));

    LORE_CONTEXT
        .scope(execution, async move {
            for round in 0..20 {
                let tree = Arc::new(ModifiedTree::new(repository.clone()));
                let mut tasks = tokio::task::JoinSet::new();
                for file in 0..16 {
                    let tree = tree.clone();
                    lore_spawn!(tasks, async move {
                        tree.record(Change::Created {
                            path: format!("shared/dir{}/file{file}.txt", file % 2),
                            directory: false,
                        })
                        .await;
                    });
                }
                while let Some(joined) = tasks.join_next().await {
                    joined.expect("recording task");
                }

                let paths = sorted_paths(&tree).await;
                assert_eq!(paths.len(), 16, "round {round} recorded {paths:?}");
                let mut distinct = paths.clone();
                distinct.dedup();
                assert_eq!(distinct.len(), 16, "round {round} recorded a path twice");

                // A directory two creates both found missing would be added twice.
                let state = tree.state();
                let top = state
                    .node_children(repository.clone(), ROOT_NODE)
                    .await
                    .expect("children of the root");
                assert_eq!(
                    top.len(),
                    1,
                    "round {round} holds `shared` {} times",
                    top.len()
                );
                let below = state
                    .node_children(repository.clone(), top[0])
                    .await
                    .expect("children of shared");
                assert_eq!(
                    below.len(),
                    2,
                    "round {round} holds {} directories",
                    below.len()
                );
            }
        })
        .await;
}

/// A walk that seeds the record runs while callbacks keep arriving. One it could not record
/// leaves the record missing a change, which the walk finishing must not make good.
#[tokio::test]
async fn a_failure_while_seeding_keeps_the_record_untrusted() {
    with_tree(async |tree| {
        let seeding = tree.distrust_mark();
        tree.distrust();

        tree.trust_unless_distrusted_since(seeding);

        assert!(!tree.is_trusted());
    })
    .await;
}

#[tokio::test]
async fn a_seed_without_failures_makes_the_record_trusted() {
    with_tree(async |tree| {
        let seeding = tree.distrust_mark();

        tree.trust_unless_distrusted_since(seeding);

        assert!(tree.is_trusted());
    })
    .await;
}

/// A change recorded after the seed began and found unrecordable, such as one for a path the
/// record cannot parse, is a failure like any other.
#[tokio::test]
async fn a_change_that_cannot_be_recorded_counts_as_a_failure_for_a_seed() {
    with_tree(async |tree| {
        let seeding = tree.distrust_mark();

        tree.record(Change::Written("../outside.txt".into())).await;
        tree.trust_unless_distrusted_since(seeding);

        assert!(!tree.is_trusted());
    })
    .await;
}

/// What the record says of a path decides where a read for it is answered from: a path changed is
/// read from SWFS, and a path deleted is simply not there.
#[tokio::test]
async fn coverage_tells_a_deleted_path_from_a_changed_one() {
    with_tree(async |tree| {
        tree.record(Change::Deleted {
            path: "gone".into(),
            directory: true,
        })
        .await;
        tree.record(Change::Deleted {
            path: "kept/removed.txt".into(),
            directory: false,
        })
        .await;
        tree.record(Change::Created {
            path: "fresh".into(),
            directory: true,
        })
        .await;
        tree.record(Change::Written("kept/edited.txt".into())).await;

        assert_eq!(tree.coverage("gone").await.unwrap(), Coverage::Deleted);
        assert_eq!(
            tree.coverage("gone/inside/deep.txt").await.unwrap(),
            Coverage::Deleted
        );
        assert_eq!(
            tree.coverage("kept/removed.txt").await.unwrap(),
            Coverage::Deleted
        );
        assert_eq!(tree.coverage("fresh").await.unwrap(), Coverage::Changed);
        assert_eq!(
            tree.coverage("fresh/x.txt").await.unwrap(),
            Coverage::Changed
        );
        assert_eq!(
            tree.coverage("kept/edited.txt").await.unwrap(),
            Coverage::Changed
        );
        assert_eq!(tree.coverage("kept").await.unwrap(), Coverage::Unchanged);
        assert_eq!(
            tree.coverage("kept/other.txt").await.unwrap(),
            Coverage::Unchanged
        );
        assert_eq!(
            tree.coverage("elsewhere/a.txt").await.unwrap(),
            Coverage::Unchanged
        );
    })
    .await;
}

/// A file deleted and then made again is there, and what is there is what the record last saw.
#[tokio::test]
async fn a_path_deleted_and_created_again_is_changed_not_deleted() {
    with_tree(async |tree| {
        tree.record(Change::Deleted {
            path: "a.txt".into(),
            directory: false,
        })
        .await;
        tree.record(Change::Created {
            path: "a.txt".into(),
            directory: false,
        })
        .await;

        assert_eq!(tree.coverage("a.txt").await.unwrap(), Coverage::Changed);
    })
    .await;
}

/// A listing is the revision's children changed by what the record holds directly in the directory.
#[tokio::test]
async fn children_lists_what_the_record_holds_directly_in_a_directory() {
    with_tree(async |tree| {
        tree.record(Change::Created {
            path: "sub/a.txt".into(),
            directory: false,
        })
        .await;
        tree.record(Change::Deleted {
            path: "sub/b.txt".into(),
            directory: false,
        })
        .await;
        tree.record(Change::Written("c.txt".into())).await;

        let mut root = tree.children("").await.unwrap();
        root.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(
            root.iter()
                .map(|child| (child.name.as_str(), child.coverage))
                .collect::<Vec<_>>(),
            // `sub` is held only as the way to what changed in it.
            vec![("c.txt", Coverage::Changed), ("sub", Coverage::Unchanged)]
        );

        let mut sub = tree.children("sub").await.unwrap();
        sub.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(
            sub.iter()
                .map(|child| (child.name.as_str(), child.coverage))
                .collect::<Vec<_>>(),
            vec![("a.txt", Coverage::Changed), ("b.txt", Coverage::Deleted)]
        );

        assert!(tree.children("missing").await.unwrap().is_empty());
        assert!(tree.children("c.txt").await.unwrap().is_empty());
    })
    .await;
}

/// An entry of SWFS's modified list, as `swfsGetModifiedFileList` names it.
fn listed(path: &str, mod_flags: u32, is_directory: bool) -> FrozenFile {
    FrozenFile {
        path: path.to_string(),
        mod_flags,
        file_index: 0,
        size: 0,
        time_create: 0,
        time_write: 0,
        is_directory,
    }
}

// The flags `swfs_experiment`'s `frozenlist` and `dirdelete` probes saw SWFS give each kind of change.
/// A listed entry carrying the `file_index` SWFS gives a node, which pairs a move's two ends.
fn listed_node(path: &str, mod_flags: u32, file_index: u32) -> FrozenFile {
    FrozenFile {
        file_index,
        ..listed(path, mod_flags, false)
    }
}

const MODIFIED: u32 = 0x7340;
const CREATED: u32 = 0x6101;
const RENAMED: u32 = 0x0406;
const DELETED: u32 = 0x000a;
const CREATED_THEN_DELETED: u32 = 0x610b;
const DIRECTORY_CHANGED_WITHIN: u32 = 0x0086;

#[tokio::test]
async fn a_rebuild_records_what_swfs_lists_and_trusts_the_record() {
    with_tree(async |tree| {
        tree.record(Change::Written("stale.txt".into())).await;
        let revision = revision_holding(
            &tree,
            &[
                "fl/modified.txt",
                "fl/deleted.txt",
                "dd/sub/a.txt",
                "stale.txt",
            ],
        )
        .await;

        let trusted = tree
            .rebuild(
                &[
                    listed(r"\", 0x0050, false),
                    listed(r"\fl", DIRECTORY_CHANGED_WITHIN, true),
                    listed(r"\fl\modified.txt", MODIFIED, false),
                    listed(r"\fl\created.txt", CREATED, false),
                    listed(r"\fl\deleted.txt", DELETED, false),
                    listed(r"\dd\sub", DELETED, true),
                    listed(r"\dd\made\x.txt", CREATED_THEN_DELETED, false),
                    listed(r"\dd\made", 0x000b, true),
                ],
                Some(&revision),
            )
            .await
            .unwrap();

        assert!(trusted);
        assert!(tree.is_trusted());
        assert_eq!(
            tree.coverage("fl/modified.txt").await.unwrap(),
            Coverage::Changed
        );
        assert_eq!(
            tree.coverage("fl/created.txt").await.unwrap(),
            Coverage::Changed
        );
        assert_eq!(
            tree.coverage("fl/deleted.txt").await.unwrap(),
            Coverage::Deleted
        );
        assert_eq!(
            tree.coverage("dd/sub/a.txt").await.unwrap(),
            Coverage::Deleted
        );
        // What the list no longer names is gone.
        assert_eq!(
            tree.coverage("stale.txt").await.unwrap(),
            Coverage::Unchanged
        );
        // A deletion of a path the revision does not hold, created and deleted since, is left out.
        assert_eq!(
            tree.coverage("dd/made/x.txt").await.unwrap(),
            Coverage::Unchanged
        );
    })
    .await;
}

#[tokio::test]
async fn a_rebuild_takes_a_renames_origin_from_the_deletion_that_shares_its_file_index() {
    with_tree(async |tree| {
        let trusted = tree
            .rebuild(
                &[
                    listed_node(r"\c.txt", RENAMED, 7),
                    listed_node(r"\a.txt", DELETED, 7),
                ],
                None,
            )
            .await
            .unwrap();

        assert!(trusted);
        assert_eq!(tree.coverage("a.txt").await.unwrap(), Coverage::Deleted);
        assert_eq!(tree.coverage("c.txt").await.unwrap(), Coverage::Changed);
        assert_eq!(tree.coverage("b.txt").await.unwrap(), Coverage::Unchanged);
    })
    .await;
}

#[test]
fn moves_in_pairs_a_destination_with_the_deletion_that_shares_its_file_index() {
    let listed = [
        listed_node(r"\c.txt", RENAMED, 7),
        listed_node(r"\a.txt", DELETED, 7),
        // A deletion with no index is no move's origin, and a rename with no deletion has none.
        listed_node(r"\gone.txt", DELETED, 0),
        listed_node(r"\lonely.txt", RENAMED, 9),
    ];

    assert_eq!(
        vec![("a.txt".to_string(), "c.txt".to_string())],
        moves_in(&listed)
    );
}

#[tokio::test]
async fn a_rebuild_leaves_the_record_untrusted_for_a_rename_whose_origin_is_unknown() {
    with_tree(async |tree| {
        let trusted = tree
            .rebuild(
                &[
                    listed(r"\renamed2.txt", RENAMED, false),
                    listed(r"\modified.txt", MODIFIED, false),
                ],
                None,
            )
            .await
            .unwrap();

        assert!(!trusted);
        assert!(!tree.is_trusted());
        // The destination is still recorded, so a diff that walks the tree finds it.
        assert_eq!(
            tree.coverage("renamed2.txt").await.unwrap(),
            Coverage::Changed
        );
        assert_eq!(
            tree.coverage("modified.txt").await.unwrap(),
            Coverage::Changed
        );
    })
    .await;
}

#[tokio::test]
async fn a_rename_a_rebuild_no_longer_finds_listed_is_forgotten() {
    with_tree(async |tree| {
        tree.record(Change::Moved {
            from: "a.txt".into(),
            to: "b.txt".into(),
            directory: false,
        })
        .await;
        // A thaw named the destination, so SWFS lists nothing.
        assert!(tree.rebuild(&[], None).await.unwrap());
        // Listed again later, as from a rename the record never heard of, it has no origin.
        assert!(
            !tree
                .rebuild(&[listed(r"\b.txt", RENAMED, false)], None)
                .await
                .unwrap()
        );
    })
    .await;
}

/// A revision holding `files`, with the directories leading to them.
/// The tree a mount serves from a revision holding `files`.
async fn revision_holding(tree: &ModifiedTree, files: &[&str]) -> ServedTree {
    let state = State::new();
    for path in files {
        let mut parent = ROOT_NODE;
        let segments: Vec<&str> = path.split('/').collect();
        for (index, segment) in segments.iter().enumerate() {
            let name_hash = lore_storage::hash::hash_string(segment);
            if let Ok(existing) = state
                .find_subnode(tree.repository(), parent, name_hash)
                .await
            {
                parent = existing;
                continue;
            }
            let node = Node {
                flags: if index + 1 == segments.len() {
                    NodeFlags::File.bits()
                } else {
                    0
                },
                name_hash,
                ..Default::default()
            };
            parent = state
                .node_add(tree.repository(), parent, node, segment)
                .await
                .expect("adding a revision node");
        }
    }
    ServedTree::new(tree.repository(), Some(state), Vec::new())
}

#[tokio::test]
async fn a_rebuild_takes_a_path_swfs_calls_new_as_the_revision_has_it_since() {
    with_tree(async |tree| {
        // Created through the mount and committed since, so SWFS still calls them new.
        let revision = revision_holding(&tree, &["a/b/c/leaf.txt", "a/b/c/sibling.txt"]).await;

        let trusted = tree
            .rebuild(
                &[
                    listed(r"\a", 0x0001, true),
                    listed(r"\a\b", 0x0001, true),
                    listed(r"\a\b\c", 0x0001, true),
                    listed(r"\a\b\c\sibling.txt", CREATED, false),
                    listed(r"\a\b\c\leaf.txt", 0x614b, false),
                    listed(r"\a\b\c\new.txt", CREATED, false),
                ],
                Some(&revision),
            )
            .await
            .unwrap();

        assert!(trusted);
        // The directories are not recorded as added, which would cover the deletion below them.
        assert_eq!(
            tree.coverage("a/b/c/leaf.txt").await.unwrap(),
            Coverage::Deleted
        );
        assert_eq!(
            tree.coverage("a/b/c/untouched.txt").await.unwrap(),
            Coverage::Unchanged
        );
        assert_eq!(
            tree.coverage("a/b/c/sibling.txt").await.unwrap(),
            Coverage::Changed
        );
        // A path the revision lacks is still an add.
        assert!(node_of(&tree, "a/b/c/new.txt").await.is_dirty_add());
        assert!(!node_of(&tree, "a/b/c/sibling.txt").await.is_dirty_add());
    })
    .await;
}
