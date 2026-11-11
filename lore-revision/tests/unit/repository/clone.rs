// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixture setup builds and permissions files directly rather than through the driver: what these
// tests exercise is how clone reacts to a filesystem in a given state, not how that state is
// reached.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore_base::error::AddressNotFound;
use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::filter::Filter;
use lore_revision::filter::FilterMode;
use lore_revision::fs::filesystem_provider::FilesystemProvider;
use lore_revision::fs::filesystem_provider::InstanceOperationImpl;
use lore_revision::fs::os::OsFilesystem;
use lore_revision::node::BLOCK_NODE_COUNT;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::clone::*;
use lore_revision::repository::prefetch::StopWhen;
use lore_revision::stage::stage_single_node;
use lore_revision::state::State;
use lore_revision::util::path::RelativePath;
use lore_storage::StoreError;
use lore_storage::hash::hash_string;
use zerocopy::FromZeros;

use crate::branch::null_repository;
use crate::fs::filesystem_provider::setup_test_execution;
use crate::hooked_store::StoreCall;
use crate::repository::test_helpers::caused_by;
use crate::repository::test_helpers::file_node;
use crate::repository::test_helpers::working_tree_repository;
use crate::repository::test_helpers::working_tree_repository_filtered;

async fn create_operation() -> (TempDir, Arc<InstanceOperationImpl>) {
    let temp = TempDir::new("lore-clone-temp-path-");
    let os_filesystem = OsFilesystem::new(temp.path());
    let operation = <OsFilesystem as FilesystemProvider>::begin_operation(&os_filesystem)
        .await
        .expect("Starting test operation");
    (temp, operation)
}

fn relative_path(path: &str) -> RelativePath {
    RelativePath::new_from_initial_path(path).expect("Relative path")
}

/// Where `path` is under the root the test operation was opened on, for the fixtures that
/// build filesystem state directly rather than through it.
fn on_disk(temp: &TempDir, path: &RelativePath) -> std::path::PathBuf {
    temp.path().join(path.as_str())
}

#[tokio::test]
async fn ensure_parent_dir_creates_missing_ancestors() {
    let (temp, operation) = create_operation().await;
    let file = relative_path("nested/deeper/file.txt");
    let stats = CloneStats::default();

    ensure_parent_dir(&file, &operation, &stats)
        .await
        .expect("missing parent should be created");

    assert!(on_disk(&temp, &file.parent_path()).is_dir());
}

/// The clone root already exists and is not ours to create: files at the top of the tree
/// take it as their parent, so this must not fail the clone.
#[tokio::test]
async fn ensure_parent_dir_accepts_existing_parent() {
    let (_temp, operation) = create_operation().await;
    let file = relative_path("file.txt");
    let stats = CloneStats::default();

    ensure_parent_dir(&file, &operation, &stats)
        .await
        .expect("existing parent should not fail");
}

#[tokio::test]
async fn ensure_parent_dir_caches_parent_once_per_path() {
    let (_temp, operation) = create_operation().await;
    let stats = CloneStats::default();
    let first = relative_path("dir/a.txt");
    let second = relative_path("dir/b.txt");

    ensure_parent_dir(&first, &operation, &stats)
        .await
        .expect("first file should create the parent");
    ensure_parent_dir(&second, &operation, &stats)
        .await
        .expect("sibling should hit the cache");

    assert_eq!(stats.created_parents.len(), 1);
}

/// Tolerating an existing parent must not extend to tolerating a real failure: a file
/// sitting where the parent directory belongs still has to abort the clone.
#[tokio::test]
async fn ensure_parent_dir_rejects_parent_that_is_a_file() {
    let (temp, operation) = create_operation().await;
    let blocker = relative_path("blocker");
    tokio::fs::File::create(on_disk(&temp, &blocker))
        .await
        .expect("create blocker file");
    let stats = CloneStats::default();

    let err = ensure_parent_dir(&blocker.join("file.txt"), &operation, &stats)
        .await
        .expect_err("a file where the parent belongs must fail");

    assert!(err.to_string().contains("Failed to create directory"));
    assert!(stats.created_parents.is_empty());
}

/// A parent that genuinely cannot be created still has to abort the clone.
#[cfg(unix)]
#[tokio::test]
async fn ensure_parent_dir_rejects_uncreatable_parent() {
    use std::os::unix::fs::PermissionsExt;

    let (temp, operation) = create_operation().await;
    let locked = relative_path("locked");
    tokio::fs::create_dir(on_disk(&temp, &locked))
        .await
        .expect("create locked dir");
    tokio::fs::set_permissions(
        on_disk(&temp, &locked),
        std::fs::Permissions::from_mode(0o500),
    )
    .await
    .expect("drop write permission");
    let stats = CloneStats::default();

    let result = ensure_parent_dir(&locked.join("child/file.txt"), &operation, &stats).await;

    // Restore write permission first so the temp dir can be cleaned up.
    tokio::fs::set_permissions(
        on_disk(&temp, &locked),
        std::fs::Permissions::from_mode(0o700),
    )
    .await
    .expect("restore write permission");
    let err = result.expect_err("uncreatable parent must fail");
    assert!(err.to_string().contains("Failed to create directory"));
    assert!(stats.created_parents.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn adversarial_dangling_symlink_parent() {
    let (temp, operation) = create_operation().await;
    let link = relative_path("link");
    std::os::unix::fs::symlink(temp.path().join("nowhere"), on_disk(&temp, &link))
        .expect("create dangling symlink");
    let stats = CloneStats::default();

    let result = ensure_parent_dir(&link.join("file.txt"), &operation, &stats).await;

    assert!(result.is_err(), "a dangling symlink is not a usable parent");
}

#[cfg(unix)]
#[tokio::test]
async fn adversarial_symlink_to_file_parent() {
    let (temp, operation) = create_operation().await;
    let target = relative_path("target");
    tokio::fs::File::create(on_disk(&temp, &target))
        .await
        .expect("create target file");
    let link = relative_path("link");
    std::os::unix::fs::symlink(on_disk(&temp, &target), on_disk(&temp, &link))
        .expect("create symlink");
    let stats = CloneStats::default();

    let result = ensure_parent_dir(&link.join("file.txt"), &operation, &stats).await;

    assert!(
        result.is_err(),
        "a symlink to a file is not a usable parent"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn adversarial_symlink_to_directory_parent() {
    let (temp, operation) = create_operation().await;
    let target = relative_path("target");
    tokio::fs::create_dir(on_disk(&temp, &target))
        .await
        .expect("create target dir");
    let link = relative_path("link");
    std::os::unix::fs::symlink(on_disk(&temp, &target), on_disk(&temp, &link))
        .expect("create symlink");
    let stats = CloneStats::default();

    ensure_parent_dir(&link.join("file.txt"), &operation, &stats)
        .await
        .expect("a symlink to a directory is a usable parent");
}

#[tokio::test]
async fn adversarial_parent_nested_under_a_file() {
    let (temp, operation) = create_operation().await;
    let blocker = relative_path("blocker");
    tokio::fs::File::create(on_disk(&temp, &blocker))
        .await
        .expect("create blocker file");
    let stats = CloneStats::default();

    let result = ensure_parent_dir(&blocker.join("deep/file.txt"), &operation, &stats).await;

    assert!(result.is_err(), "a file cannot contain a directory");
}

#[tokio::test]
async fn adversarial_paths_without_a_usable_parent() {
    let (_temp, operation) = create_operation().await;
    let stats = CloneStats::default();

    // Filesystem root: no parent to create.
    ensure_parent_dir(&relative_path("/"), &operation, &stats)
        .await
        .expect("root must be a no-op");
    // Bare relative name: parent is the empty path.
    ensure_parent_dir(&relative_path("file.txt"), &operation, &stats)
        .await
        .expect("a bare relative name must be a no-op");
    // Empty path.
    ensure_parent_dir(&RelativePath::new(), &operation, &stats)
        .await
        .expect("empty path must be a no-op");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adversarial_concurrent_calls_same_parent() {
    let (temp, operation) = create_operation().await;
    let stats = Arc::new(CloneStats::default());
    let parent = relative_path("shared/nested");

    let mut tasks = Vec::new();
    for index in 0..16 {
        let operation = operation.clone();
        let stats = stats.clone();
        let file = parent.join(format!("file-{index}.txt"));
        tasks.push(lore_spawn!(async move {
            ensure_parent_dir(&file, &operation, &stats).await
        }));
    }
    for task in tasks {
        task.await
            .expect("task must not panic")
            .expect("concurrent creation of the same parent must not fail");
    }

    assert!(on_disk(&temp, &parent).is_dir());
    assert_eq!(stats.created_parents.len(), 1);
}

/// A node with neither the file nor the link flag, which makes it a directory, and no children. A
/// `stop` that never answers true.
fn never() -> StopWhen {
    Arc::new(|| false)
}

fn empty_directory_node() -> Node {
    Node::new_zeroed()
}

/// A state holding at its root `count` empty directories and as many files whose content no store
/// holds, named so that directories and files alternate in any order of names. Discovery creates
/// each empty directory as it reaches it, which keeps it walking the tree while writing the files
/// fails.
async fn state_of_unwritable_files(
    repository: &Arc<RepositoryContext>,
    count: usize,
) -> Arc<State> {
    let state = State::new();
    for index in 0..count {
        let entries = [
            (format!("{index:04}-directory"), empty_directory_node()),
            (
                format!("{index:04}-file"),
                file_node(index.to_string().as_bytes()),
            ),
        ];
        for (name, node) in entries {
            stage_single_node(
                repository.clone(),
                state.clone(),
                relative_path(&name),
                node,
                Arc::default(),
                None,
                FilterMode::Full,
            )
            .await
            .expect("stage the node");
        }
    }
    state
}

/// A clone with unavailable content reports the missing address even when discovery spans
/// many blocks and more files than the work channel can hold.
#[tokio::test]
async fn a_clone_reports_missing_content_across_many_blocks() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let temp = TempDir::new("lore-clone-unwritable-files-");
            let repository = working_tree_repository(temp.path()).await;
            let state = state_of_unwritable_files(&repository, 2_000).await;
            let ctx = CloneContext {
                operation: repository
                    .file_system()
                    .begin_operation()
                    .await
                    .expect("Starting test operation"),
                repository,
                state,
                options: Arc::default(),
                stats: Arc::default(),
                modified_times: Arc::default(),
            };

            let error = clone_in_path(ctx)
                .await
                .expect_err("no file can be written");

            assert!(
                caused_by::<AddressNotFound>(&error),
                "the clone reported {error:?} rather than the content it could not fetch"
            );
        })
        .await;
}

/// A clone whose root file the revision does not hold reports the file as not found.
#[tokio::test]
async fn a_clone_reports_a_missing_root_file_as_not_found() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let temp = TempDir::new("lore-clone-missing-root-file-");
            let repository = working_tree_repository(temp.path()).await;
            let ctx = CloneContext {
                operation: repository
                    .file_system()
                    .begin_operation()
                    .await
                    .expect("Starting test operation"),
                repository,
                state: State::new(),
                options: Arc::new(CloneOptions {
                    root_files: vec!["missing.txt".to_string()],
                    ..Default::default()
                }),
                stats: Arc::default(),
                modified_times: Arc::default(),
            };

            let error = clone_in_path(ctx)
                .await
                .expect_err("the root file is missing");

            assert!(error.is_file_not_found(), "{error:?}");
        })
        .await;
}

/// The directories `a` and `a/b`, and the files `a/b/f` and `g`.
fn nested_entries() -> Vec<(String, Node)> {
    vec![
        ("a".to_string(), empty_directory_node()),
        ("a/b".to_string(), empty_directory_node()),
        ("a/b/f".to_string(), file_node(b"f")),
        ("g".to_string(), file_node(b"g")),
    ]
}

/// Files at the root of [`entries_spanning_blocks`].
const SPANNING_ROOT_FILES: usize = BLOCK_NODE_COUNT + 8;

/// The directories `a` and `a/b` and the file `a/b/f`, then [`SPANNING_ROOT_FILES`] files at the
/// root, then the directories `z` and `z/y` and the file `z/y/g`. Staged in this order, the root's
/// entries fill its node block and continue in the next, with a directory tree in each block.
fn entries_spanning_blocks() -> Vec<(String, Node)> {
    let mut entries = vec![
        ("a".to_string(), empty_directory_node()),
        ("a/b".to_string(), empty_directory_node()),
        ("a/b/f".to_string(), file_node(b"f")),
    ];
    entries.extend((0..SPANNING_ROOT_FILES).map(|index| {
        let name = format!("r{index:04}");
        let node = file_node(name.as_bytes());
        (name, node)
    }));
    entries.extend([
        ("z".to_string(), empty_directory_node()),
        ("z/y".to_string(), empty_directory_node()),
        ("z/y/g".to_string(), file_node(b"g")),
    ]);
    entries
}

/// What a warm-up of a state holding `entries` walks down to `levels`, stopping once `stop` answers
/// true, in a repository named after `test`.
async fn warmed(
    test: &str,
    entries: Vec<(String, Node)>,
    levels: u32,
    stop: StopWhen,
) -> WarmedTree {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let temp = TempDir::new(test);
            let repository = working_tree_repository(temp.path()).await;
            let state = State::new();
            for (name, node) in entries {
                stage_single_node(
                    repository.clone(),
                    state.clone(),
                    relative_path(&name),
                    node,
                    Arc::default(),
                    None,
                    FilterMode::Full,
                )
                .await
                .expect("stage the node");
            }
            warm_tree(repository, state, levels, stop)
                .await
                .expect("warm the tree")
        })
        .await
}

/// A warm-up enters directories down to the levels it is given: one level counts the root's
/// directory without entering it, every level counts the whole tree, and no level walks nothing.
/// The nodes share one block.
#[tokio::test]
async fn a_tree_warm_up_walks_the_levels_it_is_given() {
    let test = "a_tree_warm_up_walks_the_levels_it_is_given-";
    let one = warmed(test, nested_entries(), 1, never()).await;
    assert_eq!((one.directories, one.blocks), (1, 1));

    let all = warmed(test, nested_entries(), u32::MAX, never()).await;
    assert_eq!((all.directories, all.blocks), (2, 1));

    let none = warmed(test, nested_entries(), 0, never()).await;
    assert_eq!((none.directories, none.blocks), (0, 0));
}

/// A directory the walk reaches after the root's entries continue into another node block keeps its
/// level there: one level loads both blocks the root's entries span and enters neither directory,
/// and two levels enter both.
#[tokio::test]
async fn a_tree_warm_up_keeps_the_level_across_blocks() {
    let test = "a_tree_warm_up_keeps_the_level_across_blocks-";
    let one = warmed(test, entries_spanning_blocks(), 1, never()).await;
    assert_eq!((one.directories, one.blocks), (2, 2));

    let two = warmed(test, entries_spanning_blocks(), 2, never()).await;
    assert_eq!((two.directories, two.blocks), (4, 2));
}

/// A warm-up that stops midway returns with what it reached. Over the root's entries spanning two
/// blocks, `stop` is asked before the root is read, at the dispatch of the root's entries, by the
/// task loading their first block, and at the dispatch crossing into the second block: answering
/// true on that fourth call, the walk has counted one directory and loaded one block, and returns.
#[tokio::test]
async fn a_tree_warm_up_stopped_midway_returns_what_it_reached() {
    let test = "a_tree_warm_up_stopped_midway_returns_what_it_reached-";
    let asked = Arc::new(AtomicUsize::new(0));
    let stop: StopWhen = Arc::new(move || asked.fetch_add(1, Ordering::Relaxed) >= 3);

    let stopped = warmed(test, entries_spanning_blocks(), 1, stop).await;
    assert_eq!((stopped.directories, stopped.blocks), (1, 1));
}

/// A warm-up told to stop walks and loads nothing.
#[tokio::test]
async fn a_tree_warm_up_told_to_stop_walks_nothing() {
    let test = "a_tree_warm_up_told_to_stop_walks_nothing-";
    let stopped = warmed(test, nested_entries(), u32::MAX, Arc::new(|| true)).await;

    assert_eq!((stopped.directories, stopped.blocks), (0, 0));
}

/// A view whose re-inclusion opens with a wildcard sends discovery into every directory the view
/// excludes. A clone creates such a directory only where content it keeps lands below it, and still
/// creates an empty directory the view keeps.
#[tokio::test]
async fn a_clone_creates_no_directory_the_view_excludes() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let temp = TempDir::new("lore-clone-excluded-directories-");
            let mut filter = Filter::default();
            filter.view.add_exclusion("/*").expect("view exclusion");
            filter
                .view
                .add_inclusion("*/saved/config/")
                .expect("view inclusion");
            let repository = working_tree_repository_filtered(temp.path(), filter).await;
            let empty_file = Node {
                address: Default::default(),
                ..file_node(b"")
            };
            let state = State::new();
            for (name, node) in [
                ("game", empty_directory_node()),
                ("game/saved", empty_directory_node()),
                ("game/saved/config", empty_directory_node()),
                ("game/saved/config/a.ini", empty_file),
                ("game/saved/config/kept", empty_directory_node()),
                ("game/saved/logs", empty_directory_node()),
                ("game/saved/logs/l.log", empty_file),
                ("engine", empty_directory_node()),
                ("engine/empty", empty_directory_node()),
            ] {
                stage_single_node(
                    repository.clone(),
                    state.clone(),
                    relative_path(name),
                    node,
                    Arc::default(),
                    None,
                    FilterMode::empty(),
                )
                .await
                .expect("stage the node");
            }
            let ctx = CloneContext {
                operation: repository
                    .file_system()
                    .begin_operation()
                    .await
                    .expect("Starting test operation"),
                repository,
                state,
                options: Arc::default(),
                stats: Arc::default(),
                modified_times: Arc::default(),
            };

            clone_in_path(ctx).await.expect("clone under the view");

            let exists = |path: &str| on_disk(&temp, &relative_path(path)).exists();
            assert_eq!(
                (
                    exists("game/saved/config/a.ini"),
                    exists("game/saved/config/kept"),
                    exists("game/saved/logs"),
                    exists("engine"),
                ),
                (true, true, false, false),
                "the clone holds what the view keeps and no directory it excludes"
            );
        })
        .await;
}

/// A clone creates a directory the view includes whose children the view excludes, though a
/// re-inclusion opening with a wildcard sends discovery into each of them.
#[tokio::test]
async fn a_clone_creates_a_directory_the_view_includes_whose_children_it_excludes() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let temp = TempDir::new("lore-clone-included-directory-");
            let mut filter = Filter::default();
            filter
                .view
                .add_exclusion("/engine/*")
                .expect("view exclusion");
            filter
                .view
                .add_inclusion("*/saved/config/")
                .expect("view inclusion");
            let repository = working_tree_repository_filtered(temp.path(), filter).await;
            let empty_file = Node {
                address: Default::default(),
                ..file_node(b"")
            };
            let state = State::new();
            for (name, node) in [
                ("engine", empty_directory_node()),
                ("engine/saved", empty_directory_node()),
                ("engine/saved/r.cpp", empty_file),
            ] {
                stage_single_node(
                    repository.clone(),
                    state.clone(),
                    relative_path(name),
                    node,
                    Arc::default(),
                    None,
                    FilterMode::empty(),
                )
                .await
                .expect("stage the node");
            }
            let ctx = CloneContext {
                operation: repository
                    .file_system()
                    .begin_operation()
                    .await
                    .expect("Starting test operation"),
                repository,
                state,
                options: Arc::default(),
                stats: Arc::default(),
                modified_times: Arc::default(),
            };

            clone_in_path(ctx).await.expect("clone under the view");

            let exists = |path: &str| on_disk(&temp, &relative_path(path)).exists();
            assert_eq!(
                (exists("engine"), exists("engine/saved")),
                (true, false),
                "the directory the view includes is created and the one it excludes is not"
            );
        })
        .await;
}

/// The missing file finishes before discovery supplies more work. Files supplied after the
/// consumer observes that error must still be written, and the original error must survive.
#[tokio::test]
async fn clone_continues_after_a_missing_file() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let temp = TempDir::new("lore-clone-continue-");
            let repository = working_tree_repository(temp.path()).await;
            let stats = Arc::new(CloneStats::default());
            let ctx = CloneContext {
                operation: repository.file_system().begin_operation().await.unwrap(),
                repository: repository.clone(),
                state: State::new(),
                options: Arc::default(),
                stats: stats.clone(),
                modified_times: Arc::default(),
            };
            let (tx, rx) = tokio::sync::mpsc::channel(3);
            tx.try_send(CloneWorkItem {
                repository: repository.clone(),
                node: file_node(b"obliterated"),
                repository_path: relative_path("missing.txt"),
            })
            .unwrap();
            let mut consumer = std::pin::pin!(clone_execute(rx, ctx));
            assert!(futures::poll!(&mut consumer).is_pending());
            // Only this test polls the consumer. Reacquiring every permit waits for the
            // missing file's task to finish without letting the consumer accept another file.
            let permits = stats.file_inflight.available_permits() as u32
                + stats.file_inflight_count.load(Ordering::Relaxed) as u32;
            drop(stats.file_inflight.acquire_many(permits).await.unwrap());
            for name in ["first.txt", "last.txt"] {
                tx.try_send(CloneWorkItem {
                    repository: repository.clone(),
                    node: Node {
                        address: Default::default(),
                        ..file_node(b"")
                    },
                    repository_path: relative_path(name),
                })
                .unwrap();
            }
            drop(tx);
            let error = consumer
                .await
                .expect_err("missing content must fail the operation");
            assert!(caused_by::<AddressNotFound>(&error));
            for name in ["first.txt", "last.txt"] {
                assert_eq!(std::fs::read(temp.path().join(name)).unwrap(), b"");
            }
            assert_eq!(stats.complete.file_complete.load(Ordering::Relaxed), 2);
        })
        .await;
}

/// A revision can hold a node named for a Windows drive, which a writer that skips the name rules
/// stores. A clone skips the node, so nothing is created for it, and writes its siblings. On
/// Windows the name would take the path out of the working tree.
#[tokio::test]
async fn a_clone_skips_a_node_named_for_a_drive() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let temp = TempDir::new("lore-clone-drive-name-");
            let repository = working_tree_repository(temp.path()).await;
            let empty_file = Node {
                address: Default::default(),
                ..file_node(b"")
            };
            let state = State::new();
            for (name, node) in [
                ("alpha", empty_file),
                ("drive", empty_directory_node()),
                ("omega", empty_file),
            ] {
                stage_single_node(
                    repository.clone(),
                    state.clone(),
                    relative_path(name),
                    node,
                    Arc::default(),
                    None,
                    FilterMode::Full,
                )
                .await
                .expect("stage the node");
            }

            let planted = state
                .find_subnode(repository.clone(), ROOT_NODE, hash_string("drive"))
                .await
                .expect("the placeholder resolves");
            let block_index = NodeBlock::index(planted);
            let block = state
                .block_with_nametable(repository.clone(), block_index)
                .await
                .expect("the block reads back");
            {
                let mut writer = block.write_for_tests();
                let node = writer.node(Node::index(planted));
                let (offset, length) = (node.name_offset, node.name_length);
                node.name_hash = hash_string("C:");
                let (offset, length) = writer
                    .node_name_store_unchecked("C:", offset, length)
                    .expect("the unchecked store accepts the name");
                let node = writer.node(Node::index(planted));
                node.name_offset = offset;
                node.name_length = length;
                writer.mark_dirty();
            }
            state.block_modified_for_tests(block, block_index);

            let ctx = CloneContext {
                operation: repository
                    .file_system()
                    .begin_operation()
                    .await
                    .expect("Starting test operation"),
                repository,
                state,
                options: Arc::default(),
                stats: Arc::default(),
                modified_times: Arc::default(),
            };
            clone_in_path(ctx)
                .await
                .expect("clone past the planted node");

            let mut names: Vec<_> = std::fs::read_dir(temp.path())
                .expect("list the working tree")
                .map(|entry| entry.expect("an entry").file_name())
                .filter(|name| name != ".lore")
                .collect();
            names.sort();
            assert_eq!(names, ["alpha", "omega"]);
        })
        .await;
}

/// A clone whose check finds no metadata of its branch, which another writer stored before the
/// clone's own write, leaves that metadata, and claims no name for the branch.
#[tokio::test]
async fn a_clone_leaves_metadata_another_writer_stored_after_its_check() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let checked = Arc::new(AtomicBool::new(false));
            let check = checked.clone();
            let repository = crate::branch::hooked_repository(move |call, _key, _key_type| {
                if call == StoreCall::Load && !check.swap(true, Ordering::SeqCst) {
                    return Err(StoreError::internal(
                        "the clone's check reads before the other writer's store",
                    ));
                }
                Ok(())
            })
            .await;
            let cloned = BranchId::from([2; 16]);
            let stored = Hash::from([6; 32]);
            branch::mutable_try_store(
                repository.clone(),
                branch::METADATA,
                cloned,
                Hash::default(),
                stored,
            )
            .await
            .expect("another writer stores the branch's metadata");

            store_cloned_branch(
                repository.clone(),
                cloned,
                "cloned".to_string(),
                Hash::from([5; 32]),
                Hash::default(),
            )
            .await
            .expect("the clone stores its branch");

            assert_eq!(
                branch::metadata_hash(repository.clone(), cloned).await.ok(),
                Some(stored)
            );
            assert_eq!(
                branch::load_name_to_id_local(repository, "cloned")
                    .await
                    .ok(),
                None
            );
        })
        .await;
}

/// A clone that stores its branch where another live branch holds the branch's name succeeds: it
/// stores the cloned revision as the branch's latest and last sync, and the name stays with the
/// other branch.
#[tokio::test]
async fn a_clone_leaves_a_name_another_branch_holds() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let repository = null_repository().await;
            let (holder, cloned) = (BranchId::from([1; 16]), BranchId::from([2; 16]));
            let revision = Hash::from([3; 32]);
            branch::create_record(
                repository.clone(),
                holder,
                "cloned",
                branch::default_category(),
                "creator",
                1,
                Vec::new(),
                Hash::from([4; 32]),
                branch::BranchLatestStatus::Divergent,
                BranchId::default(),
            )
            .await
            .expect("an unclaimed name is written");

            store_cloned_branch(
                repository.clone(),
                cloned,
                "cloned".to_string(),
                Hash::from([5; 32]),
                revision,
            )
            .await
            .expect("the clone stores its branch");

            assert_eq!(
                branch::load_latest_local(repository.clone(), cloned)
                    .await
                    .ok(),
                Some(revision)
            );
            assert_eq!(
                branch::load_last_sync(repository.clone(), cloned)
                    .await
                    .ok(),
                Some(revision)
            );
            assert_eq!(
                branch::load_name_to_id_local(repository, "cloned")
                    .await
                    .ok(),
                Some(holder)
            );
        })
        .await;
}
