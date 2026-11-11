// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! What a commit keeps of the moves `lore dirty move` marked and no stage took, where a staged
//! change displaces them or they wait on each other, and the warnings it logs: in a process of its
//! own, as the tests install the process-wide log callback.
// Fixtures write in their own temporary directories, outside any repository write-token discipline.
#![allow(clippy::disallowed_methods)]

#[path = "helper.rs"]
mod helper;

#[allow(dead_code)]
#[path = "support/dirty_move_fixture.rs"]
mod dirty_move_fixture;

#[path = "commit_dirty_moves/generated.rs"]
mod generated;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use lore_base::log::LoreLogLevel;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_revision::commit::CommitOptions;
use lore_revision::event::LoreEvent;
use lore_revision::file::dirty::MovedNode;
use lore_revision::file::dirty::mark_moved_again;
use lore_revision::fs::filesystem_provider::FilesystemProvider;
use lore_revision::fs::filesystem_provider::FsError;
use lore_revision::fs::filesystem_provider::InstanceOperationImpl;
use lore_revision::fs::filesystem_provider::StaticDispatchInstanceOperation;
use lore_revision::fs::filesystem_provider::test_util::TestOperation;
use lore_revision::fs::os::OsFilesystem;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreEventCallback;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreNodeType;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::NodeIDExt;
use lore_revision::node::ROOT_NODE;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::Mark;
use lore_revision::state::State;
use parking_lot::Mutex;

use crate::dirty_move_fixture::chained;
use crate::dirty_move_fixture::children_named;
use crate::dirty_move_fixture::committed;
use crate::dirty_move_fixture::dirtied;
use crate::dirty_move_fixture::dirty_move;
use crate::dirty_move_fixture::dirty_moved;
use crate::dirty_move_fixture::made_directory;
use crate::dirty_move_fixture::node_at;
use crate::dirty_move_fixture::node_id_at;
use crate::dirty_move_fixture::node_of;
use crate::dirty_move_fixture::removed_directory;
use crate::dirty_move_fixture::restaged;
use crate::dirty_move_fixture::stage_path;
use crate::dirty_move_fixture::staged_tree;
use crate::dirty_move_fixture::tree_paths;
use crate::dirty_move_fixture::with_instance;
use crate::dirty_move_fixture::written;
use crate::helper::RepositoryContextCreationArgsExt;
use crate::helper::TestRepository;
use crate::helper::default_repository_creation_args;
use crate::helper::relative_path;
use crate::helper::test_commit;

/// Asserts that `revision` holds exactly `expected`, each path with its node, and that each of its
/// directories holds the address its children hash to, as a commit rehashing it derives.
async fn assert_revision(instance: &TestRepository, revision: &State, expected: &[(&str, NodeID)]) {
    let held = tree_paths(instance, revision).await;
    let expected: BTreeMap<String, NodeID> = expected
        .iter()
        .map(|(path, node)| ((*path).to_string(), *node))
        .collect();
    assert_eq!(held, expected, "the revision's paths");
    assert_addresses(instance, revision, &held).await;
}

/// Asserts that each directory of `revision`, which holds `held`, holds the address its children
/// hash to, as a commit rehashing it derives.
async fn assert_addresses(
    instance: &TestRepository,
    revision: &State,
    held: &BTreeMap<String, NodeID>,
) {
    let rehashed = State::deserialize(instance.repository.clone(), revision.revision())
        .await
        .expect("the revision again");
    let mut directories = vec![ROOT_NODE];
    for node in held.values() {
        if node_of(instance, revision, *node).await.is_directory() {
            directories.push(*node);
        }
    }
    for directory in &directories {
        rehashed
            .node_mark(
                &instance.repository,
                *directory,
                Mark::staged(NodeFlags::Staged, false),
            )
            .await
            .expect("the directory marked for its rehash");
    }
    lore_revision::commit::rehash_directory(
        instance.repository.clone(),
        rehashed.clone(),
        ROOT_NODE,
    )
    .await
    .expect("the rehash");
    let root = revision
        .tree(instance.repository.clone())
        .await
        .expect("the revision's tree")
        .hash_root;
    assert_eq!(
        node_of(instance, &rehashed, ROOT_NODE).await.address.hash,
        root,
        "the root holds the hash its children hash to"
    );
    for directory in directories.into_iter().skip(1) {
        assert_eq!(
            node_of(instance, &rehashed, directory).await.address,
            node_of(instance, revision, directory).await.address,
            "directory {directory} holds the address its children hash to"
        );
    }
}

/// Asserts that `revision` holds exactly what `held` does, each path with its node, as
/// [`assert_revision`] does.
async fn assert_revision_holds(
    instance: &TestRepository,
    revision: &State,
    held: &BTreeMap<String, NodeID>,
) {
    let expected: Vec<(&str, NodeID)> = held
        .iter()
        .map(|(path, node)| (path.as_str(), *node))
        .collect();
    assert_revision(instance, revision, &expected).await;
}

/// Asserts that `revision` holds exactly the paths of `moved`, each with the node `held` names at
/// the committed path beside it, and `added`, each with its node, as [`assert_revision`] does.
async fn assert_revision_of(
    instance: &TestRepository,
    revision: &State,
    held: &BTreeMap<String, NodeID>,
    moved: &[(&str, &str)],
    added: &[(&str, NodeID)],
) {
    let mut expected: Vec<(&str, NodeID)> = moved
        .iter()
        .map(|(path, committed)| (*path, held[*committed]))
        .collect();
    expected.extend_from_slice(added);
    assert_revision(instance, revision, &expected).await;
}

/// Asserts that the instance's staged tree holds exactly the paths of `expected` and `added`.
/// Each path of `expected` holds the node `held` names at the committed path beside it, marked
/// dirty as moved and not staged where the two paths differ. Each of `added` holds a node marked
/// dirty as added and not staged.
async fn assert_staged(
    instance: &TestRepository,
    held: &BTreeMap<String, NodeID>,
    expected: &[(&str, &str)],
    added: &[&str],
) {
    let staged = staged_tree(instance).await;
    let paths = tree_paths(instance, &staged).await;
    let found: BTreeSet<&str> = paths.keys().map(String::as_str).collect();
    let wanted: BTreeSet<&str> = expected
        .iter()
        .map(|(path, _)| *path)
        .chain(added.iter().copied())
        .collect();
    assert_eq!(found, wanted, "the staged tree's paths");
    for (path, committed) in expected {
        let node = paths[*path];
        assert_eq!(node, held[*committed], "the node at {path}");
        if path != committed {
            let moved = node_of(instance, &staged, node).await;
            assert!(
                moved.is_dirty_move() && !moved.is_staged(),
                "{path}: flags {:#x}",
                moved.flags
            );
        }
    }
    for path in added {
        let node = node_of(instance, &staged, paths[*path]).await;
        assert!(
            node.is_dirty_add() && !node.is_staged(),
            "{path}: flags {:#x}",
            node.flags
        );
    }
}

/// Asserts that the instance holds no dirty mark: no staged tree, or one with no dirty node.
async fn assert_nothing_dirty(instance: &TestRepository) {
    let (_, staged, _) = State::deserialize_current_and_staged(instance.repository.clone())
        .await
        .expect("the states");
    if let Some(staged) = staged {
        let dirty = staged
            .node_has_dirty_children(instance.repository.clone(), ROOT_NODE)
            .await
            .expect("the root's children");
        assert!(!dirty, "the staged tree holds dirty marks");
    }
}

/// Asserts that the instance's staged tree, where it holds one, marks no node dirty with an action
/// besides a move: a node marked moved keeps that mark past a commit that took the move.
async fn assert_no_change_marked(instance: &TestRepository) {
    let (_, staged, _) = State::deserialize_current_and_staged(instance.repository.clone())
        .await
        .expect("the states");
    let Some(staged) = staged else {
        return;
    };
    for (path, node) in tree_paths(instance, &staged).await {
        let node = node_of(instance, &staged, node).await;
        assert!(
            !node.is_dirty_add() && !node.is_dirty_modify() && !node.is_dirty_delete(),
            "{path}: flags {:#x}",
            node.flags
        );
    }
}

/// Sends each message logged to the execution context it is logged under, as an event, as the
/// library's log dispatch does.
fn log_to_context(level: LoreLogLevel, _: &str, message: &str) {
    if let Some(execution) = lore_revision::lore::try_execution_context() {
        execution
            .dispatcher
            .send(LoreEvent::Log(EventDispatcher::make_log(
                level,
                message.to_string(),
            )));
    }
}

/// Runs `body` in an execution context of its own, answering what it answers and the warnings it
/// logged.
async fn warned<F: Future>(body: F) -> (F::Output, Vec<String>) {
    lore_base::log::set_log_callback(Some(log_to_context));
    let warnings = Arc::new(Mutex::new(Vec::new()));
    let logged = warnings.clone();
    let callback: LoreEventCallback = Some(Box::new(move |event| {
        if let LoreEvent::Log(log) = event
            && log.level == LoreLogLevel::Warn
        {
            logged.lock().push(log.message.to_string());
        }
    }));
    let execution = Arc::new(ExecutionContext::new_client_with_user_id(
        LoreGlobalArgs::default(),
        EventDispatcher::new(callback),
        "test-user".to_string(),
    ));
    let output = LORE_CONTEXT.scope(execution.clone(), body).await;
    execution.dispatcher.drain().await;
    let warnings = std::mem::take(&mut *warnings.lock());
    (output, warnings)
}

/// [`warned`], answering how many warnings `body` logged.
async fn warnings_of<F: Future>(body: F) -> (F::Output, usize) {
    let (output, warnings) = warned(body).await;
    (output, warnings.len())
}

/// Commits what `instance` holds staged, as [`test_commit`] does, answering the revision and how
/// many warnings the commit logged.
async fn committed_warning(instance: &TestRepository, message: &str) -> (Arc<State>, usize) {
    warnings_of(test_commit(instance, message)).await
}

/// Moves the node at `path` to the front of its parent's children, so a walk of them meets it
/// first: through `front`, a directory marked dirty as added, which stays.
async fn to_front(instance: &TestRepository, path: &str) {
    if !instance.path.join("front").exists() {
        made_directory(instance, "front");
        dirtied(instance, "front").await;
    }
    let through = format!("front/{}", path.replace('/', "_"));
    dirty_moved(instance, path, &through).await;
    dirty_moved(instance, &through, path).await;
}

/// A filesystem provider whose operations read the working tree at `root`, except at the paths
/// `failing` names, whose file information they cannot read.
struct FailingFileInfos {
    root: PathBuf,
    failing: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl FilesystemProvider for FailingFileInfos {
    async fn begin_operation(&self) -> Result<Arc<InstanceOperationImpl>, FsError> {
        Ok(Arc::new(InstanceOperationImpl::new(
            StaticDispatchInstanceOperation::Test(TestOperation {
                reads: Some(OsFilesystem::new(&self.root).begin_operation()),
                failing_file_infos: self.failing.clone(),
                ..TestOperation::default()
            }),
            false,
        )))
    }
}

/// A context of `instance` over [`FailingFileInfos`], failing at `failing`.
fn failing_context(instance: &TestRepository, failing: &[&str]) -> Arc<RepositoryContext> {
    let provider = FailingFileInfos {
        root: instance.path.clone(),
        failing: Arc::new(Mutex::new(
            failing.iter().map(|path| (*path).to_string()).collect(),
        )),
    };
    Arc::new(
        RepositoryContext::new(
            default_repository_creation_args(
                instance.repository.immutable_store(),
                instance.repository.mutable_store.clone(),
            )
            .with_path(&instance.path)
            .with_id(instance.repository.id)
            .with_instance_id(instance.repository.instance_id)
            .with_filesystem_provider(Arc::new(provider)),
        )
        .with_write_token(instance.write_token.share()),
    )
}

/// A file `lore dirty move` moved out of a committed directory whose deletion is then staged goes
/// back into it, and leaves the revision with it: the revision holds the committed tree with the
/// staged changes alone. The commit warns that the move is not kept, and the working tree's file
/// at the moved path is marked dirty as added.
#[tokio::test]
async fn a_commit_deletes_a_dirty_moved_file_with_its_committed_directory() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["d/f.txt", "other.txt"]).await;
        let other = node_id_at(&instance, &first, "other.txt").await;
        dirty_moved(&instance, "d/f.txt", "f.txt").await;
        removed_directory(&instance, "d");
        let state = stage_path(&instance, "d").await;
        let d = node_at(&instance, &state, "d").await;
        assert!(d.is_staged_delete(), "flags {:#x}", d.flags);

        let (revision, warnings) = committed_warning(&instance, "Deleted").await;
        assert_revision(&instance, &revision, &[("other.txt", other)]).await;
        assert_eq!(warnings, 1);

        let staged = staged_tree(&instance).await;
        let added = node_at(&instance, &staged, "f.txt").await;
        assert!(
            added.is_dirty_add() && !added.is_staged(),
            "flags {:#x}",
            added.flags
        );
    })
    .await;
}

/// A file moved with `lore dirty move` whose committed path a staged file has taken leaves the
/// revision: the staged file takes the path, and the moved file commits nowhere. The commit warns,
/// and the working tree's file at the moved path is marked dirty as added.
#[tokio::test]
async fn a_commit_drops_a_dirty_moved_file_whose_committed_path_a_staged_file_takes() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["base.txt", "other.txt"]).await;
        let base = node_id_at(&instance, &first, "base.txt").await;
        let other = node_id_at(&instance, &first, "other.txt").await;
        dirty_moved(&instance, "base.txt", "moved.txt").await;
        written(&instance, &["base.txt"]);
        let state = stage_path(&instance, "base.txt").await;
        let replacement = node_id_at(&instance, &state, "base.txt").await;
        assert_ne!(replacement, base);

        let (revision, warnings) = committed_warning(&instance, "Replaced").await;
        assert_revision(
            &instance,
            &revision,
            &[("base.txt", replacement), ("other.txt", other)],
        )
        .await;
        assert_eq!(warnings, 1);

        let staged = staged_tree(&instance).await;
        let added = node_at(&instance, &staged, "moved.txt").await;
        assert!(
            added.is_dirty_add() && !added.is_staged(),
            "flags {:#x}",
            added.flags
        );
    })
    .await;
}

/// A directory moved with `lore dirty move` out of its committed parent, which a second dirty move
/// put below it and a staged file then displaced from its committed path, leaves the revision with
/// its subtree: the revision holds the staged file where the parent was, and neither directory.
/// The commit warns once, and the working tree's directory at the moved path is marked dirty as
/// added, with what it holds.
#[tokio::test]
async fn a_commit_drops_dirty_moved_directories_a_staged_file_displaces() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["p/x/f.txt", "other.txt"]).await;
        let other = node_id_at(&instance, &first, "other.txt").await;
        dirty_moved(&instance, "p/x", "x").await;
        dirty_moved(&instance, "p", "x/p").await;
        written(&instance, &["p"]);
        let state = stage_path(&instance, "p").await;
        let replacement = node_id_at(&instance, &state, "p").await;

        let (revision, warnings) = committed_warning(&instance, "Replaced").await;
        assert_revision(
            &instance,
            &revision,
            &[("other.txt", other), ("p", replacement)],
        )
        .await;
        assert_eq!(warnings, 1);

        let staged = staged_tree(&instance).await;
        for path in ["x", "x/f.txt", "x/p"] {
            let added = node_at(&instance, &staged, path).await;
            assert!(
                added.is_dirty_add() && !added.is_staged(),
                "{path}: flags {:#x}",
                added.flags
            );
        }
    })
    .await;
}

/// A file moved with `lore dirty move` back into a committed directory, which a second dirty move
/// put inside a directory staged for deletion and a staged file then displaced, leaves the revision
/// with them, and the commit completes: no move is marked again, and the working tree's file at
/// the moved path is marked dirty as added.
#[tokio::test]
async fn a_commit_completes_where_a_returned_move_leaves_the_revision() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["p/x.txt", "d/keep.txt", "other.txt"]).await;
        let other = node_id_at(&instance, &first, "other.txt").await;
        dirty_moved(&instance, "p/x.txt", "x.txt").await;
        removed_directory(&instance, "d");
        stage_path(&instance, "d").await;
        dirty_move(&instance, "p", "d/p").await;
        removed_directory(&instance, "p");
        written(&instance, &["p"]);
        let state = stage_path(&instance, "p").await;
        let replacement = node_id_at(&instance, &state, "p").await;

        let (revision, warnings) = committed_warning(&instance, "Deleted").await;
        assert_revision(
            &instance,
            &revision,
            &[("other.txt", other), ("p", replacement)],
        )
        .await;
        assert_eq!(warnings, 1);
        let staged = staged_tree(&instance).await;
        let x = node_at(&instance, &staged, "x.txt").await;
        assert!(x.is_dirty_add() && !x.is_staged(), "flags {:#x}", x.flags);
    })
    .await;
}

/// A chain of moves `lore dirty move` marked and no stage took leaves the revision as committed,
/// whichever of the two the commit reaches first, and both stay marked: here the second move,
/// which goes back first, as it holds the first one's path at the front of their directory.
#[tokio::test]
async fn a_commit_leaves_out_a_chain_of_dirty_moves_met_from_its_end() {
    with_instance(|instance| async move {
        let (x, y, held) = chained(&instance).await;
        restaged(&instance, "other.txt").await;

        let (revision, warnings) = committed_warning(&instance, "Edited").await;
        assert_revision_holds(&instance, &revision, &held).await;
        assert_eq!(warnings, 0);

        let staged = staged_tree(&instance).await;
        assert_eq!(node_id_at(&instance, &staged, "h/sub/x.txt").await, x);
        assert_eq!(node_id_at(&instance, &staged, "h/x.txt").await, y);
        for node in [x, y] {
            let moved = node_of(&instance, &staged, node).await;
            assert!(
                moved.is_dirty_move() && !moved.is_staged(),
                "flags {:#x}",
                moved.flags
            );
        }
    })
    .await;
}

/// A chain of moves as [`a_commit_leaves_out_a_chain_of_dirty_moves_met_from_its_end`] makes, met
/// from its start: the first move, into a new directory at the front of theirs, waits for the
/// second to go back.
#[tokio::test]
async fn a_commit_leaves_out_a_chain_of_dirty_moves_met_from_its_start() {
    with_instance(|instance| async move {
        let (x, y, held) = chained(&instance).await;
        made_directory(&instance, "h/sub2");
        dirtied(&instance, "h/sub2").await;
        dirty_moved(&instance, "h/sub/x.txt", "h/sub2/x.txt").await;
        restaged(&instance, "other.txt").await;

        let (revision, warnings) = committed_warning(&instance, "Edited").await;
        assert_revision_holds(&instance, &revision, &held).await;
        assert_eq!(warnings, 0);

        let staged = staged_tree(&instance).await;
        assert_eq!(node_id_at(&instance, &staged, "h/sub2/x.txt").await, x);
        assert_eq!(node_id_at(&instance, &staged, "h/x.txt").await, y);
        for node in [x, y] {
            let moved = node_of(&instance, &staged, node).await;
            assert!(
                moved.is_dirty_move() && !moved.is_staged(),
                "flags {:#x}",
                moved.flags
            );
        }
    })
    .await;
}

/// Two files `lore dirty move` swapped through a third name, none staged, leave the revision as
/// committed: one waits by a name of its own while the other goes back. Both stay marked moved.
#[tokio::test]
async fn a_commit_leaves_out_a_swap_of_dirty_moves() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["a.txt", "b.txt", "other.txt"]).await;
        let a = node_id_at(&instance, &first, "a.txt").await;
        let b = node_id_at(&instance, &first, "b.txt").await;
        let other = node_id_at(&instance, &first, "other.txt").await;
        dirty_moved(&instance, "a.txt", "t.txt").await;
        dirty_moved(&instance, "b.txt", "a.txt").await;
        dirty_moved(&instance, "t.txt", "b.txt").await;
        restaged(&instance, "other.txt").await;

        let (revision, warnings) = committed_warning(&instance, "Edited").await;
        assert_revision(
            &instance,
            &revision,
            &[("a.txt", a), ("b.txt", b), ("other.txt", other)],
        )
        .await;
        assert_eq!(warnings, 0);

        let staged = staged_tree(&instance).await;
        assert_eq!(node_id_at(&instance, &staged, "b.txt").await, a);
        assert_eq!(node_id_at(&instance, &staged, "a.txt").await, b);
        assert_eq!(
            children_named(&instance, &staged, ROOT_NODE, "a.txt").await,
            1
        );
        assert_eq!(
            children_named(&instance, &staged, ROOT_NODE, "b.txt").await,
            1
        );
        for node in [a, b] {
            let moved = node_of(&instance, &staged, node).await;
            assert!(
                moved.is_dirty_move() && !moved.is_staged(),
                "flags {:#x}",
                moved.flags
            );
        }
    })
    .await;
}

/// A directory `lore dirty move` moved out of the way, a second moved onto its name, and the first
/// moved into the second, none staged: the commit leaves all three out, and the staged tree holds
/// each where the working tree does, the first inside the second.
#[tokio::test]
async fn a_commit_keeps_a_dirty_moved_directory_inside_the_one_that_took_its_name() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["lib/l.txt", "lib_new/n.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        dirty_moved(&instance, "lib", "lib_old").await;
        dirty_moved(&instance, "lib_new", "lib").await;
        dirty_moved(&instance, "lib_old", "lib/legacy").await;
        restaged(&instance, "other.txt").await;

        let (revision, warnings) = committed_warning(&instance, "Edited").await;
        assert_revision_holds(&instance, &revision, &held).await;
        assert_eq!(warnings, 0);
        assert_staged(
            &instance,
            &held,
            &[
                ("lib", "lib_new"),
                ("lib/legacy", "lib"),
                ("lib/legacy/l.txt", "lib/l.txt"),
                ("lib/n.txt", "lib_new/n.txt"),
                ("other.txt", "other.txt"),
            ],
            &[],
        )
        .await;
    })
    .await;
}

/// The committed `d/x.txt`, `f`, `g` and `other.txt`, with `d` moved with `lore dirty move` to
/// `e`, `f` into it, and `g` onto `f`, then `front` moved to the front of the root's children, so
/// the commit meets it first. Answers what the revision holds.
async fn moved_into_a_renamed_directory(
    instance: &TestRepository,
    front: &str,
) -> BTreeMap<String, NodeID> {
    let first = committed(instance, &["d/x.txt", "f", "g", "other.txt"]).await;
    let held = tree_paths(instance, &first).await;
    dirty_moved(instance, "d", "e").await;
    dirty_moved(instance, "f", "e/f").await;
    dirty_moved(instance, "g", "f").await;
    to_front(instance, front).await;
    restaged(instance, "other.txt").await;
    held
}

/// The staged tree [`moved_into_a_renamed_directory`] holds after the commit, each path with its
/// committed one.
const MOVED_INTO_A_RENAMED_DIRECTORY: [(&str, &str); 5] = [
    ("e", "d"),
    ("e/f", "f"),
    ("e/x.txt", "d/x.txt"),
    ("f", "g"),
    ("other.txt", "other.txt"),
];

/// A file `lore dirty move` moved into a renamed directory, and a second file moved onto the first
/// one's name, none staged, stay where the working tree holds them after the commit, which meets
/// the directory before the second file.
#[tokio::test]
async fn a_commit_keeps_a_file_dirty_moved_into_a_renamed_directory_met_first() {
    with_instance(|instance| async move {
        let held = moved_into_a_renamed_directory(&instance, "e").await;

        let (revision, warnings) = committed_warning(&instance, "Edited").await;
        assert_revision_holds(&instance, &revision, &held).await;
        assert_eq!(warnings, 0);
        assert_staged(
            &instance,
            &held,
            &MOVED_INTO_A_RENAMED_DIRECTORY,
            &["front"],
        )
        .await;
    })
    .await;
}

/// The moves of [`a_commit_keeps_a_file_dirty_moved_into_a_renamed_directory_met_first`], which the
/// commit meets the second file first of.
#[tokio::test]
async fn a_commit_keeps_a_file_dirty_moved_into_a_renamed_directory_met_last() {
    with_instance(|instance| async move {
        let held = moved_into_a_renamed_directory(&instance, "f").await;

        let (revision, warnings) = committed_warning(&instance, "Edited").await;
        assert_revision_holds(&instance, &revision, &held).await;
        assert_eq!(warnings, 0);
        assert_staged(
            &instance,
            &held,
            &MOVED_INTO_A_RENAMED_DIRECTORY,
            &["front"],
        )
        .await;
    })
    .await;
}

/// A committed directory moved with `lore dirty move`, a new directory marked dirty as added and
/// moved to its committed path, and a committed file moved into that one, none staged: after the
/// commit, the staged tree holds the file in the new directory, not in the moved one.
#[tokio::test]
async fn a_commit_keeps_a_file_dirty_moved_into_a_new_directory_at_a_moved_ones_path() {
    with_instance(|instance| async move {
        let first = committed(
            &instance,
            &["assets/textures/wood.png", "logo.png", "other.txt"],
        )
        .await;
        let held = tree_paths(&instance, &first).await;
        dirty_moved(&instance, "assets/textures", "old_textures").await;
        made_directory(&instance, "new");
        dirtied(&instance, "new").await;
        dirty_moved(&instance, "new", "assets/textures").await;
        dirty_moved(&instance, "logo.png", "assets/textures/logo.png").await;
        restaged(&instance, "other.txt").await;

        let (revision, warnings) = committed_warning(&instance, "Edited").await;
        assert_revision_holds(&instance, &revision, &held).await;
        assert_eq!(warnings, 0);
        assert_staged(
            &instance,
            &held,
            &[
                ("assets", "assets"),
                ("assets/textures/logo.png", "logo.png"),
                ("old_textures", "assets/textures"),
                ("old_textures/wood.png", "assets/textures/wood.png"),
                ("other.txt", "other.txt"),
            ],
            &["assets/textures"],
        )
        .await;
    })
    .await;
}

/// Two directories `lore dirty move` swapped through a third name, a file moved into the one now
/// at the second name, and another file moved onto the first file's name, none staged, met by the
/// commit from the directory at the first name on: after the commit, the staged tree holds each
/// where the working tree does, the file inside the directory that took the second name.
#[tokio::test]
async fn a_commit_keeps_a_file_dirty_moved_into_a_swapped_directory() {
    with_instance(|instance| async move {
        let first = committed(
            &instance,
            &["a/ka.txt", "b/kb.txt", "w.txt", "z.txt", "other.txt"],
        )
        .await;
        let held = tree_paths(&instance, &first).await;
        dirty_moved(&instance, "a", "t").await;
        dirty_moved(&instance, "b", "a").await;
        dirty_moved(&instance, "t", "b").await;
        dirty_moved(&instance, "z.txt", "b/z.txt").await;
        dirty_moved(&instance, "w.txt", "z.txt").await;
        for front in ["z.txt", "b", "a"] {
            to_front(&instance, front).await;
        }
        restaged(&instance, "other.txt").await;

        let (revision, warnings) = committed_warning(&instance, "Edited").await;
        assert_revision_holds(&instance, &revision, &held).await;
        assert_eq!(warnings, 0);
        assert_staged(
            &instance,
            &held,
            &[
                ("a", "b"),
                ("a/kb.txt", "b/kb.txt"),
                ("b", "a"),
                ("b/ka.txt", "a/ka.txt"),
                ("b/z.txt", "z.txt"),
                ("other.txt", "other.txt"),
                ("z.txt", "w.txt"),
            ],
            &["front"],
        )
        .await;
    })
    .await;
}

/// A commit whose marking of its dirty changes again fails once its revision is stored completes:
/// the revision is current, the commit warns once, and no staged tree is left.
#[tokio::test]
async fn a_commit_completes_where_marking_its_dirty_changes_again_fails() {
    with_instance(|instance| async move {
        committed(&instance, &["other.txt"]).await;
        written(&instance, &["new.txt"]);
        dirtied(&instance, "new.txt").await;
        restaged(&instance, "other.txt").await;
        let committing = failing_context(&instance, &["new.txt"]);

        let (outcome, warnings) = warnings_of(lore_revision::commit::commit_boxed(
            committing,
            &instance.write_token,
            CommitOptions::new("Edited".to_string()),
        ))
        .await;
        let signature = outcome.expect("the commit completes");
        let (current, staged, _) =
            State::deserialize_current_and_staged(instance.repository.clone())
                .await
                .expect("the states");
        assert_eq!(current.revision(), signature);
        assert!(staged.is_none(), "a staged tree is left");
        assert_eq!(warnings, 1);
    })
    .await;
}

/// A directory `lore dirty move` moved out of its committed parent, into which a second dirty move
/// put that parent once a file in it was staged, is committed where the staged tree holds it, the
/// parent and the staged file with it: a directory holding a staged node is staged with it.
#[tokio::test]
async fn a_commit_takes_a_dirty_moved_directory_holding_its_staged_parent() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["p/d/k.txt", "p/f.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        dirty_moved(&instance, "p/d", "d").await;
        restaged(&instance, "p/f.txt").await;
        dirty_moved(&instance, "p", "d/p").await;

        let (revision, warnings) = committed_warning(&instance, "Edited").await;
        assert_revision_of(
            &instance,
            &revision,
            &held,
            &[
                ("d", "p/d"),
                ("d/k.txt", "p/d/k.txt"),
                ("d/p", "p"),
                ("d/p/f.txt", "p/f.txt"),
            ],
            &[],
        )
        .await;
        assert_eq!(warnings, 0);
        assert_no_change_marked(&instance).await;
    })
    .await;
}

/// The committed `d/k.txt` and `x.txt`, with `d` moved with `lore dirty move` to `e`, an edit of
/// `x.txt` staged, and `x.txt` moved with `lore dirty move` into `e`. Answers what the revision
/// holds.
async fn staged_into_a_renamed_directory(instance: &TestRepository) -> BTreeMap<String, NodeID> {
    let first = committed(instance, &["d/k.txt", "x.txt"]).await;
    let held = tree_paths(instance, &first).await;
    dirty_moved(instance, "d", "e").await;
    restaged(instance, "x.txt").await;
    dirty_moved(instance, "x.txt", "e/x.txt").await;
    held
}

/// A staged file moved with `lore dirty move` into a directory a dirty move renamed is committed
/// where the staged tree holds it, the directory with it.
#[tokio::test]
async fn a_commit_takes_a_staged_file_dirty_moved_into_a_renamed_directory() {
    with_instance(|instance| async move {
        let held = staged_into_a_renamed_directory(&instance).await;

        let (revision, warnings) = committed_warning(&instance, "Edited").await;
        assert_revision_of(
            &instance,
            &revision,
            &held,
            &[("e", "d"), ("e/k.txt", "d/k.txt"), ("e/x.txt", "x.txt")],
            &[],
        )
        .await;
        assert_eq!(warnings, 0);
        assert_no_change_marked(&instance).await;
    })
    .await;
}

/// The moves of [`a_commit_takes_a_staged_file_dirty_moved_into_a_renamed_directory`], with a new
/// file staged at the directory's committed path: the directory, holding the staged file, stays
/// where the staged tree holds it, beside the new file, rather than leaving the revision with it.
#[tokio::test]
async fn a_commit_takes_a_staged_file_dirty_moved_into_a_renamed_directory_whose_path_is_taken() {
    with_instance(|instance| async move {
        let held = staged_into_a_renamed_directory(&instance).await;
        written(&instance, &["d"]);
        let state = stage_path(&instance, "d").await;
        let replacement = node_id_at(&instance, &state, "d").await;

        let (revision, warnings) = committed_warning(&instance, "Edited").await;
        assert_revision_of(
            &instance,
            &revision,
            &held,
            &[("e", "d"), ("e/k.txt", "d/k.txt"), ("e/x.txt", "x.txt")],
            &[("d", replacement)],
        )
        .await;
        assert_eq!(warnings, 0);
        assert_no_change_marked(&instance).await;
    })
    .await;
}

/// A staged file moved with `lore dirty move` into a new directory only marked dirty as added is
/// committed there, the directory with it, rather than discarded with the directory.
#[tokio::test]
async fn a_commit_takes_a_staged_file_dirty_moved_into_a_directory_only_marked_added() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["x.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        restaged(&instance, "x.txt").await;
        made_directory(&instance, "newdir");
        let state = dirtied(&instance, "newdir").await;
        let newdir = node_id_at(&instance, &state, "newdir").await;
        dirty_moved(&instance, "x.txt", "newdir/x.txt").await;

        let (revision, warnings) = committed_warning(&instance, "Edited").await;
        assert_revision_of(
            &instance,
            &revision,
            &held,
            &[("newdir/x.txt", "x.txt"), ("other.txt", "other.txt")],
            &[("newdir", newdir)],
        )
        .await;
        assert_eq!(warnings, 0);
        assert_nothing_dirty(&instance).await;
    })
    .await;
}

/// Marking moves again skips each move whose path ends below a file, runs through one, or ends
/// inside the moved directory's own subtree: it marks none of them, gives each node its committed
/// name again, adds nothing below the file, and warns once.
#[tokio::test]
async fn marking_moves_again_skips_paths_no_directory_takes() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["a/x.txt", "f.txt", "y.txt", "z.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        let staged = State::deserialize(instance.repository.clone(), first.revision())
            .await
            .expect("a staged tree of the revision");
        let a = held["a"];
        let moves = [
            MovedNode {
                node: held["y.txt"],
                node_type: LoreNodeType::File,
                path: relative_path("f.txt/y.txt"),
            },
            MovedNode {
                node: held["z.txt"],
                node_type: LoreNodeType::File,
                path: relative_path("f.txt/sub/z.txt"),
            },
            MovedNode {
                node: a,
                node_type: LoreNodeType::Directory,
                path: relative_path(&format!(".lore-returning-{a}/sub/a")),
            },
        ];

        let (skipped, warnings) = warnings_of(mark_moved_again(
            &instance.repository,
            &first,
            &staged,
            &moves,
        ))
        .await;
        assert_eq!(skipped.expect("the marking completes").len(), moves.len());
        assert_eq!(warnings, 1);
        for path in ["a", "y.txt", "z.txt"] {
            assert_eq!(
                node_id_at(&instance, &staged, path).await,
                held[path],
                "{path}"
            );
        }
        let file = node_of(&instance, &staged, held["f.txt"]).await;
        assert!(
            !file.child.is_valid_node_id(),
            "a node was added below the file"
        );
        let below_a = staged
            .node_children(instance.repository.clone(), a)
            .await
            .expect("a's children");
        assert_eq!(below_a, [held["a/x.txt"]], "a node was added below 'a'");
    })
    .await;
}

/// A commit whose marking of a merge's dirty changes fails once its revision is stored completes:
/// the revision is current, the commit warns once, and the merge's carry is cleared.
#[tokio::test]
async fn a_commit_completes_where_marking_a_merges_dirty_changes_fails() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["other.txt"]).await;
        restaged(&instance, "other.txt").await;
        lore_revision::merge_carry::store(
            instance.repository.clone(),
            first.revision(),
            Hash::default(),
            &[relative_path("new.txt")],
        )
        .await
        .expect("the carry is stored");
        let committing = failing_context(&instance, &["new.txt"]);

        let (outcome, warnings) = warnings_of(lore_revision::commit::commit_boxed(
            committing,
            &instance.write_token,
            CommitOptions::new("Edited".to_string()),
        ))
        .await;
        let signature = outcome.expect("the commit completes");
        let (current, _, _) = State::deserialize_current_and_staged(instance.repository.clone())
            .await
            .expect("the states");
        assert_eq!(current.revision(), signature);
        assert_eq!(warnings, 1);
        let carry = lore_revision::merge_carry::load(instance.repository.clone())
            .await
            .expect("the carry reads");
        assert!(carry.is_none(), "the carry is left");
    })
    .await;
}

/// The moves of [`a_commit_keeps_a_file_dirty_moved_into_a_new_directory_at_a_moved_ones_path`],
/// with the moved file's edit staged first, then `front` moved to the front of the root's
/// children. Answers what the revision holds and the new directory's node.
async fn staged_into_a_new_directory_at_a_moved_ones_path(
    instance: &TestRepository,
    front: &str,
) -> (BTreeMap<String, NodeID>, NodeID) {
    let first = committed(
        instance,
        &["assets/textures/wood.png", "logo.png", "other.txt"],
    )
    .await;
    let held = tree_paths(instance, &first).await;
    restaged(instance, "logo.png").await;
    dirty_moved(instance, "assets/textures", "old_textures").await;
    made_directory(instance, "new");
    let state = dirtied(instance, "new").await;
    let new = node_id_at(instance, &state, "new").await;
    dirty_moved(instance, "new", "assets/textures").await;
    dirty_moved(instance, "logo.png", "assets/textures/logo.png").await;
    to_front(instance, front).await;
    (held, new)
}

/// A new directory marked dirty as added, moved to a committed directory's path that a dirty move
/// left, and holding a staged file moved into it, is staged with the file and takes that path, so
/// the committed directory, which cannot go back, leaves the revision with what it holds, whichever
/// of the two the commit meets first. The commit warns once.
#[tokio::test]
async fn a_commit_drops_a_dirty_moved_directory_whose_path_a_directory_holding_a_staged_file_takes()
{
    for front in ["old_textures", "assets"] {
        with_instance(|instance| async move {
            let (held, new) =
                staged_into_a_new_directory_at_a_moved_ones_path(&instance, front).await;

            let (revision, warnings) = committed_warning(&instance, "Edited").await;
            assert_revision_of(
                &instance,
                &revision,
                &held,
                &[
                    ("assets", "assets"),
                    ("assets/textures/logo.png", "logo.png"),
                    ("other.txt", "other.txt"),
                ],
                &[("assets/textures", new)],
            )
            .await;
            assert_eq!(warnings, 1, "{front} first");
        })
        .await;
    }
}

/// Marks `through`, paths below `f`, and an edit of `other.txt` on a revision that holds no `f`,
/// and carries them onto one holding the file `f`. Answers the rebased tree, the warnings the
/// rebase logged, and the node of `f`.
async fn rebased_through_a_file(
    instance: &TestRepository,
    through: &[&str],
) -> (Arc<State>, Vec<String>, NodeID) {
    let first = committed(instance, &["other.txt"]).await;
    written(instance, &["f"]);
    stage_path(instance, "f").await;
    let second = test_commit(instance, "File").await;
    let file = node_id_at(instance, &second, "f").await;
    std::fs::remove_file(instance.path.join("f")).expect("the delete on disk");
    written(instance, through);
    std::fs::write(instance.path.join("other.txt"), b"edited").expect("the edit on disk");
    let current = State::deserialize(instance.repository.clone(), first.revision())
        .await
        .expect("the first revision");
    let staged = State::deserialize(instance.repository.clone(), first.revision())
        .await
        .expect("a staged tree of it");
    let mut paths: Vec<_> = through.iter().map(|path| relative_path(path)).collect();
    paths.push(relative_path("other.txt"));
    let marked = Box::pin(lore_revision::file::dirty::dirty_relative_paths_in(
        instance.repository.clone(),
        current,
        staged,
        paths,
    ))
    .await
    .expect("the marks on the first revision");

    let (rebased, warnings) = warned(Box::pin(lore_revision::state::rebase_staged_state(
        instance.repository.clone(),
        marked,
        second.revision(),
        false,
    )))
    .await;
    let rebased = rebased
        .expect("the rebase completes")
        .expect("a mark carries over");
    let state = State::deserialize(instance.repository.clone(), rebased)
        .await
        .expect("the rebased tree");
    let other = node_at(instance, &state, "other.txt").await;
    assert!(other.is_dirty(), "flags {:#x}", other.flags);
    assert!(
        !node_of(instance, &state, file)
            .await
            .child
            .is_valid_node_id(),
        "a node was added below the file"
    );
    (state, warnings, file)
}

/// A staged tree's dirty marks carried onto a revision holding a file where a marked path runs
/// through a directory: that path is left unmarked, with one warning naming it and the file, and
/// the other mark carries over.
#[tokio::test]
async fn a_rebase_leaves_a_path_through_a_file_unmarked_and_carries_the_rest() {
    with_instance(|instance| async move {
        let (_, warnings, _) = rebased_through_a_file(&instance, &["f/sub/x"]).await;
        assert_eq!(
            warnings,
            ["Cannot mark f/sub/x dirty: f is not a directory"],
            "the warnings"
        );
    })
    .await;
}

/// Dirty marks of two paths through a file, carried onto a revision holding that file, leave both
/// unmarked, with one warning naming the first and counting the other.
#[tokio::test]
async fn a_rebase_warns_once_of_the_paths_through_a_file_it_leaves_unmarked() {
    with_instance(|instance| async move {
        let (_, warnings, _) = rebased_through_a_file(&instance, &["f/sub/x", "f/y"]).await;
        assert_eq!(
            warnings,
            [
                "Cannot mark f/y dirty: f is not a directory; 1 more path through a node that \
                 is not a directory is not marked"
            ],
            "the warnings"
        );
    })
    .await;
}

/// A file `lore dirty move` moved out of a committed directory, then staged where it went, leaves
/// that directory in the revision without it, its address rehashed, where a second dirty move took
/// the directory away and the commit returns it.
#[tokio::test]
async fn a_commit_rehashes_the_directory_a_staged_file_dirty_moved_out_of() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["a/s/g1", "c/k1", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        dirty_moved(&instance, "a/s/g1", "c/h1").await;
        restaged(&instance, "c/h1").await;
        dirty_moved(&instance, "a/s", "c/b").await;

        let (revision, warnings) = committed_warning(&instance, "Edited").await;
        assert_revision_of(
            &instance,
            &revision,
            &held,
            &[
                ("a", "a"),
                ("a/s", "a/s"),
                ("c", "c"),
                ("c/h1", "a/s/g1"),
                ("c/k1", "c/k1"),
                ("other.txt", "other.txt"),
            ],
            &[],
        )
        .await;
        assert_eq!(warnings, 0);
    })
    .await;
}

/// A directory `lore dirty move` renamed, which a commit takes for an edit staged in it, leaves no
/// file in it marked modified: each file not edited since the move carries its move mark alone.
#[tokio::test]
async fn a_commit_marks_nothing_modified_where_it_takes_a_dirty_moved_directory_for_an_edit() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["d/a.txt", "d/b.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        dirty_moved(&instance, "d", "e").await;
        restaged(&instance, "e/a.txt").await;

        let (revision, warnings) = committed_warning(&instance, "Edited").await;
        assert_revision_of(
            &instance,
            &revision,
            &held,
            &[("e", "d"), ("e/a.txt", "d/a.txt"), ("e/b.txt", "d/b.txt")],
            &[],
        )
        .await;
        assert_eq!(warnings, 0);
        assert_no_change_marked(&instance).await;
    })
    .await;
}

/// An empty directory `lore dirty move` renamed keeps its mark once a file added in it is removed
/// and its path marked dirty again, so a commit of another staged file leaves the move out, and the
/// staged tree holds it moved past the commit.
#[tokio::test]
async fn a_commit_leaves_out_a_dirty_move_whose_added_file_was_removed() {
    with_instance(|instance| async move {
        made_directory(&instance, "d");
        let first = committed(&instance, &["other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        assert!(held.contains_key("d"), "the empty directory is committed");
        dirty_moved(&instance, "d", "e").await;
        written(&instance, &["e/new.txt"]);
        dirtied(&instance, "e/new.txt").await;
        std::fs::remove_file(instance.path.join("e/new.txt")).expect("the delete on disk");
        dirtied(&instance, "e/new.txt").await;
        restaged(&instance, "other.txt").await;

        let (revision, warnings) = committed_warning(&instance, "Edited").await;
        assert_revision_holds(&instance, &revision, &held).await;
        assert_eq!(warnings, 0);
        assert_staged(
            &instance,
            &held,
            &[("e", "d"), ("other.txt", "other.txt")],
            &[],
        )
        .await;
    })
    .await;
}

/// A file renamed with `lore dirty move` inside a directory a dirty move renamed stays marked moved
/// past a commit that takes the directory, for a staged file moved into it, and leaves the rename
/// out: marking the directory again does not take the file's committed name, which the working
/// tree lacks, for a deletion.
#[tokio::test]
async fn a_commit_keeps_a_dirty_rename_inside_a_directory_it_takes() {
    with_instance(|instance| async move {
        let held = staged_into_a_renamed_directory(&instance).await;
        dirty_moved(&instance, "e/k.txt", "e/q.txt").await;

        let (revision, warnings) = committed_warning(&instance, "Edited").await;
        assert_revision_of(
            &instance,
            &revision,
            &held,
            &[("e", "d"), ("e/k.txt", "d/k.txt"), ("e/x.txt", "x.txt")],
            &[],
        )
        .await;
        assert_eq!(warnings, 0);
        assert_staged(
            &instance,
            &tree_paths(&instance, &revision).await,
            &[("e", "e"), ("e/q.txt", "e/k.txt"), ("e/x.txt", "e/x.txt")],
            &[],
        )
        .await;
    })
    .await;
}

/// A file renamed with `lore dirty move` to its own name in another case, edited and marked dirty
/// again, stays a move marked dirty: a commit of another staged file leaves the rename out, and the
/// staged tree holds it renamed past the commit.
#[tokio::test]
async fn a_commit_leaves_out_a_dirty_case_rename_marked_dirty_again() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["r1", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        dirty_moved(&instance, "r1", "R1").await;
        std::fs::write(instance.path.join("R1"), b"edited").expect("the edit on disk");
        dirtied(&instance, "R1").await;
        restaged(&instance, "other.txt").await;

        let (revision, warnings) = committed_warning(&instance, "Edited").await;
        assert_revision_holds(&instance, &revision, &held).await;
        assert_eq!(warnings, 0);
        assert_staged(
            &instance,
            &held,
            &[("R1", "r1"), ("other.txt", "other.txt")],
            &[],
        )
        .await;
    })
    .await;
}

/// A stage of an edit inside a directory `lore dirty move` renamed stages the directory's move, so
/// the commit takes the directory with the staged edit alone: the files moved with it carry no
/// mark past it, an edit marked dirty stays marked modified, and a rename inside stays a dirty
/// move.
#[tokio::test]
async fn a_commit_takes_the_move_a_stage_inside_a_dirty_moved_directory_stages() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["d/a.txt", "d/b.txt", "d/c.txt", "d/k.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        dirty_moved(&instance, "d", "e").await;
        std::fs::write(instance.path.join("e/b.txt"), b"edited in place").expect("the edit");
        dirtied(&instance, "e/b.txt").await;
        dirty_moved(&instance, "e/k.txt", "e/q.txt").await;
        restaged(&instance, "e/a.txt").await;

        let (revision, warnings) = committed_warning(&instance, "Edited").await;
        assert_revision_of(
            &instance,
            &revision,
            &held,
            &[
                ("e", "d"),
                ("e/a.txt", "d/a.txt"),
                ("e/b.txt", "d/b.txt"),
                ("e/c.txt", "d/c.txt"),
                ("e/k.txt", "d/k.txt"),
            ],
            &[],
        )
        .await;
        let b = node_at(&instance, &revision, "e/b.txt").await;
        assert_eq!(
            b.address,
            node_at(&instance, &first, "d/b.txt").await.address
        );
        assert_eq!(warnings, 0);
        let staged = staged_tree(&instance).await;
        let edited = node_at(&instance, &staged, "e/b.txt").await;
        assert!(
            edited.is_dirty_modify() && !edited.is_dirty_move() && !edited.is_staged(),
            "e/b.txt: flags {:#x}",
            edited.flags
        );
        for path in ["e", "e/a.txt", "e/c.txt"] {
            let node = node_at(&instance, &staged, path).await;
            assert!(
                !node.is_dirty_move() && !node.is_dirty_modify(),
                "{path}: flags {:#x}",
                node.flags
            );
        }
        let renamed = node_at(&instance, &staged, "e/q.txt").await;
        assert!(
            renamed.is_dirty_move() && !renamed.is_staged(),
            "e/q.txt: flags {:#x}",
            renamed.flags
        );
    })
    .await;
}
