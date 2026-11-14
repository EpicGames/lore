// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures build working-tree state directly; what these test is how the commit reads it.
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::runtime::runtime;
use lore_base::types::Hash;
use lore_base::types::TypedBytes;
use lore_revision::branch;
use lore_revision::change::FileAction;
use lore_revision::commit::*;
use lore_revision::filter::Filter;
use lore_revision::fs::filesystem_provider::CaseLookup;
use lore_revision::fs::filesystem_provider::FilesystemProvider;
use lore_revision::fs::filesystem_provider::FsError;
use lore_revision::fs::filesystem_provider::InstanceOperationImpl;
use lore_revision::fs::hold;
use lore_revision::fs::os::OsFilesystem;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreString;
use lore_revision::layer;
use lore_revision::layer::Layer;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::node::NodeDelta;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::state::State;
use serde::Serialize;

use crate::dirty_move_fixture::assert_addresses;
use crate::dirty_move_fixture::committed;
use crate::dirty_move_fixture::dirtied;
use crate::dirty_move_fixture::holds;
use crate::dirty_move_fixture::node_at;
use crate::dirty_move_fixture::node_id_at;
use crate::dirty_move_fixture::node_of;
use crate::dirty_move_fixture::removed_directory;
use crate::dirty_move_fixture::restaged;
use crate::dirty_move_fixture::stage_path;
use crate::dirty_move_fixture::staged_moved;
use crate::dirty_move_fixture::staged_tree;
use crate::dirty_move_fixture::tree_paths;
use crate::dirty_move_fixture::with_instance;
use crate::dirty_move_fixture::written;
use crate::fs::filesystem_provider::TestFilesystemProvider;
use crate::fs::filesystem_provider::setup_test_execution;
use crate::fs::filesystem_provider::test_execution_with;
use crate::fs::filesystem_provider::test_repository;
use crate::fs::filesystem_provider::test_store_create;
use crate::helper::RepositoryContextCreationArgsExt;
use crate::helper::TestRepository;
use crate::helper::default_repository_creation_args;
use crate::helper::mount_intermediate_at;
use crate::helper::test_commit as committed_state;

/// One commit reads the working tree through the one operation it opens, whatever it
/// commits: a provider that freezes hands out one snapshot, and a second opened partway
/// would fragment some of the files against a tree the rest were never measured against.
///
/// The count is taken from the commit alone, the staging that precedes it opening its own.
#[tokio::test]
async fn one_call_reads_the_working_tree_through_one_operation() {
    let dir = lore_base::test_util::TempDir::new("lore-commit-test-");
    std::fs::write(dir.path().join("one.txt"), b"one").expect("write file");
    std::fs::write(dir.path().join("two.txt"), b"two").expect("write file");
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    let root = dir.to_path_buf();

    let begins = runtime()
        .spawn(LORE_CONTEXT.scope(execution, async move {
            let fixture = counting_repository(&root, immutable_store, mutable_store).await;
            stage_working_tree(&fixture, &root).await;
            fixture.begins.store(0, Ordering::Release);

            commit_staged(&fixture.repository, &fixture.token, test_commit())
                .await
                .expect("Committing");

            fixture.begins.load(Ordering::Acquire)
        }))
        .await
        .expect("Test task failed");

    assert_eq!(
        1, begins,
        "The commit opened an operation beyond the one it reads the working tree through"
    );
}

/// A commit of a staged edit whose file the working tree no longer holds takes the file's
/// deletion, as it takes what the working tree holds at a staged path: the revision lacks the file,
/// and its delta records the deletion.
#[tokio::test]
async fn a_commit_deletes_a_staged_edit_the_working_tree_no_longer_holds() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["f.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        restaged(&instance, "f.txt").await;
        std::fs::remove_file(instance.path.join("f.txt")).expect("the delete on disk");

        let revision = committed_state(&instance, "Deleted").await;
        let kept = tree_paths(&instance, &revision).await;
        assert_eq!(kept.keys().collect::<Vec<_>>(), ["other.txt"]);
        assert_eq!(kept["other.txt"], held["other.txt"]);
        assert_eq!(
            delta_of(&instance, &revision).await,
            [(held["f.txt"], FileAction::Delete as u16)]
        );
    })
    .await;
}

/// A commit of a staged add whose file the working tree no longer holds commits nothing for it:
/// the revision holds the committed directory it was added to unchanged and the other staged
/// change, and its delta records nothing of the add.
#[tokio::test]
async fn a_commit_drops_a_staged_add_the_working_tree_no_longer_holds() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["c/keep.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        written(&instance, &["c/new.txt"]);
        let staged = stage_path(&instance, "c/new.txt").await;
        let added = node_id_at(&instance, &staged, "c/new.txt").await;
        std::fs::remove_file(instance.path.join("c/new.txt")).expect("the delete on disk");
        restaged(&instance, "other.txt").await;

        let revision = committed_state(&instance, "Edited").await;
        assert_eq!(tree_paths(&instance, &revision).await, held);
        assert_eq!(
            node_of(&instance, &revision, held["c"]).await.address,
            node_of(&instance, &first, held["c"]).await.address
        );
        let delta = delta_of(&instance, &revision).await;
        assert!(
            delta.iter().all(|(node, _)| *node != added),
            "the delta records the add: {delta:?}"
        );
    })
    .await;
}

/// A commit of a staged edit, and of a staged add, whose path the working tree holds a directory
/// at takes the file as gone: the edited file's deletion, and nothing of the add. The directory
/// stays on disk untracked, for a scan or a stage to take as a change of kind.
#[tokio::test]
async fn a_commit_takes_a_staged_file_whose_path_holds_a_directory_as_gone() {
    for added in [false, true] {
        with_instance(|instance| async move {
            let first = committed(&instance, &["a.txt", "other.txt"]).await;
            let held = tree_paths(&instance, &first).await;
            let path = if added {
                written(&instance, &["n.txt"]);
                stage_path(&instance, "n.txt").await;
                restaged(&instance, "other.txt").await;
                "n.txt"
            } else {
                restaged(&instance, "a.txt").await;
                "a.txt"
            };
            std::fs::remove_file(instance.path.join(path)).expect("the delete on disk");
            written(&instance, &[&format!("{path}/inside.txt")]);

            let revision = committed_state(&instance, "Replaced").await;
            let kept = tree_paths(&instance, &revision).await;
            if added {
                assert_eq!(kept, held, "the revision's paths");
            } else {
                assert_eq!(kept.keys().collect::<Vec<_>>(), ["other.txt"]);
                assert_eq!(
                    delta_of(&instance, &revision).await,
                    [(held["a.txt"], FileAction::Delete as u16)]
                );
            }
            let (_, staged, _) = State::deserialize_current_and_staged(instance.repository.clone())
                .await
                .expect("the states");
            if let Some(staged) = staged {
                assert!(
                    !holds(&instance, &staged, path).await,
                    "added {added}: the staged tree tracks {path}"
                );
            }
            assert!(
                instance.path.join(path).join("inside.txt").is_file(),
                "added {added}: the directory left the working tree"
            );
        })
        .await;
    }
}

/// A commit of a staged move whose file the working tree holds at its committed path again, and
/// at the moved path no longer, leaves the move out: the revision holds the file where it was
/// committed, with its committed content, and records nothing of it.
#[tokio::test]
async fn a_commit_leaves_out_a_staged_move_whose_file_went_back() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["a.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        staged_moved(&instance, "a.txt", "b.txt").await;
        std::fs::rename(instance.path.join("b.txt"), instance.path.join("a.txt"))
            .expect("the move back on disk");
        std::fs::write(instance.path.join("a.txt"), b"edited after the move back")
            .expect("the edit on disk");
        restaged(&instance, "other.txt").await;

        let revision = committed_state(&instance, "Edited").await;
        assert_eq!(tree_paths(&instance, &revision).await, held);
        assert_eq!(
            node_of(&instance, &revision, held["a.txt"]).await.address,
            node_of(&instance, &first, held["a.txt"]).await.address
        );
        let delta = delta_of(&instance, &revision).await;
        assert!(
            delta.iter().all(|(node, _)| *node != held["a.txt"]),
            "the delta records the moved file: {delta:?}"
        );
    })
    .await;
}

/// What stands at a staged-moved directory's committed path once the directory left its new one,
/// and what else changed below it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AtSource {
    /// The directory, moved back.
    MovedBack,
    /// The directory moved back, a file in it edited since.
    EditedAfter,
    /// The directory moved back in another case, which Linux tells apart.
    OtherCase,
    /// The directory moved back with a file added and staged in it after the move.
    AddedInside,
    /// The directory moved back with a directory holding a file added and staged in it after the
    /// move.
    AddedDirectoryInside,
    /// The directory moved back with a copy of a file made and staged in it after the move, which a
    /// stage takes as an add.
    CopiedInside,
    /// The directory moved back with a subdirectory a staged move renamed inside it, renamed back.
    RenamedInside,
    /// The directory moved back with a directory of equal depth a staged move took into it, moved
    /// back to its own place.
    MovedInAndBack,
    /// The directory moved back with a file and a directory staged moves took into it, which stay
    /// inside it.
    MovedInAndKept,
    /// The directory moved back with a file in it staged for deletion.
    DeletedInside,
    /// The directory moved back with a file a staged move took out of it.
    MovedOut,
    /// The directory moved back with a subdirectory a staged move renamed inside it, kept so.
    RenamedInsideKept,
    /// A file.
    File,
    /// A directory staged as added there since, which holds the name in the staged tree.
    StagedDirectory,
}

/// A commit of a staged directory move whose directory the working tree holds at its committed path
/// again, and at its moved path no longer, leaves the move out: the directory goes back with what
/// moved with it, as committed, and the delta records no change of theirs, a file edited since
/// keeping its committed content. A directory that moved inside it and went back too returns to
/// its own place, whatever order the two are met in. What came into it otherwise leaves: a file or
/// a directory a separate move brought in, and a subdirectory renamed inside it that stays so. A
/// staged add or copy in it, a directory added with what it holds among them, is left out and stays
/// marked dirty alone at the place it went back to, so the directory takes no staged mark past the
/// commit, and a stage of the directory and a commit then take it. A staged deletion in it is
/// committed, and a file a staged move took out of it stays where the move took it. Where a file or
/// a staged node stands at the committed path, the moved directory is gone, and the commit deletes
/// it with what it holds. Every directory holds the address its children hash to.
#[tokio::test]
async fn a_commit_leaves_out_a_staged_directory_move_whose_directory_went_back() {
    let mut cases = vec![
        AtSource::MovedBack,
        AtSource::EditedAfter,
        AtSource::AddedInside,
        AtSource::AddedDirectoryInside,
        AtSource::CopiedInside,
        AtSource::RenamedInside,
        AtSource::MovedInAndBack,
        AtSource::MovedInAndKept,
        AtSource::DeletedInside,
        AtSource::MovedOut,
        AtSource::RenamedInsideKept,
        AtSource::File,
        AtSource::StagedDirectory,
    ];
    if cfg!(target_os = "linux") {
        cases.push(AtSource::OtherCase);
    }
    for at_source in cases {
        with_instance(|instance| async move {
            let first = committed(
                &instance,
                &["d/k.txt", "d/s/g.txt", "m/h.txt", "x.txt", "other.txt"],
            )
            .await;
            let held = tree_paths(&instance, &first).await;
            match at_source {
                AtSource::RenamedInside | AtSource::RenamedInsideKept => {
                    staged_moved(&instance, "d/s", "d/t").await;
                }
                AtSource::MovedInAndBack | AtSource::MovedInAndKept => {
                    staged_moved(&instance, "m", "d/m").await;
                }
                _ => {}
            }
            staged_moved(&instance, "d", "e").await;
            match at_source {
                AtSource::AddedInside => {
                    written(&instance, &["e/new.txt"]);
                    stage_path(&instance, "e/new.txt").await;
                }
                AtSource::AddedDirectoryInside => {
                    written(&instance, &["e/n/f.txt"]);
                    stage_path(&instance, "e/n").await;
                }
                AtSource::CopiedInside => {
                    std::fs::copy(instance.path.join("x.txt"), instance.path.join("e/x2.txt"))
                        .expect("the copy on disk");
                    lore_revision::file::dirty::dirty_copy(
                        instance.repository.clone(),
                        instance.path.join("x.txt").to_string_lossy().into_owned(),
                        instance
                            .path
                            .join("e/x2.txt")
                            .to_string_lossy()
                            .into_owned(),
                    )
                    .await
                    .expect("the dirty copy");
                    stage_path(&instance, "e/x2.txt").await;
                }
                AtSource::MovedInAndKept => {
                    staged_moved(&instance, "x.txt", "e/x.txt").await;
                }
                AtSource::DeletedInside => {
                    std::fs::remove_file(instance.path.join("e/k.txt")).expect("the removal");
                    stage_path(&instance, "e/k.txt").await;
                }
                AtSource::MovedOut => {
                    staged_moved(&instance, "e/k.txt", "k.txt").await;
                }
                _ => {}
            }
            match at_source {
                AtSource::OtherCase => {
                    std::fs::rename(instance.path.join("e"), instance.path.join("D"))
                        .expect("the move back on disk");
                }
                AtSource::File => {
                    removed_directory(&instance, "e");
                    std::fs::write(instance.path.join("d"), b"a file").expect("the file on disk");
                }
                AtSource::StagedDirectory => {
                    removed_directory(&instance, "e");
                    written(&instance, &["d/n.txt"]);
                    stage_path(&instance, "d/n.txt").await;
                }
                _ => {
                    std::fs::rename(instance.path.join("e"), instance.path.join("d"))
                        .expect("the move back on disk");
                }
            }
            match at_source {
                AtSource::EditedAfter => {
                    std::fs::write(instance.path.join("d/k.txt"), b"edited after the move back")
                        .expect("the edit on disk");
                }
                AtSource::RenamedInside => {
                    std::fs::rename(instance.path.join("d/t"), instance.path.join("d/s"))
                        .expect("the rename back on disk");
                }
                AtSource::MovedInAndBack => {
                    std::fs::rename(instance.path.join("d/m"), instance.path.join("m"))
                        .expect("the move back on disk");
                }
                _ => {}
            }
            restaged(&instance, "other.txt").await;

            let revision = committed_state(&instance, "Edited").await;
            let kept = tree_paths(&instance, &revision).await;
            let delta = delta_of(&instance, &revision).await;
            let left: &[&str] = match at_source {
                AtSource::MovedInAndKept => &["m", "m/h.txt", "x.txt"],
                AtSource::DeletedInside => &["d/k.txt"],
                AtSource::RenamedInsideKept => &["d/s", "d/s/g.txt"],
                AtSource::File => &["d", "d/k.txt", "d/s", "d/s/g.txt"],
                _ => &[],
            };
            let mut expected = held.clone();
            for path in left {
                expected.remove(*path);
            }
            if at_source == AtSource::MovedOut {
                let moved = expected.remove("d/k.txt").expect("d/k.txt");
                expected.insert("k.txt".to_string(), moved);
            }
            assert_addresses(&instance, &revision, &kept).await;
            match at_source {
                AtSource::File => {
                    assert_eq!(kept, expected, "{at_source:?}");
                }
                AtSource::StagedDirectory => {
                    assert_eq!(
                        kept.keys().collect::<Vec<_>>(),
                        ["d", "d/n.txt", "m", "m/h.txt", "other.txt", "x.txt"]
                    );
                    assert_ne!(kept["d"], held["d"], "the moved directory stays");
                }
                _ => {
                    assert_eq!(kept, expected, "{at_source:?}");
                    if at_source != AtSource::DeletedInside {
                        assert_eq!(
                            node_of(&instance, &revision, held["d/k.txt"]).await.address,
                            node_of(&instance, &first, held["d/k.txt"]).await.address,
                            "{at_source:?}"
                        );
                    }
                    let deleted: Vec<NodeID> = left.iter().map(|path| held[*path]).collect();
                    let directories: Vec<NodeID> =
                        ["d", "d/s", "m"].iter().map(|path| held[*path]).collect();
                    let recorded = |&(node, action): &(NodeID, u16)| {
                        node == held["other.txt"]
                            || deleted.contains(&node)
                            || (directories.contains(&node) && action == FileAction::Keep as u16)
                            || (at_source == AtSource::MovedOut && node == held["d/k.txt"])
                    };
                    assert!(
                        delta.iter().all(recorded),
                        "{at_source:?}: the delta records {delta:?}"
                    );
                }
            }
            if at_source == AtSource::EditedAfter {
                let marked = dirtied(&instance, "d/k.txt").await;
                let node = node_at(&instance, &marked, "d/k.txt").await;
                assert!(
                    node.is_dirty_modify(),
                    "a mark misses the edit, flags {:#x}",
                    node.flags
                );
            }
            let kept: &[&str] = match at_source {
                AtSource::AddedInside => &["d/new.txt"],
                AtSource::AddedDirectoryInside => &["d/n", "d/n/f.txt"],
                AtSource::CopiedInside => &["d/x2.txt"],
                _ => &[],
            };
            if !kept.is_empty() {
                assert_kept_marked_dirty(&instance, at_source, kept).await;
            }
        })
        .await;
    }
}

/// Asserts that the staged tree past a commit holds each of `kept`, the staged adds a directory that
/// went back held, a copy staged as an add among them, marked dirty as added and not staged, with
/// no staged mark on the directory; that the dirty paths a status lists hold the files; and that a
/// stage of the directory, which takes what is marked dirty, and a commit take them.
async fn assert_kept_marked_dirty(instance: &TestRepository, at_source: AtSource, kept: &[&str]) {
    let staged = staged_tree(instance).await;
    let returned = node_at(instance, &staged, "d").await;
    assert!(
        !returned.is_staged(),
        "{at_source:?}: d keeps no staged mark, flags {:#x}",
        returned.flags
    );
    for path in kept {
        let node = node_at(instance, &staged, path).await;
        assert!(
            node.is_dirty_add() && !node.is_staged(),
            "{at_source:?}: {path}, flags {:#x}",
            node.flags
        );
    }
    let listed: Vec<String> = staged
        .collect_dirty_paths(
            instance.repository.clone(),
            ROOT_NODE,
            lore_revision::util::path::RelativePath::new(),
        )
        .await
        .expect("the dirty paths")
        .iter()
        .map(|path| path.as_str().to_string())
        .collect();
    let file = kept.last().expect("a kept file");
    assert!(
        listed.iter().any(|path| path == file),
        "{at_source:?}: the paths a status lists hold {file}: {listed:?}"
    );
    lore_revision::file::stage::stage(
        instance.repository.clone(),
        &instance.write_token,
        LoreArray::from_vec(vec![LoreString::from(&instance.path.join("d"))]),
        lore_revision::stage::StageOptions::default(),
    )
    .await
    .expect("the stage of d");
    let revision = committed_state(instance, "Kept").await;
    let held = tree_paths(instance, &revision).await;
    for path in kept {
        assert!(
            held.contains_key(*path),
            "{at_source:?}: the commit takes {path}: {held:?}"
        );
    }
    assert_addresses(instance, &revision, &held).await;
}

/// What a directory holds that the working tree lacks without its having been deleted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeldOnlyBy {
    /// A link's mount, `lib`.
    Link,
    /// `private`, which the view excludes.
    View,
}

/// The repository `instance` commits through for `held_by`: its own, or one whose view excludes
/// `**/private`.
fn committing_through(instance: &TestRepository, held_by: HeldOnlyBy) -> Arc<RepositoryContext> {
    if held_by == HeldOnlyBy::Link {
        return instance.repository.clone();
    }
    let mut filter = (*instance.repository.filter).clone();
    filter
        .view
        .add_exclusion("**/private")
        .expect("A view line");
    Arc::new(instance.repository.to_filter_context(Arc::new(filter)))
}

/// Commits `d/k.txt` and `other.txt` with `d/lib`, a link's mount, or `d/private/p.txt`, as
/// `held_by` says, then stages the move of `d` to `e`, which the working tree holds moved. The
/// working tree then lacks `e/private`, as the view excludes it. Answers the committed paths.
async fn moved_holding(instance: &TestRepository, held_by: HeldOnlyBy) -> BTreeMap<String, NodeID> {
    let first = match held_by {
        HeldOnlyBy::Link => {
            committed(instance, &["d/k.txt", "other.txt"]).await;
            mount_intermediate_at(
                &instance.repository,
                "d/lib",
                RepositoryId::from(uuid::Uuid::now_v7()),
                true,
            )
            .await;
            written(instance, &["d/lib/src/Intermediate"]);
            committed_state(instance, "Linked").await
        }
        HeldOnlyBy::View => committed(instance, &["d/k.txt", "d/private/p.txt", "other.txt"]).await,
    };
    let held = tree_paths(instance, &first).await;
    staged_moved(instance, "d", "e").await;
    if held_by == HeldOnlyBy::View {
        removed_directory(instance, "e/private");
    }
    held
}

/// A commit of a staged directory move whose directory went back to its committed path leaves the
/// move out where the directory holds a link's mount or what the view excludes, which keep a
/// directory gone from the working tree held: it goes back with what it holds, the link's mount
/// with it, and the revision holds nothing at the moved path. Where its committed path is not held
/// either, the directory stays held at the moved path, and the move is committed with the deletion
/// of the files the working tree holds there no longer.
#[tokio::test]
async fn a_commit_returns_a_moved_directory_held_only_by_a_link_or_the_view() {
    for held_by in [HeldOnlyBy::Link, HeldOnlyBy::View] {
        for went_back in [true, false] {
            with_instance(|instance| async move {
                let held = moved_holding(&instance, held_by).await;
                let to = if went_back { "d" } else { "f" };
                std::fs::rename(instance.path.join("e"), instance.path.join(to))
                    .expect("the rename on disk");
                restaged(&instance, "other.txt").await;

                let through = committing_through(&instance, held_by);
                let revision = committed_through(&instance, &through, "Moved").await;
                let case = format!("{held_by:?}, went back {went_back}");
                let at = if went_back { "d" } else { "e" };
                let expected: BTreeMap<String, NodeID> = held
                    .iter()
                    .filter(|(path, _)| {
                        went_back || !matches!(path.as_str(), "d/k.txt" | "d/private/p.txt")
                    })
                    .map(|(path, node)| {
                        let path = match path.strip_prefix("d") {
                            Some(rest) => format!("{at}{rest}"),
                            None => path.clone(),
                        };
                        (path, *node)
                    })
                    .collect();
                let kept = tree_paths(&instance, &revision).await;
                assert_eq!(kept, expected, "{case}: the revision's paths");
                assert_addresses(&instance, &revision, &kept).await;
                if held_by == HeldOnlyBy::Link {
                    let links = revision
                        .link_list(instance.repository.clone())
                        .await
                        .expect("the link registry");
                    assert_eq!(
                        links
                            .iter()
                            .map(|link| link.local_node())
                            .collect::<Vec<_>>(),
                        [held["d/lib"]],
                        "{case}: the registry holds the link"
                    );
                }
            })
            .await;
        }
    }
}

/// A commit of a staged directory move whose directory went back to its committed path is refused
/// where a link with a staged change lies inside it, whose commit would read the linked tree at the
/// moved path: the staged tree and the linked repository's branch are as they were.
#[tokio::test]
async fn a_commit_refuses_a_staged_link_inside_a_moved_directory_that_went_back() {
    with_instance(|instance| async move {
        let linked = RepositoryId::from(uuid::Uuid::now_v7());
        committed(&instance, &["d/k.txt", "other.txt"]).await;
        mount_intermediate_at(&instance.repository, "d/lib", linked, true).await;
        written(&instance, &["d/lib/src/Intermediate"]);
        committed_state(&instance, "Linked").await;
        written(&instance, &["d/lib/src/new.txt"]);
        stage_path(&instance, "d/lib/src/new.txt").await;
        let staged = staged_tree(&instance).await;
        let link = node_at(&instance, &staged, "d/lib").await;
        assert!(link.is_staged(), "d/lib: flags {:#x}", link.flags);
        staged_moved(&instance, "d", "e").await;
        std::fs::rename(instance.path.join("e"), instance.path.join("d"))
            .expect("the rename on disk");
        let (_, branch) = lore_revision::instance::load_current_anchor_boxed(&instance.repository)
            .await
            .expect("the current anchor");
        let linked_context = instance.repository.to_link_context(linked).await;
        let latest = async || {
            branch::load_latest(linked_context.clone(), branch)
                .await
                .ok()
        };
        let linked_before = latest().await;
        let staged_before = staged_tree(&instance).await.revision();

        let refused = commit_boxed(
            instance.repository.clone(),
            &instance.write_token,
            CommitOptions::new("Moved".to_string()),
        )
        .await;
        assert!(
            refused.as_ref().is_err_and(|error| error.is_internal()
                && error.to_string().contains(
                    "a staged link lies inside a directory moved back to its committed path; \
                     stage the move back or unstage it"
                )),
            "the commit: {:?}",
            refused.map(|_| ())
        );
        assert_eq!(
            staged_tree(&instance).await.revision(),
            staged_before,
            "the staged tree"
        );
        assert_eq!(latest().await, linked_before, "the linked branch");
    })
    .await;
}

/// A commit of a staged directory move whose directory the working tree no longer holds deletes
/// each file the move took, and the directory with them.
#[tokio::test]
async fn a_commit_deletes_a_staged_directory_move_gone_from_the_working_tree() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["d/x.txt", "d/y.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        staged_moved(&instance, "d", "e").await;
        std::fs::remove_dir_all(instance.path.join("e")).expect("the delete on disk");
        restaged(&instance, "other.txt").await;

        let revision = committed_state(&instance, "Deleted").await;
        let kept = tree_paths(&instance, &revision).await;
        assert_eq!(kept.keys().collect::<Vec<_>>(), ["other.txt"]);
        assert_deletes(
            &instance,
            &revision,
            &[held["d"], held["d/x.txt"], held["d/y.txt"]],
        )
        .await;
    })
    .await;
}

/// A commit of a staged directory add whose files the working tree no longer holds, while it
/// holds the directory, takes the directory added and empty; one the working tree no longer holds
/// either it leaves out.
#[tokio::test]
async fn a_commit_takes_a_staged_directory_add_as_the_working_tree_holds_it() {
    for gone in [false, true] {
        with_instance(|instance| async move {
            let first = committed(&instance, &["other.txt"]).await;
            let other = node_id_at(&instance, &first, "other.txt").await;
            written(&instance, &["n/a.txt"]);
            stage_path(&instance, "n").await;
            if gone {
                std::fs::remove_dir_all(instance.path.join("n")).expect("the delete on disk");
            } else {
                std::fs::remove_file(instance.path.join("n/a.txt")).expect("the delete on disk");
            }
            restaged(&instance, "other.txt").await;

            let revision = committed_state(&instance, "Edited").await;
            let kept = tree_paths(&instance, &revision).await;
            assert_eq!(kept["other.txt"], other);
            let expected: &[&str] = if gone {
                &["other.txt"]
            } else {
                &["n", "other.txt"]
            };
            assert_eq!(kept.keys().collect::<Vec<_>>(), expected, "gone {gone}");
        })
        .await;
    }
}

/// A commit of staged edits in a committed directory the working tree no longer holds deletes the
/// directory with everything below it, as it does where a file stands at its path, which stays
/// on disk, unmarked.
#[tokio::test]
async fn a_commit_deletes_a_directory_removed_after_edits_in_it_were_staged() {
    for replaced in [false, true] {
        with_instance(|instance| async move {
            let first = committed(&instance, &["a/x.txt", "a/y.txt", "other.txt"]).await;
            let held = tree_paths(&instance, &first).await;
            restaged(&instance, "a/x.txt").await;
            restaged(&instance, "a/y.txt").await;
            removed_directory(&instance, "a");
            if replaced {
                std::fs::write(instance.path.join("a"), b"a file").expect("the file on disk");
            }

            let revision = committed_state(&instance, "Removed").await;
            let kept = tree_paths(&instance, &revision).await;
            assert_eq!(
                kept.keys().collect::<Vec<_>>(),
                ["other.txt"],
                "replaced {replaced}"
            );
            assert_deletes(
                &instance,
                &revision,
                &[held["a"], held["a/x.txt"], held["a/y.txt"]],
            )
            .await;
            if replaced {
                let (_, staged, _) =
                    State::deserialize_current_and_staged(instance.repository.clone())
                        .await
                        .expect("the states");
                assert!(
                    staged.is_none(),
                    "the commit left the file at the path marked"
                );
                assert_eq!(
                    std::fs::read(instance.path.join("a")).expect("the file on disk"),
                    b"a file"
                );
            }
        })
        .await;
    }
}

/// A commit of a staged edit in a committed directory the working tree no longer holds deletes
/// the committed file beside it that was not staged, with the directory.
#[tokio::test]
async fn a_commit_deletes_a_removed_directory_with_a_file_in_it_not_staged() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["a/x.txt", "a/y.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        restaged(&instance, "a/x.txt").await;
        removed_directory(&instance, "a");

        let revision = committed_state(&instance, "Removed").await;
        let kept = tree_paths(&instance, &revision).await;
        assert_eq!(kept.keys().collect::<Vec<_>>(), ["other.txt"]);
        assert_deletes(
            &instance,
            &revision,
            &[held["a"], held["a/x.txt"], held["a/y.txt"]],
        )
        .await;
    })
    .await;
}

/// A commit of a staged deletion in a committed directory the working tree no longer holds deletes
/// the directory with everything below it, the staged deletion recorded once.
#[tokio::test]
async fn a_commit_deletes_a_removed_directory_that_held_only_a_staged_deletion() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["a/x.txt", "a/y.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        std::fs::remove_file(instance.path.join("a/x.txt")).expect("the delete on disk");
        stage_path(&instance, "a/x.txt").await;
        removed_directory(&instance, "a");

        let revision = committed_state(&instance, "Removed").await;
        let kept = tree_paths(&instance, &revision).await;
        assert_eq!(kept.keys().collect::<Vec<_>>(), ["other.txt"]);
        assert_deletes(
            &instance,
            &revision,
            &[held["a"], held["a/x.txt"], held["a/y.txt"]],
        )
        .await;
    })
    .await;
}

/// A commit of staged edits in a committed directory and in one below it, both gone from the
/// working tree, deletes the upper one with everything below it.
#[tokio::test]
async fn a_commit_deletes_a_removed_directory_with_a_removed_directory_in_it() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["a/b/x.txt", "a/z.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        restaged(&instance, "a/b/x.txt").await;
        restaged(&instance, "a/z.txt").await;
        removed_directory(&instance, "a");

        let revision = committed_state(&instance, "Removed").await;
        let kept = tree_paths(&instance, &revision).await;
        assert_eq!(kept.keys().collect::<Vec<_>>(), ["other.txt"]);
        assert_deletes(
            &instance,
            &revision,
            &[held["a"], held["a/b"], held["a/b/x.txt"], held["a/z.txt"]],
        )
        .await;
    })
    .await;
}

/// A commit of staged edits in a committed directory the working tree holds, with neither file,
/// deletes the files and keeps the directory.
#[tokio::test]
async fn a_commit_keeps_a_directory_the_working_tree_holds_without_its_staged_files() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["a/x.txt", "a/y.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        restaged(&instance, "a/x.txt").await;
        restaged(&instance, "a/y.txt").await;
        for file in ["a/x.txt", "a/y.txt"] {
            std::fs::remove_file(instance.path.join(file)).expect("the delete on disk");
        }

        let revision = committed_state(&instance, "Removed").await;
        let kept = tree_paths(&instance, &revision).await;
        assert_eq!(kept.keys().collect::<Vec<_>>(), ["a", "other.txt"]);
        assert_eq!(kept["a"], held["a"]);
        assert_deletes(&instance, &revision, &[held["a/x.txt"], held["a/y.txt"]]).await;
    })
    .await;
}

/// A commit of a staged edit in a committed directory the working tree no longer holds, where the
/// view excludes a directory below it, keeps the directory with what the view excludes and what
/// was not staged, and deletes the staged file alone.
#[tokio::test]
async fn a_commit_keeps_a_removed_directory_holding_what_the_view_excludes() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["a/hidden/h.txt", "a/x.txt", "a/y.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        restaged(&instance, "a/x.txt").await;
        removed_directory(&instance, "a");
        let mut filter = (*instance.repository.filter).clone();
        filter
            .view
            .add_exclusion("/a/hidden/")
            .expect("A view line");
        let sparse = Arc::new(instance.repository.to_filter_context(Arc::new(filter)));

        let revision = committed_through(&instance, &sparse, "Removed").await;
        let kept = tree_paths(&instance, &revision).await;
        assert_eq!(
            kept.keys().collect::<Vec<_>>(),
            ["a", "a/hidden", "a/hidden/h.txt", "a/y.txt"]
        );
        assert_deletes(&instance, &revision, &[held["a/x.txt"]]).await;
    })
    .await;
}

/// A commit of a staged deletion in a committed directory the working tree holds under another
/// case keeps the directory with what else it holds, on a file system that folds no case.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_commit_keeps_a_directory_the_working_tree_holds_in_another_case() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["Dir/a.txt", "Dir/b.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        std::fs::remove_file(instance.path.join("Dir/a.txt")).expect("the delete on disk");
        stage_path(&instance, "Dir/a.txt").await;
        std::fs::rename(instance.path.join("Dir"), instance.path.join("dir"))
            .expect("the rename on disk");

        let revision = committed_state(&instance, "Deleted").await;
        let kept = tree_paths(&instance, &revision).await;
        assert_eq!(
            kept.keys().collect::<Vec<_>>(),
            ["Dir", "Dir/b.txt", "other.txt"]
        );
        assert_deletes(&instance, &revision, &[held["Dir/a.txt"]]).await;
    })
    .await;
}

/// A commit of a staged edit whose file the working tree holds under another case only is refused
/// as not found, as the file is not gone, on a file system that folds no case.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_commit_refuses_a_staged_file_the_working_tree_holds_in_another_case() {
    with_instance(|instance| async move {
        committed(&instance, &["Foo.txt", "other.txt"]).await;
        restaged(&instance, "Foo.txt").await;
        std::fs::rename(instance.path.join("Foo.txt"), instance.path.join("foo.txt"))
            .expect("the rename on disk");

        let refused = commit_boxed(
            instance.repository.clone(),
            &instance.write_token,
            CommitOptions::new("Edited".to_string()),
        )
        .await;
        assert!(
            refused
                .as_ref()
                .is_err_and(|error| error.is_file_not_found()),
            "the commit: {:?}",
            refused.map(|_| ())
        );
    })
    .await;
}

/// A commit of staged edits whose files the working tree no longer holds, on a file system that
/// folds no case, lists each of their directories once to tell them gone, rather than once for
/// each file, and deletes them.
#[tokio::test]
async fn a_commit_lists_each_directory_of_its_gone_files_once() {
    let dir = lore_base::test_util::TempDir::new("lore-commit-test-");
    let filesystem =
        Arc::new(TestFilesystemProvider::reading(dir.path()).with_case_lookup(CaseLookup::OnMiss));
    let gone = ["d/a.txt", "d/b.txt", "e/x.txt", "e/y.txt"];
    for file in gone.iter().chain(&["d/c.txt", "e/z.txt", "keep.txt"]) {
        let path = dir.path().join(file);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
        std::fs::write(path, b"one").expect("write file");
    }
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    let root = dir.to_path_buf();
    let counted = filesystem.clone();
    let (listed, folded) = runtime()
        .spawn(LORE_CONTEXT.scope(execution, async move {
            let (fixture, committing) = committing_repository(
                &root,
                immutable_store,
                mutable_store,
                counted.clone(),
                Arc::default(),
            )
            .await;
            stage_working_tree(&fixture, &root).await;
            commit_staged(&committing, &fixture.token, test_commit())
                .await
                .expect("the first commit");
            for file in gone {
                std::fs::write(root.join(file), b"two").expect("rewrite file");
            }
            stage_working_tree(&fixture, &root).await;
            for file in gone {
                std::fs::remove_file(root.join(file)).expect("the removal on disk");
            }
            let listed = counted.directory_reads.load(Ordering::Acquire);
            let folded = counted.names_folding_count.load(Ordering::Acquire);
            commit_staged(&committing, &fixture.token, test_commit())
                .await
                .expect("the commit of the gone files");
            (
                counted.directory_reads.load(Ordering::Acquire) - listed,
                counted.names_folding_count.load(Ordering::Acquire) - folded,
            )
        }))
        .await
        .expect("the test task");
    assert_eq!(
        (listed, folded),
        (2, 0),
        "directories listed and names looked up by their fold"
    );
}

/// A commit of staged edits whose files the working tree no longer holds, in sibling directories it
/// removed, on a file system that folds no case, lists their parent once to resolve the siblings'
/// spellings and once to tell them gone, rather than once for each sibling, and deletes them.
#[tokio::test]
async fn a_commit_lists_the_parent_of_removed_siblings_once() {
    let dir = lore_base::test_util::TempDir::new("lore-commit-test-");
    let filesystem =
        Arc::new(TestFilesystemProvider::reading(dir.path()).with_case_lookup(CaseLookup::OnMiss));
    let gone = ["d/e1/a.txt", "d/e2/a.txt", "d/e3/a.txt"];
    for file in gone.iter().chain(&["d/k.txt", "keep.txt"]) {
        let path = dir.path().join(file);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
        std::fs::write(path, b"one").expect("write file");
    }
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    let root = dir.to_path_buf();
    let counted = filesystem.clone();
    let (listed, folded, kept) = runtime()
        .spawn(LORE_CONTEXT.scope(execution, async move {
            let (fixture, committing) = committing_repository(
                &root,
                immutable_store,
                mutable_store,
                counted.clone(),
                Arc::default(),
            )
            .await;
            stage_working_tree(&fixture, &root).await;
            commit_staged(&committing, &fixture.token, test_commit())
                .await
                .expect("the first commit");
            for file in gone {
                std::fs::write(root.join(file), b"two").expect("rewrite file");
            }
            stage_working_tree(&fixture, &root).await;
            for sibling in ["d/e1", "d/e2", "d/e3"] {
                std::fs::remove_dir_all(root.join(sibling)).expect("the removal on disk");
            }
            let listed = counted.directory_reads.load(Ordering::Acquire);
            let folded = counted.names_folding_count.load(Ordering::Acquire);
            commit_staged(&committing, &fixture.token, test_commit())
                .await
                .expect("the commit of the gone files");
            let current = State::deserialize(
                fixture.repository.clone(),
                lore_revision::instance::load_current_anchor_boxed(&fixture.repository)
                    .await
                    .expect("the current revision")
                    .0,
            )
            .await
            .expect("the revision");
            let kept: Vec<bool> = {
                let mut kept = Vec::new();
                for path in ["d/k.txt", "d/e1", "d/e2", "d/e3"] {
                    kept.push(
                        current
                            .find_node_link(fixture.repository.clone(), path)
                            .await
                            .is_ok(),
                    );
                }
                kept
            };
            (
                counted.directory_reads.load(Ordering::Acquire) - listed,
                counted.names_folding_count.load(Ordering::Acquire) - folded,
                kept,
            )
        }))
        .await
        .expect("the test task");
    assert_eq!(
        (listed, folded),
        (2, 0),
        "directories listed and names looked up by their fold"
    );
    assert_eq!(
        kept,
        [true, false, false, false],
        "d/k.txt, d/e1, d/e2, d/e3"
    );
}

/// A commit reads nothing more of a directory holding a staged deletion than the file it commits
/// beside it: reading that file showed the directory held.
#[tokio::test]
async fn a_commit_reads_no_directory_a_file_it_read_shows_held() {
    let dir = lore_base::test_util::TempDir::new("lore-commit-test-");
    let filesystem = Arc::new(TestFilesystemProvider::reading(dir.path()));
    for file in ["d/a.txt", "d/b.txt", "keep.txt"] {
        let path = dir.path().join(file);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
        std::fs::write(path, b"one").expect("write file");
    }
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    let root = dir.to_path_buf();
    let counted = filesystem.clone();
    let read = runtime()
        .spawn(LORE_CONTEXT.scope(execution, async move {
            let (fixture, committing) = committing_repository(
                &root,
                immutable_store,
                mutable_store,
                counted.clone(),
                Arc::default(),
            )
            .await;
            stage_working_tree(&fixture, &root).await;
            commit_staged(&committing, &fixture.token, test_commit())
                .await
                .expect("the first commit");
            std::fs::remove_file(root.join("d/a.txt")).expect("the removal on disk");
            std::fs::write(root.join("d/b.txt"), b"two").expect("rewrite file");
            stage_working_tree(&fixture, &root).await;
            let before = counted.file_infos();
            commit_staged(&committing, &fixture.token, test_commit())
                .await
                .expect("the commit");
            counted.file_infos() - before
        }))
        .await
        .expect("the test task");
    assert_eq!(read, 1, "file-information reads of the commit");
}

/// A commit of a staged empty directory the working tree no longer holds leaves it out where it
/// was staged as added, and deletes it where it was staged as moved.
#[tokio::test]
async fn a_commit_takes_a_staged_empty_directory_as_the_working_tree_holds_it() {
    for moved_directory in [false, true] {
        with_instance(|instance| async move {
            let first = committed(&instance, &["other.txt"]).await;
            let other = node_id_at(&instance, &first, "other.txt").await;
            std::fs::create_dir(instance.path.join("e")).expect("the directory on disk");
            stage_path(&instance, "e").await;
            let (gone, deleted) = if moved_directory {
                let revision = committed_state(&instance, "Added").await;
                let e = node_id_at(&instance, &revision, "e").await;
                staged_moved(&instance, "e", "f").await;
                ("f", vec![e])
            } else {
                ("e", Vec::new())
            };
            std::fs::remove_dir(instance.path.join(gone)).expect("the delete on disk");
            restaged(&instance, "other.txt").await;

            let revision = committed_state(&instance, "Removed").await;
            let kept = tree_paths(&instance, &revision).await;
            assert_eq!(
                kept.keys().collect::<Vec<_>>(),
                ["other.txt"],
                "moved {moved_directory}"
            );
            assert_eq!(kept["other.txt"], other);
            assert_deletes(&instance, &revision, &deleted).await;
        })
        .await;
    }
}

/// A commit of a staged move whose file the working tree no longer holds at its new path deletes
/// the file where nothing stands at its committed path, where a directory does, and where another
/// staged node holds that path; on a file system that folds no case, a file there in another case
/// takes it back.
#[tokio::test]
async fn a_commit_takes_a_staged_move_gone_from_its_new_path_as_its_committed_path_holds_it() {
    let at_source: &[&str] = if cfg!(target_os = "linux") {
        &["nothing", "directory", "staged", "case"]
    } else {
        &["nothing", "directory", "staged"]
    };
    for &source in at_source {
        with_instance(|instance| async move {
            let first = committed(&instance, &["a.txt", "other.txt"]).await;
            let held = tree_paths(&instance, &first).await;
            staged_moved(&instance, "a.txt", "b.txt").await;
            match source {
                "directory" => {
                    std::fs::create_dir(instance.path.join("a.txt")).expect("the directory");
                }
                "staged" => {
                    written(&instance, &["a.txt"]);
                    stage_path(&instance, "a.txt").await;
                }
                "case" => {
                    std::fs::rename(instance.path.join("b.txt"), instance.path.join("A.txt"))
                        .expect("the move back on disk");
                }
                _ => {}
            }
            if source != "case" {
                std::fs::remove_file(instance.path.join("b.txt")).expect("the delete on disk");
            }
            restaged(&instance, "other.txt").await;

            let revision = committed_state(&instance, "Edited").await;
            let kept = tree_paths(&instance, &revision).await;
            let (expected, deleted): (&[&str], Vec<NodeID>) = match source {
                "staged" => (&["a.txt", "other.txt"], vec![held["a.txt"]]),
                "case" => (&["a.txt", "other.txt"], Vec::new()),
                _ => (&["other.txt"], vec![held["a.txt"]]),
            };
            assert_eq!(
                kept.keys().collect::<Vec<_>>(),
                expected,
                "{source} at a.txt"
            );
            assert_eq!(
                kept.get("a.txt") == Some(&held["a.txt"]),
                source == "case",
                "{source} at a.txt"
            );
            assert_deletes(&instance, &revision, &deleted).await;
        })
        .await;
    }
}

/// Asserts that the delta of `revision` records the deletion of each of `nodes` once, and nothing
/// else of them.
async fn assert_deletes(instance: &TestRepository, revision: &State, nodes: &[NodeID]) {
    let delta = delta_of(instance, revision).await;
    let mut deleted: Vec<NodeID> = delta
        .iter()
        .filter(|(_, action)| *action == FileAction::Delete as u16)
        .map(|&(node, _)| node)
        .collect();
    deleted.sort_unstable();
    let mut expected = nodes.to_vec();
    expected.sort_unstable();
    assert_eq!(deleted, expected, "the deletions in the delta {delta:?}");
    assert!(
        delta
            .iter()
            .all(|(node, action)| *action == FileAction::Delete as u16 || !nodes.contains(node)),
        "the delta records more than the deletion of a deleted node: {delta:?}"
    );
}

/// Commits what `instance` holds staged through `repository`, a context of it, and answers the
/// revision.
async fn committed_through(
    instance: &TestRepository,
    repository: &Arc<RepositoryContext>,
    message: &str,
) -> Arc<State> {
    commit_boxed(
        repository.clone(),
        &instance.write_token,
        CommitOptions::new(message.to_string()),
    )
    .await
    .expect("Commit failed");
    let (revision, _) = lore_revision::instance::load_current_anchor_boxed(repository)
        .await
        .expect("the current anchor");
    State::deserialize(repository.clone(), revision)
        .await
        .expect("the committed state")
}

/// Each node the delta of `revision` records, with its action, in the order recorded.
async fn delta_of(instance: &TestRepository, revision: &State) -> Vec<(NodeID, u16)> {
    let delta = revision
        .delta_block(instance.repository.clone())
        .await
        .expect("the delta")
        .to_aligned::<NodeDelta>();
    delta
        .as_type_slice::<NodeDelta>()
        .iter()
        .map(|entry| (entry.node, entry.action))
        .collect()
}

/// The operation a commit read its files through, on a provider holding its state, is handed the
/// revision it stored once that is current, clean of staged and dirty nodes. A dry run leaves the
/// current revision where it was and hands nothing over.
#[tokio::test]
async fn the_operation_is_handed_the_committed_revision_and_nothing_on_a_dry_run() {
    let dir = lore_base::test_util::TempDir::new("lore-commit-test-");
    let filesystem = Arc::new(TestFilesystemProvider::holding_state_reading(dir.path()));

    let (signature, current) = commit_then_dry_run(&dir, filesystem.clone()).await;

    assert_eq!(
        *filesystem.committed_calls.lock(),
        [(signature, signature, true)],
        "The commit handed over another revision than it stored, before it was current, or not \
         clean, or a dry run handed one over"
    );
    assert_eq!(current, signature, "The dry run moved the current revision");
}

/// A commit stands when its operation fails to take it as committed.
#[tokio::test]
async fn a_failed_hand_over_leaves_the_commit_standing() {
    let dir = lore_base::test_util::TempDir::new("lore-commit-test-");
    let filesystem = Arc::new(TestFilesystemProvider::holding_state_failing_committed(
        dir.path(),
    ));

    let (signature, current) = commit_then_dry_run(&dir, filesystem.clone()).await;

    assert_eq!(
        *filesystem.committed_calls.lock(),
        [(signature, signature, true)],
        "The commit handed over another revision than it stored"
    );
    assert_eq!(current, signature, "The failed hand-over undid the commit");
}

/// A provider that does not hold its state is told of no content stored and handed no revision:
/// its working tree is the files.
#[tokio::test]
async fn a_provider_not_holding_its_state_is_handed_nothing() {
    let dir = lore_base::test_util::TempDir::new("lore-commit-test-");
    let filesystem = Arc::new(TestFilesystemProvider::reading(dir.path()));

    commit_then_dry_run(&dir, filesystem.clone()).await;

    assert!(filesystem.stored_calls.lock().is_empty());
    assert!(filesystem.committed_calls.lock().is_empty());
}

/// The operation a commit read its files through, on a provider holding its state, is told what
/// each file it read was stored as, the address, size and mode the revision records, before the
/// revision is handed over: a file the view excludes, staged from the working tree, included, and
/// one a merge staged past the view that a forced stage took from the working tree since, which
/// resolves the merge. A file a merge staged past the view and no stage took since, whose content
/// the commit takes from the staged node, is not told of. Each records the size of its content.
#[tokio::test]
async fn the_operation_is_told_what_each_file_it_read_was_stored_as_before_the_hand_over() {
    let dir = lore_base::test_util::TempDir::new("lore-commit-test-");
    let filesystem = Arc::new(TestFilesystemProvider::holding_state_reading(dir.path()));
    for file in ["one.txt", "excluded.txt", "merged.txt", "adopted.txt"] {
        std::fs::write(dir.path().join(file), b"one").expect("write file");
    }
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    let root = dir.to_path_buf();
    let committed = filesystem.clone();
    let mut filter = Filter::default();
    for file in ["excluded.txt", "merged.txt", "adopted.txt"] {
        filter.view.add_exclusion(file).expect("A view line");
    }
    let filter = Arc::new(filter);

    let (signature, read, sizes) = runtime()
        .spawn(LORE_CONTEXT.scope(execution, async move {
            let (fixture, committing) = committing_repository(
                &root,
                immutable_store,
                mutable_store,
                committed,
                filter.clone(),
            )
            .await;
            stage_working_tree(&fixture, &root).await;
            commit_staged(&fixture.repository, &fixture.token, test_commit())
                .await
                .expect("Committing through the OS filesystem");

            std::fs::write(root.join("one.txt"), b"two!").expect("rewrite file");
            stage_working_tree(&fixture, &root).await;
            stage_as_merged(&fixture, &["merged.txt", "adopted.txt"]).await;
            for file in ["excluded.txt", "merged.txt"] {
                std::fs::write(root.join(file), b"two!").expect("rewrite file");
            }
            let sparse = Arc::new(fixture.repository.to_filter_context(filter));
            LORE_CONTEXT
                .scope(
                    crate::helper::forced_execution(),
                    lore_revision::file::stage::stage(
                        sparse,
                        &fixture.token,
                        LoreArray::from_vec(vec![LoreString::from(&root)]),
                        lore_revision::stage::StageOptions {
                            scan: true,
                            ..Default::default()
                        },
                    ),
                )
                .await
                .expect("Forced staging of the working tree");
            let signature = commit_staged(&committing, &fixture.token, test_commit())
                .await
                .expect("Committing");
            let state = State::deserialize(committing.clone(), signature)
                .await
                .expect("The committed State");
            let mut read = Vec::new();
            let mut sizes = Vec::new();
            for file in ["adopted.txt", "excluded.txt", "merged.txt", "one.txt"] {
                let node = state
                    .find_node(committing.clone(), file)
                    .await
                    .expect("The committed file");
                let content = lore_revision::immutable::read(
                    committing.clone(),
                    node.address,
                    None,
                    lore_revision::immutable::read_options_from_repository(&committing),
                )
                .await
                .expect("The committed content");
                sizes.push((file, node.size, content.len() as u64));
                if file != "adopted.txt" {
                    read.push((
                        String::from(file),
                        node.address,
                        node.size,
                        node.mode,
                        false,
                    ));
                }
            }
            (signature, read, sizes)
        }))
        .await
        .expect("Test task failed");

    let mut stored = filesystem.stored_calls.lock().clone();
    stored.sort_by(|left, right| left.0.cmp(&right.0));
    assert_eq!(
        stored, read,
        "The commit told of other content than the revision records, of the file a merge staged \
         that no stage took since, or after the hand-over"
    );
    for (file, size, content) in sizes {
        assert_eq!(
            size, content,
            "{file} records another size than its content's"
        );
    }
    assert_eq!(
        *filesystem.committed_calls.lock(),
        [(signature, signature, true)]
    );
}

/// Marks each of `files` in `fixture`'s staged state as a merge stages it, with the content it
/// holds.
async fn stage_as_merged(fixture: &CountingRepository, files: &[&str]) {
    let repository = &fixture.repository;
    let (_, staged, _) = State::deserialize_current_and_staged(repository.clone())
        .await
        .expect("The current and staged States");
    let staged = staged.expect("A staged State");
    for file in files {
        let node = staged
            .find_node_link(repository.clone(), file)
            .await
            .expect("The staged file")
            .node;
        staged
            .node_mark(
                repository,
                node,
                lore_revision::state::Mark::staged(
                    lore_revision::node::NodeFlags::StagedMerge,
                    true,
                ),
            )
            .await
            .expect("Marking the file merged");
    }
    let signature = staged
        .serialize(repository.clone(), &fixture.token)
        .await
        .expect("Storing the staged State");
    lore_revision::instance::store_staged_anchor(repository, signature)
        .await
        .expect("Storing the staged anchor");
}

/// A commit of a link alone or of a layer is refused on a provider holding its state, which would
/// make a revision current that the provider is not handed.
#[tokio::test]
async fn a_commit_of_a_link_or_a_layer_alone_is_refused_on_a_provider_holding_its_state() {
    let dir = lore_base::test_util::TempDir::new("lore-commit-test-");
    let filesystem = Arc::new(TestFilesystemProvider::holding_state());
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let repository = test_repository(filesystem.clone()).await;
            let token = RepositoryWriteToken::acquire(dir.path()).await;
            for (options, what) in [
                (
                    CommitOptions {
                        link: Some(String::from("linked")),
                        ..test_commit()
                    },
                    "commit a link alone",
                ),
                (
                    CommitOptions {
                        layer: Some(String::from("layered")),
                        ..test_commit()
                    },
                    "commit a layer",
                ),
            ] {
                let refused = commit_staged(&repository, &token, options).await;
                assert!(
                    matches!(&refused, Err(CommitError::InvalidArguments(error))
                        if error.reason == hold::refused(what).reason),
                    "{refused:?}"
                );
            }
        })
        .await;
    assert!(filesystem.committed_calls.lock().is_empty());
}

/// A commit carrying a staged layer is refused on a provider holding its state, before it makes a
/// revision current: the layer's revision would be current without the provider being handed it.
#[tokio::test]
async fn a_commit_carrying_a_staged_layer_is_refused_on_a_provider_holding_its_state() {
    #[derive(Serialize)]
    struct LayerConfig {
        layers: Vec<Layer>,
    }

    let dir = lore_base::test_util::TempDir::new("lore-commit-test-");
    let filesystem = Arc::new(TestFilesystemProvider::holding_state_reading(dir.path()));
    std::fs::write(dir.path().join("one.txt"), b"one").expect("write file");
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    let root = dir.to_path_buf();
    let committed = filesystem.clone();

    let (signature, refused, current) = runtime()
        .spawn(LORE_CONTEXT.scope(execution, async move {
            let (fixture, committing) = committing_repository(
                &root,
                immutable_store,
                mutable_store,
                committed,
                Arc::default(),
            )
            .await;
            stage_working_tree(&fixture, &root).await;
            let signature = commit_staged(&committing, &fixture.token, test_commit())
                .await
                .expect("Committing");

            std::fs::write(root.join("one.txt"), b"two").expect("rewrite file");
            stage_working_tree(&fixture, &root).await;
            lore_revision::util::config::save(
                &LayerConfig {
                    layers: vec![Layer {
                        target_path: String::from("layered"),
                        repository: RepositoryId::from(uuid::Uuid::now_v7()),
                        staged: Hash::from([1u8; 32]),
                        ..Default::default()
                    }],
                },
                layer::layer_config_path(&committing).expect("Layer config path"),
            )
            .await
            .expect("Writing the layer set");
            let refused = commit_staged(&committing, &fixture.token, test_commit()).await;
            let (current, _) = lore_revision::instance::load_current_anchor_boxed(&committing)
                .await
                .expect("The current anchor");
            (signature, refused, current)
        }))
        .await
        .expect("Test task failed");

    assert!(
        matches!(&refused, Err(CommitError::InvalidArguments(error))
            if error.reason == hold::refused("commit a layer").reason),
        "{refused:?}"
    );
    assert_eq!(
        current, signature,
        "The refused commit moved the current revision"
    );
    assert_eq!(
        *filesystem.committed_calls.lock(),
        [(signature, signature, true)],
        "The refused commit handed a revision over"
    );
}

/// Commits a file written to `dir`, then an edit of it on a dry run, both through a repository
/// over `filesystem`, staging through the OS filesystem. Answers the revision the commit
/// returned and the revision current after the dry run.
async fn commit_then_dry_run(
    dir: &lore_base::test_util::TempDir,
    filesystem: Arc<TestFilesystemProvider>,
) -> (Hash, Hash) {
    std::fs::write(dir.path().join("one.txt"), b"one").expect("write file");
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    let root = dir.to_path_buf();

    runtime()
        .spawn(LORE_CONTEXT.scope(execution, async move {
            let (fixture, committing) = committing_repository(
                &root,
                immutable_store,
                mutable_store,
                filesystem,
                Arc::default(),
            )
            .await;

            stage_working_tree(&fixture, &root).await;
            let signature = commit_staged(&committing, &fixture.token, test_commit())
                .await
                .expect("Committing");

            std::fs::write(root.join("one.txt"), b"two").expect("rewrite file");
            stage_working_tree(&fixture, &root).await;
            let dry_run = test_execution_with(LoreGlobalArgs {
                dry_run: 1,
                ..Default::default()
            });
            LORE_CONTEXT
                .scope(
                    dry_run,
                    commit_staged(&committing, &fixture.token, test_commit()),
                )
                .await
                .expect("A dry run commits");
            let (current, _) = lore_revision::instance::load_current_anchor_boxed(&committing)
                .await
                .expect("The current anchor");
            (signature, current)
        }))
        .await
        .expect("Test task failed")
}

/// An [`OsFilesystem`] that counts the operations opened on it.
struct CountingFilesystem {
    inner: OsFilesystem,
    begins: Arc<AtomicUsize>,
}

#[async_trait]
impl FilesystemProvider for CountingFilesystem {
    async fn begin_operation(&self) -> Result<Arc<InstanceOperationImpl>, FsError> {
        self.begins.fetch_add(1, Ordering::AcqRel);
        FilesystemProvider::begin_operation(&self.inner).await
    }
}

/// A repository over the working tree at `root`, its filesystem counting the operations
/// opened on it.
struct CountingRepository {
    repository: Arc<RepositoryContext>,
    token: RepositoryWriteToken,
    begins: Arc<AtomicUsize>,
}

/// A repository created at `root`, anchored on a fresh default branch, over a filesystem
/// that counts.
///
/// Call from inside a `LORE_CONTEXT` scope: creating a repository reads the execution
/// context.
async fn counting_repository(
    root: &std::path::Path,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> CountingRepository {
    let begins = Arc::new(AtomicUsize::new(0));
    let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
    let branch_id = BranchId::from(uuid::Uuid::now_v7());
    let token = RepositoryWriteToken::acquire(root).await;
    let created = lore_revision::repository::create_local(
        root,
        &token,
        repository_id,
        branch_id,
        branch::DEFAULT_DEFAULT_NAME.to_string(),
        lore_revision::repository::RepositoryConfig::default(),
        false,
    )
    .await
    .expect("Initializing the repository");

    let repository = Arc::new(
        RepositoryContext::new(
            default_repository_creation_args(immutable_store, mutable_store)
                .with_path(root)
                .with_id(repository_id)
                .with_instance_id(created.instance_id)
                .with_filesystem_provider(Arc::new(CountingFilesystem {
                    inner: OsFilesystem::new(root),
                    begins: begins.clone(),
                })),
        )
        .with_write_token(token.share()),
    );
    lore_revision::instance::store_current_anchor_branch(&repository, branch_id)
        .await
        .expect("Storing the anchor branch");

    CountingRepository {
        repository,
        token,
        begins,
    }
}

/// A repository created at `root` as [`counting_repository`] makes it, which stages, and a context
/// of the same repository over `filesystem`, under `filter`, which commits.
///
/// Call from inside a `LORE_CONTEXT` scope.
async fn committing_repository(
    root: &std::path::Path,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    filesystem: Arc<TestFilesystemProvider>,
    filter: Arc<Filter>,
) -> (CountingRepository, Arc<RepositoryContext>) {
    let fixture = counting_repository(root, immutable_store.clone(), mutable_store.clone()).await;
    let committing = Arc::new(
        RepositoryContext::new(
            default_repository_creation_args(immutable_store, mutable_store)
                .with_path(root)
                .with_id(fixture.repository.id)
                .with_instance_id(fixture.repository.instance_id)
                .with_filesystem_provider(filesystem)
                .with_filter(filter),
        )
        .with_write_token(fixture.token.share()),
    );
    (fixture, committing)
}

/// Stages everything the working tree at `root` holds, scanning rather than reading dirty
/// flags so a fixture that wrote its files directly is staged whole.
async fn stage_working_tree(fixture: &CountingRepository, root: &std::path::Path) {
    lore_revision::file::stage::stage(
        fixture.repository.clone(),
        &fixture.token,
        LoreArray::from_vec(vec![LoreString::from(&root.to_path_buf())]),
        lore_revision::stage::StageOptions {
            scan: true,
            ..Default::default()
        },
    )
    .await
    .expect("Staging the working tree");
}

/// A commit with the message `test`.
fn test_commit() -> CommitOptions {
    CommitOptions::new(String::from("test"))
}

/// Commits what `repository` holds staged as `options` asks, with no metadata beyond what a
/// commit stamps.
async fn commit_staged(
    repository: &Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    options: CommitOptions,
) -> Result<Hash, CommitError> {
    Box::pin(commit_with_metadata(
        repository.clone(),
        token,
        options,
        LoreArray::from_vec(Vec::default()),
        LoreArray::from_vec(Vec::default()),
        LoreArray::from_vec(Vec::default()),
    ))
    .await
}
