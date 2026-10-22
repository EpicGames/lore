// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures build working-tree state directly; what these test is how the walk reads it.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::file::dirty::*;
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
