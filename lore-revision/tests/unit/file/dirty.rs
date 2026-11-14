// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures build working-tree state directly; what these test is how the walk reads it.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::Ordering;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
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
use lore_revision::state::step_counting;
use lore_revision::util::path::RelativePath;
use lore_revision::util::path::RelativePathBuf;
use lore_storage::hash::hash_string;

use crate::dirty_move_fixture::children_named;
use crate::dirty_move_fixture::committed;
use crate::dirty_move_fixture::dirtied;
use crate::dirty_move_fixture::dirty_moved;
use crate::dirty_move_fixture::holds as fixture_holds;
use crate::dirty_move_fixture::made_directory;
use crate::dirty_move_fixture::node_at;
use crate::dirty_move_fixture::node_id_at;
use crate::dirty_move_fixture::removed_directory;
use crate::dirty_move_fixture::stage_path;
use crate::dirty_move_fixture::staged_moved;
use crate::dirty_move_fixture::tree_paths;
use crate::dirty_move_fixture::with_instance;
use crate::dirty_move_fixture::written;
use crate::fs::filesystem_provider::TestFilesystemProvider;
use crate::fs::filesystem_provider::test_store_create;
use crate::helper::RepositoryContextCreationArgsExt;
use crate::helper::TestRepository;
use crate::helper::default_repository_creation_args;
use crate::helper::test_commit;

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

/// Marking dirty the removal of a file in a directory `lore dirty move` moved marks it deleted at
/// its moved path, which the current revision holds at its committed one.
#[tokio::test]
async fn marking_a_removed_file_in_a_dirty_moved_directory_dirty_marks_it_deleted() {
    with_instance(|instance| async move {
        committed(&instance, &["d/f.txt", "d/g.txt"]).await;
        dirty_moved(&instance, "d", "e").await;
        std::fs::remove_file(instance.path.join("e/f.txt")).expect("the delete on disk");
        let staged = dirtied(&instance, "e/f.txt").await;
        let removed = node_at(&instance, &staged, "e/f.txt").await;
        assert!(
            removed.is_dirty_delete() && !removed.is_staged(),
            "e/f.txt: flags {:#x}",
            removed.flags
        );
    })
    .await;
}

/// Marking dirty a file written again at the path of a file staged for deletion keeps the staged
/// deletion and marks the new file added beside it, so the commit deletes the committed file and
/// the staged tree holds the new one marked added past it.
#[tokio::test]
async fn marking_a_file_staged_for_deletion_dirty_keeps_its_staged_deletion() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["f.txt", "other.txt"]).await;
        let f = first
            .find_node_link(instance.repository.clone(), "f.txt")
            .await
            .expect("the committed file")
            .node;
        std::fs::remove_file(instance.path.join("f.txt")).expect("the delete on disk");
        stage_path(&instance, "f.txt").await;
        std::fs::write(instance.path.join("f.txt"), b"written again").expect("the write");
        let staged = dirtied(&instance, "f.txt").await;
        let deleted = staged
            .node(instance.repository.clone(), f)
            .await
            .expect("the deleted file");
        assert!(
            deleted.is_staged_delete(),
            "f.txt: flags {:#x}",
            deleted.flags
        );

        let revision = test_commit(&instance, "Deleted").await;
        let held = tree_paths(&instance, &revision).await;
        assert!(
            !held.contains_key("f.txt"),
            "the revision holds {:?}",
            held.keys()
        );
        let staged = dirtied(&instance, "other.txt").await;
        let added = node_at(&instance, &staged, "f.txt").await;
        assert!(
            added.is_dirty_add() && !added.is_staged(),
            "f.txt: flags {:#x}",
            added.flags
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

    dirty_directory(
        &walk,
        root,
        &RelativePath::new(),
        (FilterStates::ROOT, false),
    )
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

/// The chain steps the calling thread's finds and children walks have taken.
fn steps_on_this_thread() -> u64 {
    step_counting::set_chain_step_counting(true);
    step_counting::thread_chain_steps() + step_counting::thread_walk_steps()
}

/// Marks `path` below `instance` dirty, answering with the staged revision and the chain steps
/// the call took on this thread.
async fn dirtied_counting(instance: &TestRepository, path: &str) -> (Hash, u64) {
    let before = steps_on_this_thread();
    let signature = dirty(
        instance.repository.clone(),
        LoreArray::from_vec(vec![LoreString::from(&instance.path.join(path))]),
    )
    .await
    .expect("the dirty marks");
    (signature, steps_on_this_thread() - before)
}

/// `count` file paths in `dir`.
fn files_in(dir: &str, count: usize) -> Vec<String> {
    (0..count).map(|index| format!("{dir}/f{index}")).collect()
}

/// How many children of the directory at `path` in the staged revision `signature` `marked` holds
/// for.
async fn children_marked(
    instance: &TestRepository,
    signature: Hash,
    path: &str,
    marked: impl Fn(&Node) -> bool,
) -> usize {
    let staged = State::deserialize(instance.repository.clone(), signature)
        .await
        .expect("the staged state");
    let dir = node_id_at(instance, &staged, path).await;
    let children = staged
        .node_children(instance.repository.clone(), dir)
        .await
        .expect("the directory's children");
    let mut count = 0;
    for child in children {
        let node = staged
            .node(instance.repository.clone(), child)
            .await
            .expect("the child's record");
        count += usize::from(marked(&node));
    }
    count
}

/// Marking a wide directory dirty resolves each entry of the working tree's listing from one
/// listing of the directory in each tree, and each child the working tree lacks from those too:
/// its new files, with nothing staged, where the trees share one State; then its unchanged files
/// and its deleted files, with a staged tree of its own. Each costs the walk a few steps per child
/// at most, where a search of the chain for each name costs N(N-1)/2 steps for N, and marks each
/// child. The deleted files cost a step each in each tree's listing and none more: the children
/// the working tree lacks come from the current tree's listing.
#[tokio::test]
async fn dirtying_a_wide_directory_steps_through_each_child_a_few_times_at_most() {
    with_instance(|instance| async move {
        const N: usize = 256;
        let kept = files_in("kept", N);
        let committing: Vec<&str> = ["new/seed"]
            .into_iter()
            .chain(kept.iter().map(String::as_str))
            .collect();
        committed(&instance, &committing).await;
        let added = files_in("new", N);
        written(
            &instance,
            &added.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        let (signature, steps) = dirtied_counting(&instance, "new").await;
        assert!(steps < 4 * N as u64, "the new files took {steps} steps");
        let adds = children_marked(&instance, signature, "new", Node::is_dirty_add).await;
        assert_eq!(adds, N, "the new files are marked added");
        let (signature, steps) = dirtied_counting(&instance, "kept").await;
        assert!(
            steps < 4 * N as u64,
            "the unchanged files took {steps} steps"
        );
        let modifies = children_marked(&instance, signature, "kept", Node::is_dirty_modify).await;
        assert_eq!(modifies, N, "the unchanged files are marked");
        for file in &kept {
            std::fs::remove_file(instance.path.join(file)).expect("the delete on disk");
        }
        let (signature, steps) = dirtied_counting(&instance, "kept").await;
        assert!(steps < 3 * N as u64, "the deleted files took {steps} steps");
        let deletes = children_marked(&instance, signature, "kept", Node::is_dirty_delete).await;
        assert_eq!(deletes, N, "the deleted files are marked deleted");
    })
    .await;
}

/// Marking dirty the removal of many files marked dirty as added, in a directory holding a staged
/// file listed ahead of them, clears the marks above each removal in a few steps through the
/// directory's children: the walk stops at the staged file, for which the directory keeps its
/// staged bit, rather than reading every child to hash them each time.
#[tokio::test]
async fn reverting_adds_beside_a_staged_file_steps_through_the_directory_a_few_times_each() {
    with_instance(|instance| async move {
        const N: usize = 256;
        committed(&instance, &["d/seed", "other.txt"]).await;
        let added = files_in("d", N);
        written(
            &instance,
            &added.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        dirtied(&instance, "d").await;
        written(&instance, &["d/staged.txt"]);
        stage_path(&instance, "d/staged.txt").await;
        for path in &added {
            std::fs::remove_file(instance.path.join(path)).expect("the removal on disk");
        }
        step_counting::set_chain_step_counting(true);
        let before = step_counting::thread_walk_steps();
        let signature = dirty(
            instance.repository.clone(),
            LoreArray::from_vec(
                added
                    .iter()
                    .map(|path| LoreString::from(&instance.path.join(path)))
                    .collect(),
            ),
        )
        .await
        .expect("the dirty marks");
        let steps = step_counting::thread_walk_steps() - before;
        assert!(
            steps < 8 * N as u64,
            "the removals took {steps} steps through children"
        );
        let adds = children_marked(&instance, signature, "d", |node| {
            node.is_dirty_add() && !node.is_staged()
        })
        .await;
        assert_eq!(adds, 0, "the removed files leave the staged tree");
    })
    .await;
}

/// Marking dirty the removal of many files marked dirty as added, in a directory a staged move took
/// a committed file out of, clears the marks above each removal in a few steps through the
/// directory's children: a file that leaves unstaged carried no staged bit up, so the directory's
/// bare bit, kept for its rehash, is not weighed again for each.
#[tokio::test]
async fn reverting_adds_in_a_directory_a_staged_move_left_steps_through_it_a_few_times_each() {
    with_instance(|instance| async move {
        const N: usize = 256;
        committed(&instance, &["d/seed", "other.txt"]).await;
        let added = files_in("d", N);
        written(
            &instance,
            &added.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        dirtied(&instance, "d").await;
        staged_moved(&instance, "d/seed", "seed").await;
        for path in &added {
            std::fs::remove_file(instance.path.join(path)).expect("the removal on disk");
        }
        step_counting::set_chain_step_counting(true);
        let before = step_counting::thread_walk_steps();
        let signature = dirty(
            instance.repository.clone(),
            LoreArray::from_vec(
                added
                    .iter()
                    .map(|path| LoreString::from(&instance.path.join(path)))
                    .collect(),
            ),
        )
        .await
        .expect("the dirty marks");
        let steps = step_counting::thread_walk_steps() - before;
        assert!(
            steps < 8 * N as u64,
            "the removals took {steps} steps through children"
        );
        let adds = children_marked(&instance, signature, "d", |node| {
            node.is_dirty_add() && !node.is_staged()
        })
        .await;
        assert_eq!(adds, 0, "the removed files leave the staged tree");
    })
    .await;
}

/// Marking dirty a directory of new subdirectories, half of them empty and half holding a new file,
/// adds a node for each, counted as the walk's own add, so the walk steps through each child a few
/// times at most and marks each subdirectory added.
#[tokio::test]
async fn dirtying_a_directory_of_new_directories_steps_through_each_a_few_times_at_most() {
    with_instance(|instance| async move {
        const N: usize = 256;
        committed(&instance, &["wide/seed"]).await;
        for index in 0..N {
            made_directory(&instance, &format!("wide/d{index}"));
            if index % 2 == 0 {
                written(&instance, &[&format!("wide/d{index}/file")]);
            }
        }
        let (signature, steps) = dirtied_counting(&instance, "wide").await;
        assert!(
            steps < 4 * N as u64,
            "the new directories took {steps} steps"
        );
        let adds = children_marked(&instance, signature, "wide", |node| {
            node.is_directory() && node.is_dirty_add()
        })
        .await;
        assert_eq!(adds, N, "the new directories are marked added");
    })
    .await;
}

/// Two new names in one directory differing only in case, as a case-sensitive file system holds
/// them side by side, take one node: the second finds the node added for the first, which the
/// directory's listing, taken before that add, lacks. A case-insensitive file system holds one of
/// the names, which takes one node all the same.
#[tokio::test]
async fn two_new_case_variants_in_one_directory_take_one_node() {
    with_instance(|instance| async move {
        committed(&instance, &["d/seed"]).await;
        written(&instance, &["d/Rock.mesh", "d/rock.mesh"]);
        let (signature, _) = dirtied_counting(&instance, "d").await;
        let staged = State::deserialize(instance.repository.clone(), signature)
            .await
            .expect("the staged state");
        let d = staged
            .find_node_link(instance.repository.clone(), "d")
            .await
            .expect("d")
            .node;
        let children = staged
            .node_children_with_name_hash(instance.repository.clone(), d)
            .await
            .expect("d's children");
        let variants = children
            .iter()
            .filter(|&&(_, name_hash)| name_hash == hash_string("rock.mesh"))
            .count();
        assert_eq!(variants, 1, "d's children: {children:?}");
    })
    .await;
}

/// Two new directories differing only in case, as a case-sensitive file system holds them side by
/// side, where the staged tree holds their name twice, as a committed file staged for delete and
/// as the directory staged in its place: each takes the directory, as a search of the chain does,
/// whichever the file system lists first, so the file new in each is added below the directory.
/// The directory holds more entries than the walk searches for, so it is listed. A
/// case-insensitive file system holds one of the names, so the test has nothing to check there.
#[tokio::test]
async fn case_variants_of_a_staged_type_change_each_take_the_directory() {
    with_instance(|instance| async move {
        if crate::helper::case_insensitive(&instance.path) {
            return;
        }
        let seeds = files_in("d", SEARCHED_CHILDREN);
        let committing: Vec<&str> = ["d/x"]
            .into_iter()
            .chain(seeds.iter().map(String::as_str))
            .collect();
        let first = committed(&instance, &committing).await;
        let d = node_id_at(&instance, &first, "d").await;
        std::fs::remove_file(instance.path.join("d/x")).expect("the delete on disk");
        made_directory(&instance, "d/x");
        let typed = stage_path(&instance, "d").await;
        assert_eq!(
            children_named(&instance, &typed, d, "x").await,
            2,
            "the type change holds the name twice"
        );
        written(&instance, &["d/x/lower", "d/X/upper"]);
        let staged = dirtied(&instance, "d").await;
        for path in ["d/x/lower", "d/x/upper"] {
            assert!(
                fixture_holds(&instance, &staged, path).await,
                "{path} is added below the directory"
            );
        }
    })
    .await;
}

/// A directory the staged tree holds as a directory and the current tree as a link, to a revision
/// the store lacks, is walked without the linked State: a node that is not a directory lists no
/// children, as a search of it finds none, so the walk marks the new files below the directory.
/// One new file the walk searches for, and more it lists the directory for.
#[tokio::test]
async fn a_directory_the_current_tree_holds_as_a_link_is_walked_without_the_linked_state() {
    for count in [1, SEARCHED_CHILDREN + 1] {
        a_directory_held_as_a_link_is_walked(count).await;
    }
}

/// The walk of [`a_directory_the_current_tree_holds_as_a_link_is_walked_without_the_linked_state`]
/// with `count` new files.
async fn a_directory_held_as_a_link_is_walked(count: usize) {
    with_instance(|instance| async move {
        let added = files_in("lib", count);
        written(
            &instance,
            &added.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        let repository = instance.repository.clone();
        let current = State::new();
        let link = Node {
            flags: NodeFlags::Link.bits(),
            name_hash: hash_string("lib"),
            child: ROOT_NODE,
            address: lore_base::types::Address {
                hash: Hash::from_u64(0x11b),
                context: uuid::Uuid::now_v7().into(),
            },
            ..Default::default()
        };
        current
            .node_add(repository.clone(), ROOT_NODE, link, "lib")
            .await
            .expect("the link");
        let staged = State::new();
        staged
            .node_add(
                repository.clone(),
                ROOT_NODE,
                Node {
                    name_hash: hash_string("lib"),
                    ..Default::default()
                },
                "lib",
            )
            .await
            .expect("the directory");
        let walk = DirtyWalk::new(
            repository
                .file_system()
                .begin_operation()
                .await
                .expect("beginning an operation"),
            repository.clone(),
            current,
            staged.clone(),
            Arc::new(DirtyStats::default()),
            None,
        )
        .await
        .expect("reading the link registry");
        let root = DirtyNodes {
            current: ROOT_NODE,
            staged: ROOT_NODE,
        };

        dirty_directory(
            &walk,
            root,
            &RelativePath::new(),
            (FilterStates::ROOT, false),
        )
        .await
        .expect("the walk reads no linked State");

        for path in &added {
            assert!(
                fixture_holds(&instance, &staged, path).await,
                "{count}: {path}"
            );
        }
    })
    .await;
}

/// A walk holds its current State as the base of its staged tree, so a node's return to the place
/// the current revision holds it at reads that revision from the walk rather than again.
#[tokio::test]
async fn a_walk_holds_its_current_state_as_the_base_of_its_staged_tree() {
    with_instance(|instance| async move {
        let current = committed(&instance, &["a.txt"]).await;
        let staged = State::deserialize(instance.repository.clone(), current.revision())
            .await
            .expect("a staged tree of the revision");
        DirtyWalk::new(
            instance
                .repository
                .file_system()
                .begin_operation()
                .await
                .expect("beginning an operation"),
            instance.repository.clone(),
            current.clone(),
            staged.clone(),
            Arc::new(DirtyStats::default()),
            None,
        )
        .await
        .expect("a walk");

        let base = staged
            .base(&instance.repository, current.revision())
            .await
            .expect("the base");
        assert!(Arc::ptr_eq(&base, &current), "the base was read again");
    })
    .await;
}

/// A node that went back to its committed place is spelled as the walk spells its paths: as the
/// tree holds it for the instance's own walk, and below the mount a layer's or link's walk spells
/// them from where the tree holds it below the mount's source, a mount whose last names are its
/// source's among them, and nowhere elsewhere.
#[test]
fn a_returned_node_is_spelled_below_the_walks_mount() {
    let path = |spelled: &str| RelativePath::new_from_initial_path(spelled).expect("a path");
    for (mount, tree, expected) in [
        (None, "t/x", Some("t/x")),
        (None, "t", Some("t")),
        (Some(("m", "src")), "src/t/x", Some("m/t/x")),
        (Some(("m", "src")), "src", Some("m")),
        (Some(("m", "SRC")), "src/t", Some("m/t")),
        (Some(("m", "")), "t/x", Some("m/t/x")),
        (Some(("m", "src")), "elsewhere/t", None),
        (Some(("m", "src")), "srcs/t", None),
        (
            Some(("vendor/src", "src")),
            "src/t/x",
            Some("vendor/src/t/x"),
        ),
        (Some(("vendor/src", "src")), "elsewhere/t", None),
    ] {
        let mounted = mount.map(|(at, source)| Mount {
            at: path(at),
            source: path(source),
        });
        assert_eq!(
            spelled_as_walked(mounted.as_ref(), tree)
                .as_ref()
                .map(RelativePath::as_str),
            expected,
            "mounted {mount:?}, back at {tree}"
        );
    }
}

/// A walk marking a directory `lore dirty move` moved deleted, once the working tree lacks it,
/// returns it to its committed place and asks the view of its children there: below a directory
/// whose contents the view leaves out, they stay as they are, as the working tree lacks them
/// without their having been deleted.
#[tokio::test]
async fn a_returned_directory_marked_deleted_leaves_the_children_its_view_excludes() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["t/a", "t/b", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        let viewed = Arc::new(
            instance
                .repository
                .to_filter_context(Arc::new(crate::helper::test_filter(&[], &["/t/*"]))),
        );
        for file in ["t/a", "t/b"] {
            std::fs::remove_file(instance.path.join(file)).expect("the view's absence on disk");
        }
        std::fs::rename(instance.path.join("t"), instance.path.join("u")).expect("the move");
        lore_revision::file::dirty::dirty_move(
            viewed.clone(),
            instance.path.join("t").to_string_lossy().into_owned(),
            instance.path.join("u").to_string_lossy().into_owned(),
        )
        .await
        .expect("the dirty move");
        std::fs::remove_dir(instance.path.join("u")).expect("the delete on disk");

        let signature = lore_revision::file::dirty::dirty(
            viewed,
            LoreArray::from_vec(vec![LoreString::from(&instance.path.join("u"))]),
        )
        .await
        .expect("the walk");
        let state = State::deserialize(instance.repository.clone(), signature)
            .await
            .expect("the staged state");

        let t = node_at(&instance, &state, "t").await;
        assert!(t.is_dirty_delete(), "t, flags {:#x}", t.flags);
        for file in ["t/a", "t/b"] {
            let node = state
                .node(instance.repository.clone(), held[file])
                .await
                .expect("the file");
            assert!(
                !node.is_dirty_delete(),
                "{file} is marked deleted, flags {:#x}",
                node.flags
            );
        }
    })
    .await;
}

/// A walk discarding a directory staged as added and marked dirty, once the working tree lacks
/// it, leaves a node a staged move took into it to leave the tree with it, as a commit of the
/// directory gone takes it, rather than return it with its staged move.
#[tokio::test]
async fn a_discarded_added_directory_takes_a_node_staged_as_moved_into_it() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["a.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        written(&instance, &["n/k"]);
        stage_path(&instance, "n/k").await;
        staged_moved(&instance, "a.txt", "n/a.txt").await;
        written(&instance, &["n/d"]);
        let marked = dirtied(&instance, "n/d").await;
        let n = node_at(&instance, &marked, "n").await;
        assert!(n.is_dirty_add() && n.is_staged(), "n, flags {:#x}", n.flags);
        removed_directory(&instance, "n");

        let state = dirtied(&instance, "n").await;

        assert!(!fixture_holds(&instance, &state, "n").await, "n is held");
        assert!(
            !fixture_holds(&instance, &state, "a.txt").await,
            "a.txt went back, flags {:#x}",
            node_at(&instance, &state, "a.txt").await.flags
        );
        assert!(
            state
                .node(instance.repository.clone(), held["a.txt"])
                .await
                .expect("the slot")
                .is_discarded(),
            "a.txt's node stays in the tree"
        );
    })
    .await;
}

/// A walk of a directory where a file marked dirty as added now stands as a directory marks the
/// file's node deleted, which discards it, and adds the directory in its place, whether the walk
/// searches the directory's chain or lists it for more entries than it searches.
#[tokio::test]
async fn a_walk_takes_a_directory_where_a_file_marked_added_stood() {
    for width in [0, SEARCHED_CHILDREN + 1] {
        with_instance(|instance| async move {
            let mut files = vec!["a/k".to_string(), "other.txt".to_string()];
            files.extend((0..width).map(|index| format!("a/p{index}")));
            let files: Vec<&str> = files.iter().map(String::as_str).collect();
            committed(&instance, &files).await;
            written(&instance, &["a/x"]);
            dirtied(&instance, "a/x").await;
            std::fs::remove_file(instance.path.join("a/x")).expect("the delete on disk");
            written(&instance, &["a/x/y"]);

            let state = dirtied(&instance, "a").await;

            let a = node_id_at(&instance, &state, "a").await;
            assert_eq!(
                children_named(&instance, &state, a, "x").await,
                1,
                "{width}: the nodes at a/x"
            );
            let x = node_at(&instance, &state, "a/x").await;
            assert!(
                x.is_directory() && x.is_dirty_add(),
                "{width}: a/x, flags {:#x}",
                x.flags
            );
            assert!(
                fixture_holds(&instance, &state, "a/x/y").await,
                "{width}: a/x/y"
            );
        })
        .await;
    }
}
