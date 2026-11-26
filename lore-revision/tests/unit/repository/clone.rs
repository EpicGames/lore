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
use lore_storage::test_util::StoreCall;
use zerocopy::FromZeros;

use crate::branch::null_repository;
use crate::fs::filesystem_provider::setup_test_execution;
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
async fn ensure_directory_creates_missing_ancestors() {
    let (temp, operation) = create_operation().await;
    let file = relative_path("nested/deeper/file.txt");
    let stats = CloneStats::default();

    ensure_directory(&file.parent_path(), &operation, &stats, false)
        .await
        .expect("missing parent should be created");

    assert!(on_disk(&temp, &file.parent_path()).is_dir());
}

/// The clone root already exists and is not ours to create: files at the top of the tree
/// take it as their parent, so this must not fail the clone.
#[tokio::test]
async fn ensure_directory_accepts_existing_parent() {
    let (_temp, operation) = create_operation().await;
    let file = relative_path("file.txt");
    let stats = CloneStats::default();

    ensure_directory(&file.parent_path(), &operation, &stats, false)
        .await
        .expect("existing parent should not fail");
}

#[tokio::test]
async fn ensure_directory_caches_parent_once_per_path() {
    let (_temp, operation) = create_operation().await;
    let stats = CloneStats::default();
    let first = relative_path("dir/a.txt");
    let second = relative_path("dir/b.txt");

    ensure_directory(&first.parent_path(), &operation, &stats, false)
        .await
        .expect("first file should create the parent");
    ensure_directory(&second.parent_path(), &operation, &stats, false)
        .await
        .expect("sibling should hit the cache");

    assert_eq!(stats.created_parents.len(), 1);
}

/// Tolerating an existing parent must not extend to tolerating a real failure: a file
/// sitting where the parent directory belongs still has to abort the clone.
#[tokio::test]
async fn ensure_directory_rejects_parent_that_is_a_file() {
    let (temp, operation) = create_operation().await;
    let blocker = relative_path("blocker");
    tokio::fs::File::create(on_disk(&temp, &blocker))
        .await
        .expect("create blocker file");
    let stats = CloneStats::default();

    let err = ensure_directory(&blocker, &operation, &stats, false)
        .await
        .expect_err("a file where the parent belongs must fail");

    assert!(err.to_string().contains("Failed to create directory"));
    assert!(stats.created_parents.is_empty());
}

/// A parent that genuinely cannot be created still has to abort the clone. Root can write the
/// read-only directory that stands in for one, so the test returns early as root.
#[cfg(unix)]
#[tokio::test]
async fn ensure_directory_rejects_uncreatable_parent() {
    use std::os::unix::fs::PermissionsExt;

    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
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

    let result = ensure_directory(&locked.join("child"), &operation, &stats, false).await;

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

    let result = ensure_directory(&link, &operation, &stats, false).await;

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

    let result = ensure_directory(&link, &operation, &stats, false).await;

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

    ensure_directory(&link, &operation, &stats, false)
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

    let result = ensure_directory(&blocker.join("deep"), &operation, &stats, false).await;

    assert!(result.is_err(), "a file cannot contain a directory");
}

#[tokio::test]
async fn adversarial_paths_without_a_usable_parent() {
    let (_temp, operation) = create_operation().await;
    let stats = CloneStats::default();

    // Filesystem root: no parent to create.
    ensure_directory(&relative_path("/"), &operation, &stats, false)
        .await
        .expect("root must be a no-op");
    // Bare relative name: parent is the empty path.
    ensure_directory(
        &relative_path("file.txt").parent_path(),
        &operation,
        &stats,
        false,
    )
    .await
    .expect("a bare relative name must be a no-op");
    // Empty path.
    ensure_directory(&RelativePath::new(), &operation, &stats, false)
        .await
        .expect("empty path must be a no-op");
}

/// A dry run creates nothing, and records each directory and ancestor once, so a directory two
/// calls share is listed once.
#[tokio::test]
async fn ensure_directory_in_a_dry_run_creates_nothing_and_records_each_directory_once() {
    let (temp, operation) = create_operation().await;
    let stats = CloneStats::default();

    for directory in ["a/b/c", "a/b/d", "a/b"] {
        ensure_directory(&relative_path(directory), &operation, &stats, true)
            .await
            .expect("a dry run fails nothing");
    }

    assert!(!on_disk(&temp, &relative_path("a")).exists());
    assert_eq!(
        stats.created_parents.len(),
        4,
        "a, a/b, a/b/c and a/b/d are each listed once"
    );
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
            ensure_directory(&file.parent_path(), &operation, &stats, false).await
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

/// Files of one new directory whose writers start together create it with one call, and a create
/// that fails fails each of them.
#[tokio::test]
async fn concurrent_files_of_a_new_directory_share_its_create() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            for blocked in [false, true] {
                let temp = TempDir::new("lore-clone-shared-create-");
                let provider =
                    crate::fs::filesystem_provider::TestFilesystemProvider::reading(temp.path());
                let creates = provider.directory_creates.clone();
                let operation = provider
                    .begin_operation()
                    .await
                    .expect("Starting test operation");
                if blocked {
                    std::fs::write(temp.path().join("shared"), b"").expect("block the directory");
                }
                let stats = CloneStats::default();
                let directory = relative_path("shared/nested");

                let results = futures::future::join_all(
                    (0..8).map(|_| ensure_directory(&directory, &operation, &stats, false)),
                )
                .await;

                assert_eq!(creates.load(Ordering::Relaxed), 1, "blocked {blocked}");
                assert!(
                    results.iter().all(|result| result.is_err() == blocked),
                    "blocked {blocked}: {results:?}"
                );
                assert!(stats.creating_parents.is_empty());
            }
        })
        .await;
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

/// The directories and files below `root`, as sorted paths relative to it, a directory's ending
/// in `/`.
fn tree_on_disk(root: &std::path::Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).expect("list a cloned directory") {
            let path = entry.expect("a cloned entry").path();
            let relative = path
                .strip_prefix(root)
                .expect("below the root")
                .to_string_lossy()
                .replace('\\', "/");
            if path.is_dir() {
                found.push(format!("{relative}/"));
                pending.push(path);
            } else {
                found.push(relative);
            }
        }
    }
    found.sort();
    found
}

/// Clones a revision holding `entries` under the view `rules`, answering what the clone holds as
/// [`tree_on_disk`] lists it. An entry ending in `/` is a directory and any other an empty file,
/// and every ancestor of an entry is a directory.
async fn cloned_tree(rules: &[&str], entries: &[&str]) -> Vec<String> {
    cloned_tree_and_stats(rules, entries).await.0
}

/// [`cloned_tree`], with the statistics of the clone.
async fn cloned_tree_and_stats(rules: &[&str], entries: &[&str]) -> (Vec<String>, Arc<CloneStats>) {
    let temp = TempDir::new("lore-clone-view-directories-");
    let repository =
        working_tree_repository_filtered(temp.path(), crate::helper::test_filter(&[], rules)).await;
    let stats = Arc::new(CloneStats::default());
    let state = State::new();
    let mut staged = Vec::<String>::new();
    for entry in entries {
        let mut prefix = String::new();
        for component in entry.trim_end_matches('/').split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            if staged.contains(&prefix) {
                continue;
            }
            let node = if prefix.len() < entry.len() {
                empty_directory_node()
            } else {
                Node {
                    address: Default::default(),
                    ..file_node(b"")
                }
            };
            stage_single_node(
                repository.clone(),
                state.clone(),
                relative_path(&prefix),
                node,
                Arc::default(),
                None,
                FilterMode::empty(),
            )
            .await
            .expect("stage the node");
            staged.push(prefix.clone());
        }
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
        stats: stats.clone(),
        modified_times: Arc::default(),
    };
    clone_in_path(ctx).await.expect("clone under the view");
    let tree = tree_on_disk(temp.path())
        .into_iter()
        .filter(|path| !path.starts_with(".lore"))
        .collect();
    (tree, stats)
}

/// Asserts that a clone of `entries` under each view in `cases` holds exactly what the case lists.
async fn assert_cloned_trees(cases: &[(&[&str], &[&str], &[&str])]) {
    for (rules, entries, expected) in cases {
        assert_eq!(
            cloned_tree(rules, entries).await,
            *expected,
            "a clone of {entries:?} under {rules:?}"
        );
    }
}

/// The view `**`, `!/a/**/*.bat`, `!/a/b/`, `!/a/c/file.txt`: `/a/b`, which the view keeps, and
/// `/a/c` and `/a/e/f`, which hold a file the view keeps, are created with `/a` above them, and
/// `/a/d`, which the walk enters only because the `.bat` re-inclusion could match below it, is
/// not.
#[tokio::test]
async fn a_clone_under_the_example_view_creates_what_it_keeps_and_not_a_d() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            assert_cloned_trees(&[(
                &["**", "!/a/**/*.bat", "!/a/b/", "!/a/c/file.txt"],
                &[
                    "a/b/x.txt",
                    "a/c/file.txt",
                    "a/c/other.txt",
                    "a/d/some.thing",
                    "a/e/run.bat",
                    "a/e/f/deep.bat",
                ],
                &[
                    "a/",
                    "a/b/",
                    "a/b/x.txt",
                    "a/c/",
                    "a/c/file.txt",
                    "a/e/",
                    "a/e/f/",
                    "a/e/f/deep.bat",
                    "a/e/run.bat",
                ],
            )])
            .await;
        })
        .await;
}

/// A clone creates each directory the view keeps, whatever it holds: nested directories holding
/// only files extension rules exclude, `a` under `/a/*` and `/a/**`, which exclude what `a` holds
/// and not `a`, and under `/foo/`, `!/foo/bar/`, `/foo/bar/something/specific/` the directory
/// `foo/bar/something`, which holds only the excluded `specific`.
#[tokio::test]
async fn a_clone_creates_each_directory_the_view_keeps_whatever_it_holds() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            assert_cloned_trees(&[
                (
                    &["*.bat", "*.uasset"],
                    &["a/b/c/x.bat", "a/b/y.uasset", "a/z.bat", "d/kept.txt"],
                    &["a/", "a/b/", "a/b/c/", "d/", "d/kept.txt"],
                ),
                (
                    &["/a/*"],
                    &["a/x.txt", "a/b/y.txt", "c.txt"],
                    &["a/", "c.txt"],
                ),
                (
                    &["/a/**"],
                    &["a/x.txt", "a/b/y.txt", "c.txt"],
                    &["a/", "c.txt"],
                ),
                (
                    &["/foo/", "!/foo/bar/", "/foo/bar/something/specific/"],
                    &[
                        "foo/top.txt",
                        "foo/other/o.txt",
                        "foo/bar/b.txt",
                        "foo/bar/something/specific/s.txt",
                        "foo/bar/something/specific/deep/d.txt",
                    ],
                    &["foo/", "foo/bar/", "foo/bar/b.txt", "foo/bar/something/"],
                ),
                (
                    &["/foo/", "!/foo/bar/", "/foo/bar/*"],
                    &["foo/bar/b.txt", "foo/bar/x/y.txt"],
                    &["foo/", "foo/bar/"],
                ),
            ])
            .await;
        })
        .await;
}

/// A clone tells a directory from what it holds. An exclusion of `/a` or `/a/` excludes `a` and
/// everything in it, and one of `/a/*` or `/a/**` excludes what `a` holds and keeps `a`. Below
/// `**`, a re-inclusion of `!/a` or `!/a/` keeps `a` and everything in it, `!/a/*` keeps the
/// entries of `a` and nothing below them, and `!/a/**` keeps everything below `a`: under the last
/// two `a` is created as the parent of what they keep, and not when it is empty. Below `/*`, which
/// excludes only the top level, `!/a/*` keeps everything below `a`. Followed by `!/a/b/*`, `/a/*`
/// lets what lies below `a/b/c` follow it, and `/a/**` still excludes it.
#[tokio::test]
async fn a_clone_tells_a_directory_from_what_it_holds() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let entries: &[&str] = &["a/f.txt", "a/b/g.txt", "a/b/c/", "a/e/", "z.txt"];
            let below_everything: &[&str] =
                &["a/", "a/b/", "a/b/c/", "a/b/g.txt", "a/e/", "a/f.txt"];
            assert_cloned_trees(&[
                (&["/a"], entries, &["z.txt"]),
                (&["/a/"], entries, &["z.txt"]),
                (&["/a/*"], entries, &["a/", "z.txt"]),
                (&["/a/**"], entries, &["a/", "z.txt"]),
                (&["**", "!/a"], entries, below_everything),
                (&["**", "!/a/"], entries, below_everything),
                (
                    &["**", "!/a/*"],
                    entries,
                    &["a/", "a/b/", "a/e/", "a/f.txt"],
                ),
                (&["**", "!/a/**"], entries, below_everything),
                (&["/*", "!/a/*"], entries, below_everything),
                (&["**", "!/a"], &["a/"], &["a/"]),
                (&["**", "!/a/"], &["a/"], &["a/"]),
                (&["**", "!/a/*"], &["a/"], &[]),
                (&["**", "!/a/**"], &["a/"], &[]),
                (
                    &["/a/*", "!/a/b/*"],
                    &["a/b/c/d.txt"],
                    &["a/", "a/b/", "a/b/c/", "a/b/c/d.txt"],
                ),
                (
                    &["/a/**", "!/a/b/*"],
                    &["a/b/c/d.txt"],
                    &["a/", "a/b/", "a/b/c/"],
                ),
            ])
            .await;
        })
        .await;
}

/// A clone creates a directory the view excludes only as the parent of what it creates below it:
/// not `Engine` under `/*`, `!*/Saved/Config/` with no `Saved/Config` in the revision, not `a/b`
/// under `**`, `!/a/**/*.bat` when it holds no `.bat` file, and not `a` under `**`, `!/a/b/c/`
/// with no `a/b/c` in the revision.
#[tokio::test]
async fn a_clone_creates_a_directory_the_view_excludes_only_above_what_it_keeps() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            assert_cloned_trees(&[
                (
                    &["/*", "!*/Saved/Config/"],
                    &["Engine/Build/b.txt", "Engine/Saved/Logs/l.txt"],
                    &[],
                ),
                (
                    &["**", "!/a/**/*.bat"],
                    &["a/b/x.txt", "a/c/y.bat"],
                    &["a/", "a/c/", "a/c/y.bat"],
                ),
                (&["**", "!/a/b/c/"], &["a/b/x.txt"], &[]),
            ])
            .await;
        })
        .await;
}

/// A clone creates a directory the view keeps, though the view excludes everything in it: `!/a/b/`
/// under `**`, a line keeping a directory the view already keeps, each directory a wildcard
/// re-inclusion matches, and the how-to's view that keeps `Engine/Content` empty. Each ancestor is
/// created as its parent.
#[tokio::test]
async fn a_clone_creates_a_kept_directory_whose_entries_the_view_excludes() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            assert_cloned_trees(&[
                (
                    &["**", "!/a/b/", "/a/b/*"],
                    &["a/b/x.txt", "a/b/c/y.txt", "a/z.txt"],
                    &["a/", "a/b/"],
                ),
                (
                    &["!/a/b/", "/a/b/*"],
                    &["a/b/x.txt", "a/b/c/y.txt"],
                    &["a/", "a/b/"],
                ),
                (
                    &["/*", "!*/Saved/Config/", "/*/Saved/Config/*"],
                    &[
                        "Engine/Saved/Config/a.ini",
                        "Game/Saved/Config/b.ini",
                        "Game/Saved/Logs/l.txt",
                    ],
                    &[
                        "Engine/",
                        "Engine/Saved/",
                        "Engine/Saved/Config/",
                        "Game/",
                        "Game/Saved/",
                        "Game/Saved/Config/",
                    ],
                ),
                (
                    &["/*", "!/Engine/Content/", "/Engine/Content/*"],
                    &["Engine/Content/a.uasset", "Engine/Source/s.cpp", "top.txt"],
                    &["Engine/", "Engine/Content/"],
                ),
            ])
            .await;
        })
        .await;
}

/// A clone creates every directory that holds, at any depth, an entry it creates: the chain above
/// a kept file, the excluded directories above a file a re-inclusion keeps, and a directory the
/// repository holds empty with the chain above it.
#[tokio::test]
async fn a_clone_creates_each_directory_holding_what_it_keeps() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            assert_cloned_trees(&[
                (
                    &["*.bat"],
                    &["a/b/c/d/kept.txt", "a/b/x.bat"],
                    &["a/", "a/b/", "a/b/c/", "a/b/c/d/", "a/b/c/d/kept.txt"],
                ),
                (
                    &["**", "!/a/c/file.txt"],
                    &["a/c/file.txt", "a/c/other.txt"],
                    &["a/", "a/c/", "a/c/file.txt"],
                ),
                (&["*.bat"], &["e/f/", "e/g.bat"], &["e/", "e/f/"]),
            ])
            .await;
        })
        .await;
}

/// A clone creates the directories above a file it keeps with the file, in one call for the file's
/// parent: under `!/a/**`, which keeps every directory below `a`, two files in sibling directories
/// cost one call each, and the clone holds the same tree.
#[tokio::test]
async fn a_clone_creates_the_directories_above_a_file_with_the_file() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let (tree, stats) =
                cloned_tree_and_stats(&["**", "!/a/**"], &["a/b/c/f.txt", "a/b/d/g.txt"]).await;
            assert_eq!(
                tree,
                [
                    "a/",
                    "a/b/",
                    "a/b/c/",
                    "a/b/c/f.txt",
                    "a/b/d/",
                    "a/b/d/g.txt"
                ]
            );
            assert_eq!(
                stats.created_parents.len(),
                2,
                "one directory call per file"
            );
        })
        .await;
}

/// A clone creates a chain of directories the view keeps and no file is written in with one call
/// for its deepest directory: under `*.bat`, `a/b/c` holding only a `.bat` file costs one call
/// for `a`, `a/b` and `a/b/c`. Under `/foo/`, `!/foo/bar/`, `/foo/bar/something/specific/`, the
/// file written in `foo/bar` creates it with `foo`, and `foo/bar/something` costs one more call.
#[tokio::test]
async fn a_clone_creates_a_chain_of_kept_directories_with_one_call() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            type Case = (
                &'static [&'static str],
                &'static [&'static str],
                &'static [&'static str],
                usize,
            );
            let cases: &[Case] = &[
                (&["*.bat"], &["a/b/c/x.bat"], &["a/", "a/b/", "a/b/c/"], 1),
                (
                    &["/foo/", "!/foo/bar/", "/foo/bar/something/specific/"],
                    &["foo/bar/b.txt", "foo/bar/something/specific/s.txt"],
                    &["foo/", "foo/bar/", "foo/bar/b.txt", "foo/bar/something/"],
                    2,
                ),
            ];
            for (rules, entries, expected, calls) in cases {
                let (tree, stats) = cloned_tree_and_stats(rules, entries).await;
                assert_eq!(tree, *expected, "a clone of {entries:?} under {rules:?}");
                assert_eq!(
                    stats.created_parents.len(),
                    *calls,
                    "the directory calls of a clone of {entries:?} under {rules:?}"
                );
            }
        })
        .await;
}

/// A clone creates a directory the view keeps, though the view excludes everything in it, when its
/// entries continue into another node block: the walk carries the directory into the block it
/// continues in and creates it after the last entry there.
#[tokio::test]
async fn a_clone_creates_a_kept_directory_whose_excluded_entries_span_blocks() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let entries: Vec<String> = (0..SPANNING_ROOT_FILES)
                .map(|index| format!("d/f{index:04}"))
                .collect();
            let entries: Vec<&str> = entries.iter().map(String::as_str).collect();
            assert_cloned_trees(&[(&["!/d/", "/d/*"], &entries, &["d/"])]).await;
        })
        .await;
}

/// A clone with no view creates every directory the revision holds, empty or not.
#[tokio::test]
async fn a_clone_with_no_view_creates_every_directory() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            assert_cloned_trees(&[(
                &[],
                &["a/b/x.bat", "c/", "d/e/"],
                &["a/", "a/b/", "a/b/x.bat", "c/", "d/", "d/e/"],
            )])
            .await;
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

/// Copies the directory `from` to `to`, files and directories alike.
fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).expect("make the copy");
    for entry in std::fs::read_dir(from).expect("list the directory") {
        let entry = entry.expect("read an entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("stat an entry").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy a file");
        }
    }
}

/// A clone or create at the path of a registered virtual instance, here one whose checkout is gone
/// and whose external directory names FUSE, is refused with `InvalidArguments` before anything is
/// written at the path. A write-mode open of an unregistered instance there registers nothing,
/// and the virtual instance stays registered.
#[tokio::test]
async fn creating_at_a_registered_virtual_instance_path_is_refused_and_writes_nothing() {
    use lore_revision::global::external_dir::external_dir_for_instance;
    use lore_revision::global::external_dir::resolve::resolve;
    use lore_revision::instance::InstanceId;
    use lore_revision::instance::instance_key;
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository::DOT_LORE;
    use lore_revision::repository::INSTANCE;
    use lore_revision::repository::RepositoryAccess;
    use lore_revision::repository::RepositoryConfig;
    use lore_revision::repository::RepositoryWriteToken;
    use lore_revision::repository::SharedStoreToUseConfig;
    use lore_revision::repository::create_local;
    use lore_revision::repository::load_and_connect_with_token;

    let global = TempDir::new("lore-clone-virtual-path-global-");
    let shared = TempDir::new("lore-clone-virtual-path-store-");
    let parent = TempDir::new("lore-clone-virtual-path-");
    let held = TempDir::new("lore-clone-virtual-path-held-");
    LORE_CONTEXT
        .scope(crate::helper::execution_in(global.path()), async {
            let path = parent.path().join("instance");
            std::fs::create_dir(&path).expect("make the instance path");
            let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
            let config = RepositoryConfig {
                remote_url: Some("lore://127.0.0.1:1/virtual-path".to_owned()),
                shared_store_to_use: Some(SharedStoreToUseConfig {
                    use_shared_store: Some(true),
                    shared_store_path: Some(shared.path().display().to_string()),
                }),
                ..Default::default()
            };
            let create = |config: RepositoryConfig| {
                let path = path.clone();
                async move {
                    let token = RepositoryWriteToken::acquire(&path).await;
                    create_local(
                        &path,
                        &token,
                        repository_id,
                        BranchId::from(uuid::Uuid::now_v7()),
                        branch::DEFAULT_DEFAULT_NAME.to_string(),
                        config,
                        false,
                    )
                    .await
                }
            };
            let virtual_instance = create(config.clone()).await.expect("create the instance");
            let virtual_id = virtual_instance.instance_id;
            drop(virtual_instance);
            copy_tree(&path.join(DOT_LORE), held.path());
            std::fs::remove_dir_all(path.join(DOT_LORE)).expect("leave an empty mount point");
            let state = resolve().expect("resolve the external root");
            let dot_lore = external_dir_for_instance(&state, virtual_id)
                .expect("the external directory")
                .join(lore_revision::repository::DOT_LORE);
            std::fs::create_dir_all(&dot_lore).expect("make the external directory");
            std::fs::write(
                dot_lore.join("config.toml"),
                b"[vfs]\nvfs_type = \"Fuse\"\n",
            )
            .expect("write the config");

            let refused = create(config).await;

            match refused {
                Err(error) => assert!(error.is_invalid_arguments(), "{error}"),
                Ok(_) => panic!("creating at a virtual instance's path is refused"),
            }
            assert_eq!(
                std::fs::read_dir(&path).expect("list the path").count(),
                0,
                "nothing is written at the path"
            );

            copy_tree(held.path(), &path.join(DOT_LORE));
            let unregistered = InstanceId::generate();
            std::fs::write(path.join(DOT_LORE).join(INSTANCE), unregistered.data())
                .expect("give the checkout an unregistered instance");
            let token = RepositoryWriteToken::acquire(&path).await;
            let opened =
                load_and_connect_with_token(&path, RepositoryAccess::ReadWrite, Some(token))
                    .await
                    .expect("the open goes ahead");
            let registered = |instance_id| {
                let opened = opened.clone();
                async move {
                    let (key, key_type) = instance_key(opened.salt(), instance_id);
                    opened
                        .read_mutable_store()
                        .load(opened.id, key, key_type)
                        .await
                        .is_ok_and(|registration| !registration.is_zero())
                }
            };
            assert!(
                !registered(unregistered).await,
                "the open registers nothing"
            );
            assert!(
                registered(virtual_id).await,
                "the virtual instance stays registered"
            );
        })
        .await;
}

/// A creation over a shared store that already exists opens each of its stores once, registers the
/// new instance in it, and leaves the instances registered before it.
#[tokio::test]
async fn creating_over_an_existing_shared_store_opens_it_once_and_registers_the_instance() {
    use lore_revision::instance::instance_key;
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository::RepositoryConfig;
    use lore_revision::repository::RepositoryWriteToken;
    use lore_revision::repository::SharedStoreToUseConfig;
    use lore_revision::repository::create_local;
    use lore_revision::repository::store_opens::opens_in;
    use lore_revision::shared_store::get_shared_store_path_for_repo;

    let global = TempDir::new("lore-create-shared-global-");
    let shared = TempDir::new("lore-create-shared-store-");
    let parent = TempDir::new("lore-create-shared-");
    LORE_CONTEXT
        .scope(crate::helper::execution_in(global.path()), async {
            let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
            let config = RepositoryConfig {
                remote_url: Some("lore://127.0.0.1:1/create-shared".to_owned()),
                shared_store_to_use: Some(SharedStoreToUseConfig {
                    use_shared_store: Some(true),
                    shared_store_path: Some(shared.path().display().to_string()),
                }),
                ..Default::default()
            };
            let create = |name: &str| {
                let path = parent.path().join(name);
                let config = config.clone();
                async move {
                    let token = RepositoryWriteToken::acquire(&path).await;
                    create_local(
                        &path,
                        &token,
                        repository_id,
                        BranchId::from(uuid::Uuid::now_v7()),
                        branch::DEFAULT_DEFAULT_NAME.to_string(),
                        config,
                        false,
                    )
                    .await
                    .expect("create the instance")
                }
            };
            let first = create("first").await;
            let first_id = first.instance_id;
            drop(first);
            let store = get_shared_store_path_for_repo(&config)
                .await
                .expect("find the shared store")
                .expect("the shared store exists");
            let before = opens_in(&store);

            let second = create("second").await;

            let after = opens_in(&store);
            assert_eq!(
                (
                    after.immutable - before.immutable,
                    after.mutable - before.mutable
                ),
                (1, 1),
                "the creation opens each store once"
            );

            for instance_id in [first_id, second.instance_id] {
                let (key, key_type) = instance_key(second.salt(), instance_id);
                let registration = second
                    .read_mutable_store()
                    .load(second.id, key, key_type)
                    .await
                    .expect("the instance is registered");
                assert!(!registration.is_zero());
            }
        })
        .await;
}

/// A creation over a shared store that already exists refuses a path whose Lore directory holds a
/// mutable store of its own.
#[tokio::test]
async fn creating_over_an_existing_shared_store_refuses_a_local_mutable_store() {
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository::DOT_LORE;
    use lore_revision::repository::RepositoryConfig;
    use lore_revision::repository::RepositoryWriteToken;
    use lore_revision::repository::SharedStoreToUseConfig;
    use lore_revision::repository::create_local;

    let global = TempDir::new("lore-create-local-mutable-global-");
    let shared = TempDir::new("lore-create-local-mutable-store-");
    let parent = TempDir::new("lore-create-local-mutable-");
    LORE_CONTEXT
        .scope(crate::helper::execution_in(global.path()), async {
            let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
            let config = RepositoryConfig {
                remote_url: Some("lore://127.0.0.1:1/create-local-mutable".to_owned()),
                shared_store_to_use: Some(SharedStoreToUseConfig {
                    use_shared_store: Some(true),
                    shared_store_path: Some(shared.path().display().to_string()),
                }),
                ..Default::default()
            };
            let create = |path: std::path::PathBuf| {
                let config = config.clone();
                async move {
                    let token = RepositoryWriteToken::acquire(&path).await;
                    create_local(
                        &path,
                        &token,
                        repository_id,
                        BranchId::from(uuid::Uuid::now_v7()),
                        branch::DEFAULT_DEFAULT_NAME.to_string(),
                        config,
                        false,
                    )
                    .await
                }
            };
            drop(
                create(parent.path().join("first"))
                    .await
                    .expect("create the shared store's first instance"),
            );
            let second = parent.path().join("second");
            std::fs::create_dir_all(second.join(DOT_LORE).join("mutable"))
                .expect("make a local mutable store");

            let refused = create(second).await;

            match refused {
                Err(error) => assert!(error.to_string().contains("local mutable store"), "{error}"),
                Ok(_) => panic!("a local mutable store is refused"),
            }
        })
        .await;
}

/// A clone's options give the context it writes its files through direct file write where they
/// ask for it, which its reads into files then take, and leave it as the instance's config sets it
/// otherwise. The clone uploads nothing either way.
#[tokio::test]
async fn a_clone_asked_for_direct_file_write_writes_its_files_directly() {
    for direct in [false, true] {
        let (immutable, mutable) = lore_revision::repository::create_client_memory_stores()
            .await
            .expect("in-memory stores should be creatable");
        let repository = RepositoryContext::new(crate::helper::default_repository_creation_args(
            immutable, mutable,
        ));

        apply_clone_settings(
            &repository,
            &CloneOptions {
                direct_file_write: direct,
                ..Default::default()
            },
        );

        assert_eq!(
            lore_revision::immutable::read_options_from_repository(&repository).direct_write,
            direct
        );
        assert!(repository.disable_upload());
    }
}
