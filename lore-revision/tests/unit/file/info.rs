// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// A fixture builds working-tree state directly, outside any revision; what these test is what the
// walk measures of it.
#![allow(clippy::disallowed_methods)]

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::MAX_CONCURRENT_TREE_TASKS;
use lore_revision::file::info::*;
use lore_revision::filter::FilterStates;
use lore_revision::fs::filesystem_provider::FileInfo;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreEvent;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository::DOT_LORE;
use lore_revision::repository::DOT_LOREIGNORE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_revision::util::path::RelativePath;

use crate::fs::filesystem_provider::test_store_create;
use crate::helper::RepositoryContextCreationArgsExt;
use crate::helper::default_repository_creation_args;

/// A repository over the working tree at `root`, admitting every path in it.
async fn os_repository(root: &Path) -> Arc<RepositoryContext> {
    let (immutable_store, mutable_store, _context) =
        test_store_create().await.expect("making test stores");
    Arc::new(RepositoryContext::new(
        default_repository_creation_args(immutable_store, mutable_store).with_path(root),
    ))
}

/// A directory holding two files and a subdirectory holding a third, 175 bytes in all.
fn write_tree(root: &Path) {
    let tree = root.join("tree");
    std::fs::create_dir_all(tree.join("inner")).expect("create directory");
    std::fs::write(tree.join("first.txt"), vec![b'a'; 100]).expect("write file");
    std::fs::write(tree.join("third.txt"), vec![b'c'; 25]).expect("write file");
    std::fs::write(tree.join("inner").join("second.txt"), vec![b'b'; 50]).expect("write file");
}

/// What the walk measures at `path`, under the working tree at `root`.
async fn measure(root: &Path, path: RelativePath) -> u64 {
    let repository = os_repository(root).await;
    let operation = repository
        .file_system()
        .begin_operation()
        .await
        .expect("an operation over the working tree");
    calculate_local_size_recurse(
        operation,
        repository,
        path,
        FileInfo::Directory,
        FilterStates::ROOT,
        None,
    )
    .await
    .expect("a measured tree")
}

/// What the walk measures of the tree [`write_tree`] wrote under `root`.
async fn measure_tree(root: &Path) -> u64 {
    measure(
        root,
        RelativePath::new_from_initial_path("tree").expect("relative path"),
    )
    .await
}

#[tokio::test]
async fn a_directory_measures_every_file_below_it() {
    let dir = lore_base::test_util::TempDir::new("lore-info-local-size-");
    write_tree(dir.path());

    assert_eq!(175, measure_tree(dir.path()).await);
}

/// The repository's own directory adds nothing, whatever the working tree holds at its name.
#[tokio::test]
async fn the_repository_directory_is_not_measured() {
    let dir = lore_base::test_util::TempDir::new("lore-info-local-size-");
    write_tree(dir.path());
    std::fs::write(dir.path().join(DOT_LORE), vec![b'd'; 40]).expect("write file");

    assert_eq!(175, measure(dir.path(), RelativePath::default()).await);
}

/// A subtree is walked inline once the fan-out budget is spent, measuring what a walk of its
/// own would have.
#[tokio::test]
async fn a_subtree_is_measured_inline_once_the_budget_is_spent() {
    let dir = lore_base::test_util::TempDir::new("lore-info-local-size-");
    write_tree(dir.path());

    let _permits = local_size_task_semaphore()
        .clone()
        .acquire_many_owned(MAX_CONCURRENT_TREE_TASKS as u32)
        .await
        .expect("the whole fan-out budget");

    assert_eq!(175, measure_tree(dir.path()).await);
}

/// The local and filtered sizes `file info` reports of `path` in `repository`.
async fn info_sizes(repository: &Arc<RepositoryContext>, path: &str) -> (u64, u64) {
    info_sizes_at(repository, path, None).await
}

/// The local and filtered sizes `file info` reports of `path` in `repository`, read from `revision`
/// where given.
async fn info_sizes_at(
    repository: &Arc<RepositoryContext>,
    path: &str,
    revision: Option<String>,
) -> (u64, u64) {
    let reported = Arc::new(Mutex::new(None));
    let recorded = reported.clone();
    let execution = Arc::new(ExecutionContext::new_client(
        LoreGlobalArgs::default(),
        EventDispatcher::new(Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::FileInfo(data) = event {
                *recorded.lock().expect("recording the sizes") =
                    Some((data.local_size, data.filter_size));
            }
        }))),
    ));
    LORE_CONTEXT
        .scope(execution.clone(), async {
            info(
                repository.clone(),
                vec![crate::helper::relative_path(path)],
                InfoOptions {
                    revision,
                    local: true,
                    filtered: true,
                },
            )
            .await
            .expect("the information of the path");
            execution.dispatcher.drain().await;
        })
        .await;
    reported
        .lock()
        .expect("reading the sizes")
        .take()
        .expect("the information of the path is reported")
}

/// `file info` measures a committed file the ignore file names: the tree tracks it, so the ignore
/// file has no say over it.
#[tokio::test]
async fn a_tracked_file_the_ignore_file_names_is_measured() {
    crate::helper::in_execution(async {
        let (ruled, _, _tempdir) =
            crate::helper::ignoring_repository(&["debug.log"], "*.log\n").await;
        let committed = b"committed".len() as u64;
        assert_eq!(
            (committed, committed),
            info_sizes(&ruled, "debug.log").await
        );
    })
    .await;
}

/// `file info` of a committed directory the ignore file names measures the files the tree tracks
/// below it, at any depth, and leaves out the files beside them the tree does not track.
#[tokio::test]
async fn a_tracked_directory_the_ignore_file_names_measures_what_the_tree_tracks() {
    crate::helper::in_execution(async {
        let (ruled, _, tempdir) =
            crate::helper::ignoring_repository(&["build/keep.o", "build/sub/keep.o"], "build/\n")
                .await;
        crate::helper::test_file_write(&tempdir.join("build/new.o"), b"untracked");
        crate::helper::test_file_write(&tempdir.join("build/sub/new.o"), b"untracked");
        let committed = 2 * b"committed".len() as u64;
        assert_eq!((committed, committed), info_sizes(&ruled, "build").await);
    })
    .await;
}

/// `file info` of a directory measures a committed file below it that the ignore file names, and
/// leaves out the untracked file beside it that the rule names.
#[tokio::test]
async fn a_directory_measures_a_tracked_file_the_ignore_file_names_below_it() {
    crate::helper::in_execution(async {
        let (ruled, _, tempdir) =
            crate::helper::ignoring_repository(&["logs/debug.log"], "*.log\n").await;
        crate::helper::test_file_write(&tempdir.join("logs/new.log"), b"untracked");
        let committed = b"committed".len() as u64;
        assert_eq!((committed, committed), info_sizes(&ruled, "logs").await);
    })
    .await;
}

/// What the local walk measures of `dir` in the working tree at `root`, filtered by the rule
/// `foo`, where the revision holds the file `dir/foo` and the directory `dir/Foo`, adding the
/// directory first where `directory_first` says.
async fn measure_folded_pair(root: &Path, directory_first: bool) -> u64 {
    std::fs::write(root.join(DOT_LOREIGNORE), b"foo\n").expect("write file");
    let repository = Arc::new(os_repository(root).await.to_filter_context(
        lore_revision::repository::load_filter(root).expect("loading the filter"),
    ));
    let state = State::new();
    let dir = state
        .node_add(
            repository.clone(),
            ROOT_NODE,
            folded_node("dir", false),
            "dir",
        )
        .await
        .expect("adding dir");
    let mut pair = [("foo", true), ("Foo", false)];
    if directory_first {
        pair.reverse();
    }
    for (name, is_file) in pair {
        let added = state
            .node_add(repository.clone(), dir, folded_node(name, is_file), name)
            .await
            .expect("adding a child");
        if !is_file {
            state
                .node_add(
                    repository.clone(),
                    added,
                    folded_node("inner.txt", true),
                    "inner.txt",
                )
                .await
                .expect("adding inner.txt");
        }
    }
    let operation = repository
        .file_system()
        .begin_operation()
        .await
        .expect("an operation over the working tree");
    let held = HeldAncestor {
        repository: repository.clone(),
        state,
        node: dir,
        path_len: "dir".len(),
    };
    calculate_local_size_recurse(
        operation,
        repository,
        RelativePath::new_from_initial_path("dir").expect("relative path"),
        FileInfo::Directory,
        FilterStates::ROOT,
        Some(held),
    )
    .await
    .expect("a measured tree")
}

/// A staged node named `name`, which its tree tracks, a file where `is_file` says and a directory
/// otherwise.
fn folded_node(name: &str, is_file: bool) -> Node {
    let kind = if is_file {
        NodeFlags::File
    } else {
        NodeFlags::empty()
    };
    Node {
        name_hash: lore_storage::hash::hash_string(name),
        flags: (kind | NodeFlags::StagedAdd).bits(),
        ..Default::default()
    }
}

/// The local walk measures a file and a directory whose names fold to one, both of which the
/// revision holds and the ignore file names, each as the kind it is, whichever the revision lists
/// first. Only a case-sensitive filesystem holds the two side by side, so on one that folds case
/// the test has nothing to measure.
#[tokio::test]
async fn a_file_and_a_directory_of_one_folded_name_are_each_measured_as_their_kind() {
    for directory_first in [false, true] {
        crate::helper::in_execution(async move {
            let dir = lore_base::test_util::TempDir::new("lore-info-folded-");
            if crate::helper::case_insensitive(dir.path()) {
                return;
            }
            std::fs::create_dir_all(dir.path().join("dir/Foo")).expect("create directory");
            std::fs::write(dir.path().join("dir/foo"), vec![b'a'; 100]).expect("write file");
            std::fs::write(dir.path().join("dir/Foo/inner.txt"), vec![b'b'; 50])
                .expect("write file");
            assert_eq!(
                150,
                measure_folded_pair(dir.path(), directory_first).await,
                "the directory added first: {directory_first}"
            );
        })
        .await;
    }
}

/// Commits what is staged in `repository`.
async fn commit(repository: &Arc<RepositoryContext>, message: &str) {
    lore_revision::commit::commit_boxed(
        repository.clone(),
        repository
            .try_write_token()
            .expect("The repository was opened for writing"),
        lore_revision::commit::CommitOptions::new(message.to_string()),
    )
    .await
    .expect("Commit failed");
}

/// `file info` measures a file the ignore file names that a forced stage added, nothing committed:
/// the staged tree tracks it.
#[tokio::test]
async fn a_staged_file_the_ignore_file_names_is_measured() {
    crate::helper::in_execution(async {
        let (ruled, _, tempdir) =
            crate::helper::ignoring_repository(&["dir/keep.txt"], "*.log\n").await;
        crate::helper::test_file_write(&tempdir.join("dir/new.log"), b"staged");
        crate::helper::stage_paths(&ruled, &tempdir, &["dir/new.log"], false, true).await;
        let measured = (b"committed".len() + b"staged".len()) as u64;
        assert_eq!((measured, measured), info_sizes(&ruled, "dir").await);
    })
    .await;
}

/// `file info` measures a committed file `lore dirty move` moved into a directory the ignore file
/// names: the move keeps the node the staged tree tracks.
#[tokio::test]
async fn a_file_a_dirty_move_took_into_an_ignored_directory_is_measured() {
    crate::helper::in_execution(async {
        let (ruled, _, tempdir) =
            crate::helper::ignoring_repository(&["a.txt", "logs/old.txt"], "logs/\n").await;
        std::fs::rename(tempdir.join("a.txt"), tempdir.join("logs/a.txt")).expect("Move failed");
        let at = |path: &str| tempdir.join(path).to_string_lossy().into_owned();
        lore_revision::file::dirty::dirty_move(ruled.clone(), at("a.txt"), at("logs/a.txt"))
            .await
            .expect("Dirty move failed");
        let measured = 2 * b"committed".len() as u64;
        assert_eq!((measured, measured), info_sizes(&ruled, "logs").await);
    })
    .await;
}

/// `file info` of a file the ignore file names that the tree does not track leaves it out of the
/// local size.
#[tokio::test]
async fn an_untracked_file_the_ignore_file_names_is_left_out() {
    crate::helper::in_execution(async {
        let (ruled, _, tempdir) =
            crate::helper::ignoring_repository(&["keep.txt"], "*.log\n").await;
        crate::helper::test_file_write(&tempdir.join("new.log"), b"untracked");
        assert_eq!((0, 0), info_sizes(&ruled, "new.log").await);
    })
    .await;
}

/// `file info --revision` decides what is tracked against that revision: a file it holds that the
/// current revision deleted, which the ignore file names, is measured there and left out without
/// it.
#[tokio::test]
async fn a_revision_decides_what_file_info_tracks() {
    crate::helper::in_execution(async {
        let (ruled, _, tempdir) =
            crate::helper::ignoring_repository(&["dir/a.log", "dir/keep.txt"], "*.log\n").await;
        let holding = crate::helper::next_tree(&ruled)
            .await
            .revision()
            .to_string();
        std::fs::remove_file(tempdir.join("dir/a.log")).expect("Delete failed");
        crate::helper::stage_paths(&ruled, &tempdir, &["dir/a.log"], false, false).await;
        commit(&ruled, "Delete the log").await;
        crate::helper::test_file_write(&tempdir.join("dir/a.log"), b"committed");
        let committed = b"committed".len() as u64;
        assert_eq!(
            2 * committed,
            info_sizes_at(&ruled, "dir", Some(holding)).await.0
        );
        assert_eq!(committed, info_sizes(&ruled, "dir").await.0);
    })
    .await;
}
