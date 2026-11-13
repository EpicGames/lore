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
use crate::dirty_move_fixture::stage_path;
use crate::dirty_move_fixture::with_instance;
use crate::dirty_move_fixture::written;
use crate::fs::filesystem_provider::TestFilesystemProvider;
use crate::fs::filesystem_provider::test_store_create;
use crate::helper::RepositoryContextCreationArgsExt;
use crate::helper::TestRepository;
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

        dirty_directory(&walk, root, &RelativePath::new(), FilterStates::ROOT)
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
