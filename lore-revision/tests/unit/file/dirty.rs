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
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreString;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_revision::util::path::RelativePath;
use lore_revision::util::path::RelativePathBuf;

use crate::dirty_move_fixture::committed;
use crate::dirty_move_fixture::dirtied;
use crate::dirty_move_fixture::dirty_moved;
use crate::dirty_move_fixture::node_at;
use crate::dirty_move_fixture::with_instance;
use crate::dirty_move_fixture::written;
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

/// A path the operation cannot describe is not one that is gone: marking it as deleted would
/// stage the removal of a file that is there, so the walk reports the failure instead.
#[tokio::test]
async fn a_path_the_operation_cannot_describe_fails_the_walk() {
    let filesystem = Arc::new(TestFilesystemProvider::failing_file_info());
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    let repository = Arc::new(RepositoryContext::new(
        default_repository_creation_args(immutable_store, mutable_store)
            .with_filesystem_provider(filesystem),
    ));
    let paths = vec![RelativePathBuf::new().push_and_freeze("tracked.txt")];

    let result = LORE_CONTEXT
        .scope(execution, async move {
            let state = State::new();
            dirty_relative_paths_in(repository, state.clone(), state, paths).await
        })
        .await;

    assert!(
        result.is_err(),
        "the walk treated a path it could not describe as absent"
    );
}

/// Marking dirty the paths below a committed file the working tree holds as a directory, a file and
/// a directory below a missing name and below the file itself, leaves them unmarked and adds
/// nothing below the file, while a path beside them is marked.
#[tokio::test]
async fn paths_below_a_committed_file_are_left_unmarked_beside_the_others() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["f.txt", "other.txt"]).await;
        let file = first
            .find_node_link(instance.repository.clone(), "f.txt")
            .await
            .expect("the file")
            .node;
        std::fs::remove_file(instance.path.join("f.txt")).expect("the delete on disk");
        written(&instance, &["f.txt/sub/new.txt", "f.txt/new.txt"]);
        std::fs::write(instance.path.join("other.txt"), b"edited").expect("the edit on disk");
        let paths = [
            "f.txt/sub/new.txt",
            "f.txt/new.txt",
            "f.txt/sub",
            "other.txt",
        ]
        .into_iter()
        .map(|path| LoreString::from(&instance.path.join(path)))
        .collect();

        let signature = dirty(instance.repository.clone(), LoreArray::from_vec(paths))
            .await
            .expect("the paths beside the file are marked");
        let staged = State::deserialize(instance.repository.clone(), signature)
            .await
            .expect("the staged tree");
        let below = staged
            .node(instance.repository.clone(), file)
            .await
            .expect("the file's record")
            .child;
        assert_eq!(
            below,
            first
                .node(instance.repository.clone(), file)
                .await
                .expect("the committed file")
                .child,
            "a node was added below the file"
        );
        let other = staged
            .find_node_link(instance.repository.clone(), "other.txt")
            .await
            .expect("other.txt")
            .node;
        let marked = staged
            .node(instance.repository.clone(), other)
            .await
            .expect("other.txt's record");
        assert!(marked.is_dirty(), "flags {:#x}", marked.flags);
    })
    .await;
}

/// A directory marked dirty as added keeps its mark once the file added in it is removed and its
/// path marked dirty or staged again: the working tree still holds the directory, and a node the
/// staged tree holds unmarked a commit would store unhashed.
#[tokio::test]
async fn an_added_directory_keeps_its_mark_once_the_added_file_it_held_leaves() {
    for staging in [false, true] {
        with_instance(|instance| async move {
            committed(&instance, &["other.txt"]).await;
            std::fs::create_dir(instance.path.join("new")).expect("mkdir");
            written(&instance, &["new/file.txt", "added.txt"]);
            for path in ["new", "new/file.txt", "added.txt"] {
                dirty(
                    instance.repository.clone(),
                    LoreArray::from_vec(vec![LoreString::from(&instance.path.join(path))]),
                )
                .await
                .expect("the add marked");
            }
            std::fs::remove_file(instance.path.join("new/file.txt")).expect("the delete on disk");
            let gone =
                LoreArray::from_vec(vec![LoreString::from(&instance.path.join("new/file.txt"))]);
            if staging {
                lore_revision::file::stage::stage(
                    instance.repository.clone(),
                    &instance.write_token,
                    gone,
                    lore_revision::stage::StageOptions {
                        scan: true,
                        ..Default::default()
                    },
                )
                .await
                .expect("the stage");
            } else {
                dirty(instance.repository.clone(), gone)
                    .await
                    .expect("the reverted add marked");
            }

            let (_, staged, _) = State::deserialize_current_and_staged(instance.repository.clone())
                .await
                .expect("the states");
            let staged = staged.expect("a staged tree");
            let directory = staged
                .find_node_link(instance.repository.clone(), "new")
                .await
                .expect("the directory")
                .node;
            let directory = staged
                .node(instance.repository.clone(), directory)
                .await
                .expect("its record");
            assert!(
                directory.is_dirty_add() && !directory.is_staged() && directory.child().is_none(),
                "staging {staging}: flags {:#x}",
                directory.flags
            );
            let added = staged
                .find_node_link(instance.repository.clone(), "added.txt")
                .await
                .expect("the file added beside it")
                .node;
            let added = staged
                .node(instance.repository.clone(), added)
                .await
                .expect("its record");
            assert!(
                added.is_dirty_add(),
                "staging {staging}: flags {:#x}",
                added.flags
            );
        })
        .await;
    }
}

/// Marking dirty an edited file in a directory `lore dirty move` renamed marks it modified as well
/// as moved.
#[tokio::test]
async fn marking_an_edited_file_in_a_dirty_moved_directory_dirty_marks_it_moved_and_modified() {
    with_instance(|instance| async move {
        committed(&instance, &["d/a.txt", "d/b.txt"]).await;
        dirty_moved(&instance, "d", "e").await;
        std::fs::write(instance.path.join("e/b.txt"), b"edited in place").expect("the edit");
        let staged = dirtied(&instance, "e/b.txt").await;
        let edited = node_at(&instance, &staged, "e/b.txt").await;
        assert!(
            edited.is_dirty_move() && edited.is_dirty_modify() && !edited.is_staged(),
            "e/b.txt: flags {:#x}",
            edited.flags
        );
        let untouched = node_at(&instance, &staged, "e/a.txt").await;
        assert!(
            untouched.is_dirty_move() && !untouched.is_dirty_modify(),
            "e/a.txt: flags {:#x}",
            untouched.flags
        );
    })
    .await;
}

/// Marking a directory dirty keeps the moves `lore dirty move` marked out of it, to another
/// directory and to another name in it: the working tree lacks their committed names, and the
/// staged tree holds them elsewhere.
#[tokio::test]
async fn marking_a_directory_dirty_keeps_the_moves_out_of_it() {
    with_instance(|instance| async move {
        committed(&instance, &["d/x", "d/y"]).await;
        dirty_moved(&instance, "d/x", "e").await;
        dirty_moved(&instance, "d/y", "d/z").await;
        let staged = dirtied(&instance, "d").await;
        for path in ["e", "d/z"] {
            let node = node_at(&instance, &staged, path).await;
            assert!(node.is_dirty_move(), "{path}: flags {:#x}", node.flags);
        }
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
