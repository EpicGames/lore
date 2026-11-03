// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures build working-tree state directly; what these test is how the walk reads it.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::file::dirty::*;
use lore_revision::filter::Filter;
use lore_revision::filter::FilterStates;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_revision::util::path::RelativePath;
use lore_revision::util::path::RelativePathBuf;

use crate::fs::filesystem_provider::TestFilesystemProvider;
use crate::fs::filesystem_provider::test_store_create;
use crate::helper::RepositoryContextCreationArgsExt;
use crate::helper::default_repository_creation_args;

/// Every path a call names is read through the one operation it opens, whatever the call
/// finds: a provider that freezes hands out one snapshot, and a walk opening a second
/// operation would measure one path against a tree the others never saw.
///
/// The test provider holds none of the paths, so each costs the one lookup that settles it
/// and the walk reaches no listing.
#[tokio::test]
async fn one_call_reads_the_working_tree_through_one_operation() {
    let filesystem = Arc::new(TestFilesystemProvider::new());
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    let repository = Arc::new(RepositoryContext::new(
        default_repository_creation_args(immutable_store, mutable_store)
            .with_filesystem_provider(filesystem.clone()),
    ));
    let paths: Vec<RelativePath> = ["one.txt", "two.txt", "three.txt"]
        .into_iter()
        .map(|name| RelativePathBuf::new().push_and_freeze(name))
        .collect();

    LORE_CONTEXT
        .scope(execution, async move {
            let state = State::new();
            dirty_relative_paths_in(repository, state.clone(), state, paths)
                .await
                .expect("marking the named paths");
        })
        .await;

    assert_eq!(
        1,
        filesystem.begins(),
        "The call opened an operation per path rather than one for the whole of it"
    );
    assert_eq!(
        3,
        filesystem.file_infos(),
        "The paths were not all read through the operation"
    );
    assert_eq!(vec![false], *filesystem.finalize_events.lock());
}

/// A named path below a directory the ignore file leaves out whole is left out whatever it
/// holds, so the call reads nothing of the working tree for it.
#[tokio::test]
async fn a_path_below_an_ignored_directory_is_left_out_unread() {
    let filesystem = Arc::new(TestFilesystemProvider::new());
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    let repository = Arc::new(RepositoryContext::new(
        default_repository_creation_args(immutable_store, mutable_store)
            .with_filesystem_provider(filesystem.clone())
            .with_filter(Arc::new(crate::helper::test_filter(&["ignored/"], &[]))),
    ));
    let paths: Vec<RelativePath> = ["ignored/a.txt", "ignored/b.txt"]
        .into_iter()
        .map(crate::helper::relative_path)
        .collect();

    LORE_CONTEXT
        .scope(execution, async move {
            let state = State::new();
            dirty_relative_paths_in(repository, state.clone(), state, paths)
                .await
                .expect("marking the named paths");
        })
        .await;

    assert_eq!(
        0,
        filesystem.file_infos(),
        "The call read the working tree for paths the filter leaves out whatever they hold"
    );
}

/// A layer's trees hold below its source what the filter names below its mount, so a path the
/// layer tracks is looked up at the source spelling. Below the parent's ignored `lay/sub/`, which
/// leaves out whatever a path below it holds, the tracked `debug.log` is still read from the
/// working tree, while an untracked `new.log` beside it is left out unread.
///
/// Looking the mount spelling up in the layer's trees would find nothing, and leave out the
/// tracked file with the untracked one.
#[tokio::test]
async fn a_tracked_layer_path_the_ignore_file_names_is_looked_up_below_the_source() {
    let filesystem = Arc::new(TestFilesystemProvider::new());
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    let mut filter = Filter::default();
    filter
        .ignore
        .add_exclusion("/lay/sub/")
        .expect("ignore rule");
    let repository = Arc::new(RepositoryContext::new(
        default_repository_creation_args(immutable_store, mutable_store)
            .with_filesystem_provider(filesystem.clone())
            .with_filter(Arc::new(filter)),
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            // The layer's tree holds `src/sub/debug.log`; the layer is mounted at `lay/`.
            let state = State::new();
            let mut parent = ROOT_NODE;
            for name in ["src", "sub"] {
                let directory = Node {
                    name_hash: lore_storage::hash::hash_string(name),
                    ..Default::default()
                };
                parent = state
                    .node_add(repository.clone(), parent, directory, name)
                    .await
                    .expect("adding a source directory");
            }
            let tracked = Node {
                flags: NodeFlags::File.bits(),
                name_hash: lore_storage::hash::hash_string("debug.log"),
                ..Default::default()
            };
            state
                .node_add(repository.clone(), parent, tracked, "debug.log")
                .await
                .expect("tracking the file");

            let walk = DirtyWalk::new(
                repository
                    .file_system()
                    .begin_operation()
                    .await
                    .expect("beginning an operation"),
                repository.clone(),
                state.clone(),
                state,
                Arc::new(DirtyStats::default()),
                None,
            )
            .await
            .expect("reading the link registry");

            let root = DirtyNodes {
                current: ROOT_NODE,
                staged: ROOT_NODE,
            };
            let reads = async |mount: &str, source: &str| {
                let before = filesystem.file_infos();
                dirty_named_path(
                    &walk,
                    root,
                    &crate::helper::relative_path(source),
                    &crate::helper::relative_path(mount),
                )
                .await
                .expect("marking the path");
                filesystem.file_infos() - before
            };

            assert_ne!(
                0,
                reads("lay/sub/debug.log", "src/sub/debug.log").await,
                "The tracked layer file was left out by the parent's ignore rule"
            );
            assert_eq!(
                0,
                reads("lay/sub/new.log", "src/sub/new.log").await,
                "An untracked file beside it was read past the ignore rule"
            );
            assert_eq!(
                0,
                reads("lay/sub/debug.log", "lay/sub/debug.log").await,
                "The mount spelling was looked up in the layer's trees"
            );
        })
        .await;
}

/// The repository tracks no links, so a listing yields none and the walk marks none: a link
/// beside a file leaves the file marked and nothing standing for the link.
///
/// The listing is what the walk reads a directory through, so one reaching the filesystem
/// directly instead would mark the link as the file it resolves to.
#[cfg(unix)]
#[tokio::test]
async fn a_link_beside_a_file_is_left_unmarked() {
    let dir = lore_base::test_util::TempDir::new("lore-dirty-test-");
    std::fs::write(dir.path().join("file.txt"), b"content").expect("write file");
    std::os::unix::fs::symlink(dir.path().join("file.txt"), dir.path().join("link.txt"))
        .expect("create link");
    let (repository, execution) = working_tree_repository(dir.path()).await;

    LORE_CONTEXT
        .scope(execution, async move {
            let state = State::new();
            let stats = walk_root(&repository, &state).await;

            assert_eq!(
                1,
                stats.add_count.load(Ordering::Relaxed),
                "The walk marked something beyond the one file the repository tracks"
            );
            assert!(
                holds(&state, &repository, "file.txt").await,
                "The file the repository tracks was not marked"
            );
            assert!(
                !holds(&state, &repository, "link.txt").await,
                "The link was marked, which the repository tracks nothing for"
            );
        })
        .await;
}

/// A link standing where a tracked file was is a path the repository holds nothing at, so the
/// node is marked deleted: the listing names what the working tree holds and it names no link.
///
/// Presence read from the filesystem per child instead would stat through the link, report the
/// target it resolves to, and leave the node neither deleted nor modified.
#[cfg(unix)]
#[tokio::test]
async fn a_tracked_file_a_link_now_stands_at_is_deleted() {
    let dir = lore_base::test_util::TempDir::new("lore-dirty-test-");
    std::fs::write(dir.path().join("target.txt"), b"content").expect("write file");
    std::os::unix::fs::symlink(dir.path().join("target.txt"), dir.path().join("file.txt"))
        .expect("create link");
    let (repository, execution) = working_tree_repository(dir.path()).await;

    LORE_CONTEXT
        .scope(execution, async move {
            let state = State::new();
            let tracked = Node {
                flags: NodeFlags::File.bits(),
                name_hash: lore_storage::hash::hash_string("file.txt"),
                ..Default::default()
            };
            state
                .node_add(repository.clone(), ROOT_NODE, tracked, "file.txt")
                .await
                .expect("tracking the file");

            let stats = walk_root(&repository, &state).await;

            assert_eq!(
                1,
                stats.delete_count.load(Ordering::Relaxed),
                "The tracked file a link now stands at was not marked deleted"
            );
        })
        .await;
}

/// A repository rooted at `path`, with the stores and execution context a walk needs.
#[cfg(unix)]
async fn working_tree_repository(
    path: &std::path::Path,
) -> (
    Arc<RepositoryContext>,
    Arc<lore_revision::interface::ExecutionContext>,
) {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    (
        Arc::new(RepositoryContext::new(
            default_repository_creation_args(immutable_store, mutable_store).with_path(path),
        )),
        execution,
    )
}

/// Marks the repository's root against `state` on both sides, answering with what the walk
/// counted. One state for both is a working copy with nothing staged, which shares the
/// current tree's storage.
#[cfg(unix)]
async fn walk_root(repository: &Arc<RepositoryContext>, state: &Arc<State>) -> Arc<DirtyStats> {
    let walk = DirtyWalk::new(
        repository
            .file_system()
            .begin_operation()
            .await
            .expect("beginning an operation"),
        repository.clone(),
        state.clone(),
        state.clone(),
        Arc::new(DirtyStats::default()),
        None,
    )
    .await
    .expect("reading the link registry");
    let root = DirtyNodes {
        current: ROOT_NODE,
        staged: ROOT_NODE,
    };

    dirty_directory(&walk, root, &RelativePath::new(), FilterStates::ROOT)
        .await
        .expect("marking the working tree");

    walk.stats
}

/// Whether `state` holds a node at `path`.
#[cfg(unix)]
async fn holds(state: &Arc<State>, repository: &Arc<RepositoryContext>, path: &str) -> bool {
    state
        .find_node_link(repository.clone(), path)
        .await
        .is_ok_and(|link| link.is_valid())
}
