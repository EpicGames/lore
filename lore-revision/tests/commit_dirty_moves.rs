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
use lore_revision::file::dirty::SEARCHED_CHILDREN;
use lore_revision::file::dirty::mark_moved_again;
use lore_revision::fs::filesystem_provider::FilesystemProvider;
use lore_revision::fs::filesystem_provider::FsError;
use lore_revision::fs::filesystem_provider::InstanceOperationImpl;
use lore_revision::fs::filesystem_provider::StaticDispatchInstanceOperation;
use lore_revision::fs::filesystem_provider::test_util::TestOperation;
use lore_revision::fs::os::OsFilesystem;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreEventCallback;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreNodeType;
use lore_revision::interface::LoreString;
use lore_revision::node::NodeID;
use lore_revision::node::NodeIDExt;
use lore_revision::node::ROOT_NODE;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use parking_lot::Mutex;

use crate::dirty_move_fixture::assert_addresses;
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
use crate::dirty_move_fixture::staged_moved;
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

/// Removes `front`, the directory [`to_front`] moves nodes through, and marks it dirty, so it leaves
/// the staged tree: a directory a staged change passed through stays staged, and the commit would
/// take it.
async fn without_front(instance: &TestRepository) {
    if instance.path.join("front").exists() {
        removed_directory(instance, "front");
        dirtied(instance, "front").await;
    }
}

/// Moves the node at `path` to the front of its parent's children ([`to_front`]) and removes
/// `front` again ([`without_front`]): a chain order the commit's, which follows the working tree's
/// listing, does not set alike on every file system.
async fn put_first(instance: &TestRepository, path: &str) {
    to_front(instance, path).await;
    without_front(instance).await;
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
    without_front(instance).await;
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

/// A file renamed with `lore dirty move` inside a directory a dirty move renamed is committed
/// renamed once a staged file moved into the directory staged the directory's whole move, as where
/// a stage inside the directory staged it: the file's move is staged with the directory's.
#[tokio::test]
async fn a_commit_takes_a_dirty_rename_inside_a_directory_whose_move_is_staged_whole() {
    with_instance(|instance| async move {
        let held = staged_into_a_renamed_directory(&instance).await;
        dirty_moved(&instance, "e/k.txt", "e/q.txt").await;

        let (revision, warnings) = committed_warning(&instance, "Edited").await;
        assert_revision_of(
            &instance,
            &revision,
            &held,
            &[("e", "d"), ("e/q.txt", "d/k.txt"), ("e/x.txt", "x.txt")],
            &[],
        )
        .await;
        assert_eq!(warnings, 0);
        assert_no_change_marked(&instance).await;
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

/// `held` without `removed` and the paths below it.
fn without(held: &BTreeMap<String, NodeID>, removed: &str) -> BTreeMap<String, NodeID> {
    let below = format!("{removed}/");
    held.iter()
        .filter(|(path, _)| path.as_str() != removed && !path.starts_with(&below))
        .map(|(path, node)| (path.clone(), *node))
        .collect()
}

/// Asserts that `node`, which the instance's staged tree holds at `path`, hangs from a directory.
async fn assert_below_a_directory(instance: &TestRepository, state: &State, path: &str) {
    let node = node_at(instance, state, path).await;
    let parent = node_of(instance, state, node.parent).await;
    assert!(
        parent.is_directory(),
        "{path} hangs from a node of flags {:#x}",
        parent.flags
    );
}

/// A directory removed on disk and marked dirty, below which the deletion of a directory is
/// staged, is not staged for deletion by the mark, and is deleted by the commit with everything
/// below it, as the working tree no longer holds it: nothing of it stays marked.
#[tokio::test]
async fn a_commit_deletes_a_directory_marked_deleted_above_a_staged_deletion() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["a/f1", "a/s/g1", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        removed_directory(&instance, "a/s");
        stage_path(&instance, "a/s").await;
        removed_directory(&instance, "a");
        let state = dirtied(&instance, "a").await;
        let a = node_at(&instance, &state, "a").await;
        assert!(!a.is_staged_delete(), "a: flags {:#x}", a.flags);

        let (revision, _) = committed_warning(&instance, "Deleted").await;
        assert_revision_holds(&instance, &revision, &without(&held, "a")).await;
        assert_nothing_dirty(&instance).await;
    })
    .await;
}

/// A file marked dirty as added in a committed directory, which is then removed on disk and marked
/// dirty, leaves the staged tree: a commit of another staged file stores the committed tree with
/// that edit alone, each directory holding the address its children hash to.
#[tokio::test]
async fn a_commit_leaves_out_an_added_file_in_a_directory_marked_deleted() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["d/keep.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        written(&instance, &["d/new.txt"]);
        dirtied(&instance, "d/new.txt").await;
        removed_directory(&instance, "d");
        dirtied(&instance, "d").await;
        restaged(&instance, "other.txt").await;

        let (revision, _) = committed_warning(&instance, "Edited").await;
        assert_revision_holds(&instance, &revision, &held).await;
    })
    .await;
}

/// A committed file `lore dirty move` moved into a committed directory, which is then removed on
/// disk and marked dirty, is committed at its committed path, as neither the move nor the
/// deletion was staged.
#[tokio::test]
async fn a_commit_returns_a_file_moved_into_a_directory_marked_deleted() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["d/keep.txt", "x.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        dirty_moved(&instance, "x.txt", "d/x.txt").await;
        removed_directory(&instance, "d");
        dirtied(&instance, "d").await;
        restaged(&instance, "other.txt").await;

        let (revision, _) = committed_warning(&instance, "Edited").await;
        assert_revision_holds(&instance, &revision, &held).await;
    })
    .await;
}

/// A committed file `lore dirty move` moved into a directory marked dirty as added, which is then
/// removed on disk and marked dirty, is committed at its committed path: the directory leaves the
/// staged tree, and the committed file does not leave with it.
#[tokio::test]
async fn a_commit_returns_a_file_moved_into_an_added_directory_removed_again() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["x.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        made_directory(&instance, "n");
        dirtied(&instance, "n").await;
        dirty_moved(&instance, "x.txt", "n/x.txt").await;
        removed_directory(&instance, "n");
        dirtied(&instance, "n").await;
        restaged(&instance, "other.txt").await;

        let (revision, _) = committed_warning(&instance, "Edited").await;
        assert_revision_holds(&instance, &revision, &held).await;
    })
    .await;
}

/// A file whose deletion is marked dirty keeps that mark once `lore dirty move` moves its
/// directory, past a commit of another staged file that leaves the move out.
#[tokio::test]
async fn a_commit_keeps_the_deletion_marked_in_a_directory_dirty_moved_after_it() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["d/f.txt", "d/g.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        std::fs::remove_file(instance.path.join("d/f.txt")).expect("the delete on disk");
        dirtied(&instance, "d/f.txt").await;
        dirty_moved(&instance, "d", "e").await;
        restaged(&instance, "other.txt").await;

        let (revision, _) = committed_warning(&instance, "Edited").await;
        assert_revision_holds(&instance, &revision, &held).await;
        let staged = staged_tree(&instance).await;
        let f = node_of(&instance, &staged, held["d/f.txt"]).await;
        assert!(
            f.is_dirty_delete() && !f.is_staged(),
            "d/f.txt: flags {:#x}",
            f.flags
        );
    })
    .await;
}

/// A committed file `lore dirty move` moved below a new directory at the name of a file staged for
/// deletion, in another case, hangs from that directory, so the commit deletes the staged file
/// alone and keeps the moved file at its committed path, and the staged tree holds it moved past
/// the commit.
#[tokio::test]
async fn a_commit_keeps_a_file_dirty_moved_below_the_name_of_a_file_staged_for_deletion() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["a/f1", "a/f2", "c", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        std::fs::remove_file(instance.path.join("a/f1")).expect("the delete on disk");
        stage_path(&instance, "a/f1").await;
        made_directory(&instance, "a/F1");
        let state = dirty_moved(&instance, "c", "a/F1/n").await;
        assert_below_a_directory(&instance, &state, "a/F1/n").await;

        let (revision, _) = committed_warning(&instance, "Deleted").await;
        assert_revision_holds(&instance, &revision, &without(&held, "a/f1")).await;
        let staged = staged_tree(&instance).await;
        assert_below_a_directory(&instance, &staged, "a/F1/n").await;
        let moved = node_at(&instance, &staged, "a/F1/n").await;
        assert!(
            moved.is_dirty_move() && !moved.is_staged(),
            "a/F1/n: flags {:#x}",
            moved.flags
        );
    })
    .await;
}

/// A new directory marked dirty at the name of a file staged for deletion, in its case and in
/// another, is a directory of its own: the file the working tree holds in it hangs from it, the
/// commit deletes the staged file alone, and the staged tree holds the directory and its file
/// marked added past the commit.
#[tokio::test]
async fn a_commit_keeps_a_directory_marked_added_at_the_name_of_a_file_staged_for_deletion() {
    for name in ["f1", "F1"] {
        with_instance(|instance| async move {
            let first = committed(&instance, &["a/f1", "a/f2", "other.txt"]).await;
            let held = tree_paths(&instance, &first).await;
            std::fs::remove_file(instance.path.join("a/f1")).expect("the delete on disk");
            stage_path(&instance, "a/f1").await;
            let file = format!("a/{name}/n");
            written(&instance, &[file.as_str()]);
            let state = dirtied(&instance, &format!("a/{name}")).await;
            assert_below_a_directory(&instance, &state, &file).await;

            let (revision, _) = committed_warning(&instance, "Deleted").await;
            assert_revision_holds(&instance, &revision, &without(&held, "a/f1")).await;
            let staged = staged_tree(&instance).await;
            for path in [format!("a/{name}"), file.clone()] {
                let added = node_at(&instance, &staged, &path).await;
                assert!(
                    added.is_dirty_add() && !added.is_staged(),
                    "{name}: {path}: flags {:#x}",
                    added.flags
                );
            }
            assert!(
                node_at(&instance, &staged, &format!("a/{name}"))
                    .await
                    .is_directory()
            );
        })
        .await;
    }
}

/// A new file marked dirty at the name of a directory staged for deletion, in its case and in
/// another, leaves the staged deletion as it is: the commit deletes the directory, and the staged
/// tree holds the file marked added past the commit.
#[tokio::test]
async fn a_commit_keeps_a_file_marked_added_at_the_name_of_a_directory_staged_for_deletion() {
    for name in ["s", "S"] {
        with_instance(|instance| async move {
            let first = committed(&instance, &["a/s/g1", "a/f2", "other.txt"]).await;
            let held = tree_paths(&instance, &first).await;
            removed_directory(&instance, "a/s");
            stage_path(&instance, "a/s").await;
            let file = format!("a/{name}");
            written(&instance, &[file.as_str()]);
            let state = dirtied(&instance, &file).await;
            let directory = node_of(&instance, &state, held["a/s"]).await;
            assert!(
                directory.is_staged_delete(),
                "{name}: a/s: flags {:#x}",
                directory.flags
            );

            let (revision, _) = committed_warning(&instance, "Deleted").await;
            assert_revision_holds(&instance, &revision, &without(&held, "a/s")).await;
            let staged = staged_tree(&instance).await;
            let added = node_at(&instance, &staged, &file).await;
            assert!(
                added.is_file() && added.is_dirty_add() && !added.is_staged(),
                "{name}: {file}: flags {:#x}",
                added.flags
            );
        })
        .await;
    }
}

/// A directory a staged file was moved out of with `lore dirty move`, and which `lore dirty move`
/// then moved, holds no staged change, so a commit takes the staged file where the staged tree
/// holds it and leaves the directory's move out.
#[tokio::test]
async fn a_commit_leaves_out_the_move_of_a_directory_a_staged_file_was_moved_out_of() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["p/f.txt", "p/g.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        restaged(&instance, "p/f.txt").await;
        dirty_moved(&instance, "p/f.txt", "f.txt").await;
        dirty_moved(&instance, "p", "q").await;

        let (revision, _) = committed_warning(&instance, "Edited").await;
        assert_revision_of(
            &instance,
            &revision,
            &held,
            &[
                ("f.txt", "p/f.txt"),
                ("other.txt", "other.txt"),
                ("p", "p"),
                ("p/g.txt", "p/g.txt"),
            ],
            &[],
        )
        .await;
    })
    .await;
}

/// A directory `lore dirty move` moved after a file in it was staged as added takes its whole move
/// staged, as a stage inside it would, the file that moves with it alone a staged move too, and
/// stays a staged move once the added file is removed and its path marked dirty: the commit takes
/// the moved directory, and nothing stays marked past it.
#[tokio::test]
async fn a_dirty_moved_directory_stays_a_staged_move_once_the_staged_add_in_it_leaves() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["old/a.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        written(&instance, &["old/temp"]);
        stage_path(&instance, "old/temp").await;
        let state = dirty_moved(&instance, "old", "new").await;
        let carried = node_at(&instance, &state, "new/a.txt").await;
        assert!(
            carried.is_staged_move(),
            "new/a.txt: flags {:#x}",
            carried.flags
        );
        std::fs::remove_file(instance.path.join("new/temp")).expect("the delete on disk");
        let state = dirtied(&instance, "new/temp").await;
        let moved = node_at(&instance, &state, "new").await;
        assert!(moved.is_staged_move(), "new: flags {:#x}", moved.flags);
        restaged(&instance, "other.txt").await;

        let (revision, _) = committed_warning(&instance, "Edited").await;
        assert_revision_of(
            &instance,
            &revision,
            &held,
            &[
                ("new", "old"),
                ("new/a.txt", "old/a.txt"),
                ("other.txt", "other.txt"),
            ],
            &[],
        )
        .await;
        assert_nothing_dirty(&instance).await;
    })
    .await;
}

/// A file staged as moved out of a directory `lore dirty move` moved leaves the directory's move
/// marked dirty and not staged: the commit takes the file's move and leaves the directory's out.
#[tokio::test]
async fn a_commit_leaves_out_the_dirty_move_of_a_directory_a_staged_move_left() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["d/x.txt", "d/y.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        dirty_moved(&instance, "d", "e").await;
        std::fs::rename(instance.path.join("e/x.txt"), instance.path.join("x.txt"))
            .expect("the move on disk");
        let signature = lore_revision::file::stage::stage_move(
            instance.repository.clone(),
            &instance.write_token,
            instance.path.join("e/x.txt").to_string_lossy().into_owned(),
            instance.path.join("x.txt").to_string_lossy().into_owned(),
            Default::default(),
        )
        .await
        .expect("the staged move");
        let state = State::deserialize(instance.repository.clone(), signature)
            .await
            .expect("the staged state");
        let left = node_at(&instance, &state, "e").await;
        assert!(
            left.is_dirty_move() && !left.is_staged_move(),
            "e: flags {:#x}",
            left.flags
        );

        let (revision, _) = committed_warning(&instance, "Moved").await;
        assert_revision_of(
            &instance,
            &revision,
            &held,
            &[
                ("d", "d"),
                ("d/y.txt", "d/y.txt"),
                ("other.txt", "other.txt"),
                ("x.txt", "d/x.txt"),
            ],
            &[],
        )
        .await;
    })
    .await;
}

/// A directory a staged move took a file out of stays marked for the commit's rehash once a file
/// added in it is removed and its path marked dirty again: the revision holds directories whose
/// addresses hash their children.
#[tokio::test]
async fn a_commit_rehashes_a_directory_a_staged_move_left_once_an_add_in_it_is_reverted() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["p/f.txt", "p/g.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        std::fs::rename(instance.path.join("p/f.txt"), instance.path.join("f.txt"))
            .expect("the move on disk");
        lore_revision::file::stage::stage_move(
            instance.repository.clone(),
            &instance.write_token,
            instance.path.join("p/f.txt").to_string_lossy().into_owned(),
            instance.path.join("f.txt").to_string_lossy().into_owned(),
            Default::default(),
        )
        .await
        .expect("the staged move");
        written(&instance, &["p/tmp"]);
        dirtied(&instance, "p/tmp").await;
        std::fs::remove_file(instance.path.join("p/tmp")).expect("the delete on disk");
        dirtied(&instance, "p/tmp").await;

        let (revision, _) = committed_warning(&instance, "Moved").await;
        assert_revision_of(
            &instance,
            &revision,
            &held,
            &[
                ("f.txt", "p/f.txt"),
                ("other.txt", "other.txt"),
                ("p", "p"),
                ("p/g.txt", "p/g.txt"),
            ],
            &[],
        )
        .await;
    })
    .await;
}

/// A committed file `lore dirty move` moved into a committed directory, whose committed path
/// another file a dirty move took holds, keeps its move once the directory is removed on disk and
/// marked dirty: a commit of another staged file returns both moves, and stores the committed
/// tree with that edit alone, each directory holding the address its children hash to.
#[tokio::test]
async fn a_commit_returns_a_file_moved_into_a_removed_directory_whose_place_another_move_took() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["d/keep.txt", "x.txt", "y.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        dirty_moved(&instance, "x.txt", "d/x.txt").await;
        dirty_moved(&instance, "y.txt", "x.txt").await;
        removed_directory(&instance, "d");
        let state = dirtied(&instance, "d").await;
        let x = node_of(&instance, &state, held["x.txt"]).await;
        assert!(
            x.is_dirty_move() && !x.is_dirty_delete(),
            "x.txt: flags {:#x}",
            x.flags
        );
        restaged(&instance, "other.txt").await;

        let (revision, _) = committed_warning(&instance, "Edited").await;
        assert_revision_holds(&instance, &revision, &held).await;
    })
    .await;
}

/// A committed file `lore dirty move` moved into a directory marked dirty as added, whose
/// committed path another file a dirty move took holds, leaves the directory before its discard
/// once the directory is removed on disk and marked dirty: a commit of another staged file
/// returns both moves and stores the committed tree with that edit alone.
#[tokio::test]
async fn a_commit_returns_a_file_moved_into_a_removed_added_directory_whose_place_another_move_took()
 {
    with_instance(|instance| async move {
        let first = committed(&instance, &["x.txt", "y.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        made_directory(&instance, "n");
        dirtied(&instance, "n").await;
        dirty_moved(&instance, "x.txt", "n/x.txt").await;
        dirty_moved(&instance, "y.txt", "x.txt").await;
        removed_directory(&instance, "n");
        dirtied(&instance, "n").await;
        restaged(&instance, "other.txt").await;

        let (revision, _) = committed_warning(&instance, "Edited").await;
        assert_revision_holds(&instance, &revision, &held).await;
    })
    .await;
}

/// `lore dirty move` of a directory that holds no staged content, only a directory a staged move
/// took a file out of, two levels below, marks the move dirty and does not stage it; a commit
/// takes the staged move alone, the directory left rehashed, and leaves the dirty move out.
#[tokio::test]
async fn a_dirty_move_of_a_directory_holding_only_a_directory_a_staged_move_left_stays_unstaged() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["p/s/f.txt", "p/s/g.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        staged_moved(&instance, "p/s/f.txt", "f.txt").await;
        let state = dirty_moved(&instance, "p", "q").await;
        let q = node_at(&instance, &state, "q").await;
        assert!(
            q.is_dirty_move() && !q.is_staged(),
            "q: flags {:#x}",
            q.flags
        );

        let (revision, _) = committed_warning(&instance, "Moved").await;
        assert_revision(
            &instance,
            &revision,
            &[
                ("f.txt", held["p/s/f.txt"]),
                ("other.txt", held["other.txt"]),
                ("p", held["p"]),
                ("p/s", held["p/s"]),
                ("p/s/g.txt", held["p/s/g.txt"]),
            ],
        )
        .await;
    })
    .await;
}

/// `lore unstage .` after `lore dirty move` of a file whose edit is staged into a committed
/// directory reaches the staged move there: nothing stays staged.
#[tokio::test]
async fn an_unstage_of_the_tree_reaches_a_staged_file_dirty_moved_into_a_committed_directory() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["d/keep.txt", "f.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        restaged(&instance, "f.txt").await;
        dirty_moved(&instance, "f.txt", "d/f.txt").await;

        lore_revision::file::unstage::unstage(
            instance.repository.clone(),
            &instance.write_token,
            lore_revision::interface::LoreArray::from_vec(vec![
                lore_revision::interface::LoreString::from(&instance.path),
            ]),
            lore_revision::file::unstage::UnstageOptions::default(),
        )
        .await
        .expect("the unstage");
        let staged = staged_tree(&instance).await;
        let f = node_of(&instance, &staged, held["f.txt"]).await;
        assert!(!f.is_staged(), "d/f.txt: flags {:#x}", f.flags);
        assert!(
            !staged
                .node_has_staged_children(instance.repository.clone(), ROOT_NODE)
                .await
                .expect("the root's children"),
            "the staged tree holds a staged node"
        );
    })
    .await;
}

/// A commit of a directory the working tree no longer holds, after edits in it were staged,
/// counts what it does as a commit of the directory's staged deletion does: each file and
/// directory once in the totals and once as done, and as deleted.
#[tokio::test]
async fn a_commit_of_a_removed_directory_counts_it_as_its_staged_deletion() {
    let mut counts = Vec::new();
    for staged in [false, true] {
        let (sender, receiver) = std::sync::mpsc::channel();
        with_instance(|instance| async move {
            committed(&instance, &["a/x.txt", "a/y.txt", "a/z.txt", "other.txt"]).await;
            restaged(&instance, "a/x.txt").await;
            restaged(&instance, "a/y.txt").await;
            removed_directory(&instance, "a");
            if staged {
                stage_path(&instance, "a").await;
            }
            let _ = sender.send(counted_commit(&instance).await);
        })
        .await;
        counts.push(receiver.recv().expect("the counts"));
    }
    assert_eq!(counts[0], counts[1], "gone, then staged for deletion");
    let [
        directories,
        directory_total,
        files,
        file_total,
        directories_deleted,
        files_deleted,
    ] = counts[0];
    assert_eq!((files, file_total, files_deleted), (3, 3, 3));
    assert_eq!(directories, directory_total);
    assert_eq!(directories_deleted, 1);
}

/// A commit of a directory the working tree no longer holds, after edits in the directories below
/// it were staged, counts each of those directories once in the totals and once as done, as a
/// commit of the directory's staged deletion does, though the walk entered them first.
#[tokio::test]
async fn a_commit_of_a_removed_directory_counts_the_directories_the_walk_entered_once() {
    let mut counts = Vec::new();
    for staged in [false, true] {
        let (sender, receiver) = std::sync::mpsc::channel();
        with_instance(|instance| async move {
            committed(
                &instance,
                &["a/b/x.txt", "a/c/y.txt", "a/z.txt", "other.txt"],
            )
            .await;
            restaged(&instance, "a/b/x.txt").await;
            restaged(&instance, "a/c/y.txt").await;
            removed_directory(&instance, "a");
            if staged {
                stage_path(&instance, "a").await;
            }
            let _ = sender.send(counted_commit(&instance).await);
        })
        .await;
        counts.push(receiver.recv().expect("the counts"));
    }
    assert_eq!(counts[0], counts[1], "gone, then staged for deletion");
    let [
        directories,
        directory_total,
        _,
        _,
        directories_deleted,
        files_deleted,
    ] = counts[0];
    assert_eq!(directories, directory_total);
    assert_eq!((directories_deleted, files_deleted), (3, 3));
}

/// Commits what `instance` holds staged, answering the counts the commit reports at its end:
/// directories done and in total, files done and in total, and directories and files deleted.
async fn counted_commit(instance: &TestRepository) -> [u64; 6] {
    let counted = Arc::new(Mutex::new(None));
    let seen = counted.clone();
    let callback: LoreEventCallback = Some(Box::new(move |event| {
        if let LoreEvent::RevisionCommitEnd(end) = event {
            let count = &end.count;
            *seen.lock() = Some([
                count.directory_count,
                count.directory_total,
                count.file_count,
                count.file_total,
                count.directory_delete_count,
                count.file_delete_count,
            ]);
        }
    }));
    let execution = Arc::new(ExecutionContext::new_client_with_user_id(
        LoreGlobalArgs::default(),
        EventDispatcher::new(callback),
        "test-user".to_string(),
    ));
    LORE_CONTEXT
        .scope(execution.clone(), test_commit(instance, "Counted"))
        .await;
    execution.dispatcher.drain().await;
    counted.lock().take().expect("the commit's end")
}

/// A committed file `lore dirty move` moved out of a committed directory that holds a staged edit,
/// which is then removed on disk and not marked, goes back into the directory for the commit,
/// which deletes the directory with it as the working tree no longer holds it: the commit warns
/// that it does not keep the move, and the file at the moved path is marked dirty as added.
#[tokio::test]
async fn a_commit_deletes_a_file_dirty_moved_out_of_a_removed_directory_with_it() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["a/x.txt", "a/y.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        restaged(&instance, "a/y.txt").await;
        dirty_moved(&instance, "a/x.txt", "x.txt").await;
        removed_directory(&instance, "a");

        let (revision, warnings) = committed_warning(&instance, "Removed").await;
        assert_revision_holds(&instance, &revision, &without(&held, "a")).await;
        assert_eq!(warnings, 1);
        let staged = staged_tree(&instance).await;
        let x = node_at(&instance, &staged, "x.txt").await;
        assert!(
            x.is_dirty_add() && !x.is_staged(),
            "x.txt: flags {:#x}",
            x.flags
        );
    })
    .await;
}

/// A staged add reverted in a committed directory that holds a file marked dirty as added, and
/// nothing else changed, leaves the directory unstaged: the children a commit would take hash to
/// its address, the file marked added left out.
#[tokio::test]
async fn a_reverted_staged_add_beside_a_dirty_add_leaves_its_directory_unstaged() {
    with_instance(|instance| async move {
        committed(&instance, &["p/a.txt", "other.txt"]).await;
        written(&instance, &["p/x.txt"]);
        stage_path(&instance, "p/x.txt").await;
        written(&instance, &["p/y.txt"]);
        dirtied(&instance, "p/y.txt").await;
        std::fs::remove_file(instance.path.join("p/x.txt")).expect("the delete on disk");
        let state = dirtied(&instance, "p/x.txt").await;

        let p = node_at(&instance, &state, "p").await;
        assert!(!p.is_staged() && p.is_dirty(), "p: flags {:#x}", p.flags);
    })
    .await;
}

/// `lore dirty copy` into a new directory at the name of a file staged for deletion adds the
/// directory beside the deleted file and the copy in it, as `lore dirty move` does; a copy below a
/// file the working tree holds is refused.
#[tokio::test]
async fn a_dirty_copy_below_the_name_of_a_file_staged_for_deletion_lands_in_a_directory() {
    with_instance(|instance| async move {
        committed(&instance, &["a/f1", "a/f2", "c", "other.txt"]).await;
        std::fs::remove_file(instance.path.join("a/f1")).expect("the delete on disk");
        stage_path(&instance, "a/f1").await;
        made_directory(&instance, "a/F1");
        std::fs::copy(instance.path.join("c"), instance.path.join("a/F1/n")).expect("the copy");
        let signature = lore_revision::file::dirty::dirty_copy(
            instance.repository.clone(),
            instance.path.join("c").to_string_lossy().into_owned(),
            instance.path.join("a/F1/n").to_string_lossy().into_owned(),
        )
        .await
        .expect("the dirty copy");
        let state = crate::dirty_move_fixture::staged(&instance, signature).await;
        assert_below_a_directory(&instance, &state, "a/F1/n").await;

        for refused in [
            lore_revision::file::dirty::dirty_copy(
                instance.repository.clone(),
                instance.path.join("c").to_string_lossy().into_owned(),
                instance.path.join("a/f2/n").to_string_lossy().into_owned(),
            )
            .await,
            lore_revision::file::dirty::dirty_move(
                instance.repository.clone(),
                instance.path.join("c").to_string_lossy().into_owned(),
                instance.path.join("a/f2/n").to_string_lossy().into_owned(),
            )
            .await,
        ] {
            assert!(
                refused
                    .as_ref()
                    .is_err_and(|error| error.is_invalid_arguments()),
                "below a/f2: {:?}",
                refused.map(|_| ())
            );
        }
    })
    .await;
}

/// Where a copy `lore dirty copy` marked stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CopiedInto {
    /// The root, the copy removed alone.
    Root,
    /// A new directory marked added, removed with it.
    NewDirectory,
    /// A committed directory, removed with it.
    CommittedDirectory,
}

/// The copy `lore dirty copy` marked, removed from the working tree and marked dirty, leaves the
/// staged tree as a reverted add does, rather than being marked deleted, alone, with a directory
/// marked added that holds it, or from below a committed directory removed with it, whose commit
/// then stores the base's addresses.
#[tokio::test]
async fn a_removed_dirty_copy_leaves_the_staged_tree_as_a_reverted_add() {
    for into in [
        CopiedInto::Root,
        CopiedInto::NewDirectory,
        CopiedInto::CommittedDirectory,
    ] {
        with_instance(|instance| async move {
            let first = committed(&instance, &["c.txt", "d/e.txt", "other.txt"]).await;
            let held = tree_paths(&instance, &first).await;
            let copy = match into {
                CopiedInto::Root => "k.txt",
                CopiedInto::NewDirectory => {
                    made_directory(&instance, "n");
                    dirtied(&instance, "n").await;
                    "n/k.txt"
                }
                CopiedInto::CommittedDirectory => "d/k.txt",
            };
            std::fs::copy(instance.path.join("c.txt"), instance.path.join(copy))
                .expect("the copy on disk");
            lore_revision::file::dirty::dirty_copy(
                instance.repository.clone(),
                instance.path.join("c.txt").to_string_lossy().into_owned(),
                instance.path.join(copy).to_string_lossy().into_owned(),
            )
            .await
            .expect("the dirty copy");
            let removed = match into {
                CopiedInto::Root => {
                    std::fs::remove_file(instance.path.join(copy)).expect("the removal on disk");
                    copy
                }
                CopiedInto::NewDirectory => {
                    removed_directory(&instance, "n");
                    "n"
                }
                CopiedInto::CommittedDirectory => {
                    removed_directory(&instance, "d");
                    "d"
                }
            };
            let state = dirtied(&instance, removed).await;

            let gone = if into == CopiedInto::CommittedDirectory {
                copy
            } else {
                removed
            };
            assert!(
                !crate::dirty_move_fixture::holds(&instance, &state, gone).await,
                "{into:?}: {gone} stays in the staged tree"
            );
            if into == CopiedInto::CommittedDirectory {
                restaged(&instance, "other.txt").await;
                let (revision, _) = committed_warning(&instance, "Edited").await;
                assert_revision_holds(&instance, &revision, &held).await;
            }
        })
        .await;
    }
}

/// `lore dirty move` from the name of a file staged for deletion, which a node renamed in place
/// took, moves the renamed node, whichever of the two its directory lists first.
#[tokio::test]
async fn a_dirty_move_from_the_name_of_a_file_staged_for_deletion_moves_the_live_node() {
    let orders = Arc::new(Mutex::new(BTreeSet::new()));
    for files in [["a/e", "a/f1"], ["a/f1", "a/e"]] {
        let orders = orders.clone();
        with_instance(|instance| async move {
            let first = committed(&instance, &["a/e", "a/f1", "b/k", "other.txt"]).await;
            let held = tree_paths(&instance, &first).await;
            put_first(&instance, files[0]).await;
            std::fs::remove_file(instance.path.join("a/f1")).expect("the delete on disk");
            stage_path(&instance, "a/f1").await;
            let state = dirty_moved(&instance, "a/e", "a/f1").await;
            let ahead = lies_ahead(&instance, &state, held["a"], (held["a/e"], held["a/f1"])).await;
            orders.lock().insert(ahead);
            let state = dirty_moved(&instance, "a/f1", "b/f1").await;

            let moved = node_at(&instance, &state, "b/f1").await;
            assert_eq!(
                node_id_at(&instance, &state, "b/f1").await,
                held["a/e"],
                "{files:?}: the node at b/f1, flags {:#x}",
                moved.flags
            );
            let deleted = node_of(&instance, &state, held["a/f1"]).await;
            assert!(
                deleted.is_staged_delete(),
                "{files:?}: a/f1, flags {:#x}",
                deleted.flags
            );
        })
        .await;
    }
    assert_eq!(orders.lock().len(), 2, "both chain orders");
}

/// A walk of a directory where a node renamed in place took the name of a file staged for deletion
/// listed ahead of it marks the renamed node at that name and adds none beside it, whether the
/// walk searches the directory's chain or lists it for more entries than it searches.
#[tokio::test]
async fn a_walk_takes_the_live_node_at_the_name_of_a_file_staged_for_deletion() {
    for width in [0, SEARCHED_CHILDREN + 1] {
        with_instance(|instance| async move {
            let mut files = vec![
                "a/f1".to_string(),
                "a/e".to_string(),
                "other.txt".to_string(),
            ];
            files.extend((0..width).map(|index| format!("a/p{index}")));
            let files: Vec<&str> = files.iter().map(String::as_str).collect();
            let first = committed(&instance, &files).await;
            let held = tree_paths(&instance, &first).await;
            put_first(&instance, "a/f1").await;
            std::fs::remove_file(instance.path.join("a/f1")).expect("the delete on disk");
            stage_path(&instance, "a/f1").await;
            let state = dirty_moved(&instance, "a/e", "a/f1").await;
            assert!(
                !lies_ahead(&instance, &state, held["a"], (held["a/e"], held["a/f1"])).await,
                "{width}: the deleted file lies ahead of the renamed one"
            );
            let state = dirtied(&instance, "a").await;

            let a = node_id_at(&instance, &state, "a").await;
            assert_eq!(
                children_named(&instance, &state, a, "f1").await,
                2,
                "{width}: the deleted file and the renamed one at a/f1"
            );
            let renamed = node_of(&instance, &state, held["a/e"]).await;
            assert!(
                renamed.is_dirty_move() && !renamed.is_staged(),
                "{width}: a/f1, flags {:#x}",
                renamed.flags
            );
        })
        .await;
    }
}

/// A commit returns a file `lore dirty move` moved, the move not staged, to its committed place
/// where the node another move took there is staged for deletion, rather than leaving it out.
#[tokio::test]
async fn a_commit_returns_a_file_to_a_place_whose_holder_is_staged_for_deletion() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["a/f", "a/g", "d/k", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        dirty_moved(&instance, "a/f", "d/f").await;
        dirty_moved(&instance, "a/g", "a/f").await;
        std::fs::remove_file(instance.path.join("a/f")).expect("the delete on disk");
        stage_path(&instance, "a/f").await;
        let (revision, _) = committed_warning(&instance, "Deleted").await;

        let kept = tree_paths(&instance, &revision).await;
        assert_eq!(
            kept.get("a/f"),
            Some(&held["a/f"]),
            "the revision's paths: {:?}",
            kept.keys().collect::<Vec<_>>()
        );
        assert!(!kept.contains_key("a/g"), "a/g stays in the revision");
    })
    .await;
}

/// How a staged node comes to stand below a directory `lore dirty move` moved.
#[derive(Clone, Copy, Debug)]
enum ArrivesBelow {
    /// A stage of a new file inside it.
    Staged,
    /// `lore dirty move` of a file holding a staged edit into it.
    DirtyMoved,
    /// `lore stage move` of a file into it.
    StagedMove,
}

/// A staged node arriving below a directory `lore dirty move` moved, by a stage inside it, by
/// `lore dirty move` of a staged file into it or by `lore stage move` into it, stages the
/// directory's whole move: each file that moves with it alone takes a staged move.
#[tokio::test]
async fn a_staged_node_below_a_dirty_moved_directory_stages_its_whole_move() {
    for arrives in [
        ArrivesBelow::Staged,
        ArrivesBelow::DirtyMoved,
        ArrivesBelow::StagedMove,
    ] {
        with_instance(|instance| async move {
            let first = committed(&instance, &["p/f1", "p/f2", "x", "other.txt"]).await;
            let held = tree_paths(&instance, &first).await;
            dirty_moved(&instance, "p", "r").await;
            match arrives {
                ArrivesBelow::Staged => {
                    written(&instance, &["r/n"]);
                    stage_path(&instance, "r/n").await;
                }
                ArrivesBelow::DirtyMoved => {
                    restaged(&instance, "x").await;
                    dirty_moved(&instance, "x", "r/x").await;
                }
                ArrivesBelow::StagedMove => {
                    staged_moved(&instance, "x", "r/x").await;
                }
            }
            let state = staged_tree(&instance).await;

            let r = node_of(&instance, &state, held["p"]).await;
            assert!(r.is_staged_move(), "{arrives:?}: r, flags {:#x}", r.flags);
            for file in ["p/f1", "p/f2"] {
                let moved = node_of(&instance, &state, held[file]).await;
                assert!(
                    moved.is_staged_move() && !moved.is_dirty(),
                    "{arrives:?}: {file}, flags {:#x}",
                    moved.flags
                );
            }
        })
        .await;
    }
}

/// How a stage reaches a node renamed in place onto the name of a file staged for deletion.
#[derive(Clone, Copy, Debug)]
enum StagedThere {
    /// `lore stage move` of it to `b/f1`.
    Moved,
    /// `lore stage` of its path.
    Path,
    /// `lore stage` of its path beside another target in its directory, staged ahead of it.
    Targets,
    /// `lore stage` of its directory.
    Directory,
}

/// `lore stage move` from the name of a file staged for deletion, which a node renamed in place
/// took, and `lore stage` of a node standing there, of its path alone or beside another, or of its
/// directory, narrow or listed, take the renamed node, whichever of the two its directory lists
/// first.
#[tokio::test]
async fn a_stage_at_the_name_of_a_file_staged_for_deletion_takes_the_live_node() {
    let shapes = [
        (StagedThere::Moved, 0),
        (StagedThere::Path, 0),
        (StagedThere::Targets, 0),
        (StagedThere::Directory, 0),
        (StagedThere::Directory, SEARCHED_CHILDREN + 1),
    ];
    for (staged, width) in shapes {
        let orders = Arc::new(Mutex::new(BTreeSet::new()));
        for files in [["a/e", "a/f1"], ["a/f1", "a/e"]] {
            let orders = orders.clone();
            with_instance(|instance| async move {
                let mut all = vec![
                    "a/e".to_string(),
                    "a/f1".to_string(),
                    "a/b0".to_string(),
                    "b/k".to_string(),
                    "other.txt".to_string(),
                ];
                all.extend((0..width).map(|index| format!("a/q{index}")));
                let all: Vec<&str> = all.iter().map(String::as_str).collect();
                let first = committed(&instance, &all).await;
                let held = tree_paths(&instance, &first).await;
                put_first(&instance, files[0]).await;
                std::fs::remove_file(instance.path.join("a/f1")).expect("the delete on disk");
                stage_path(&instance, "a/f1").await;
                let state = dirty_moved(&instance, "a/e", "a/f1").await;
                let ahead =
                    lies_ahead(&instance, &state, held["a"], (held["a/e"], held["a/f1"])).await;
                orders.lock().insert(ahead);
                let state = match staged {
                    StagedThere::Moved => {
                        staged_moved(&instance, "a/f1", "b/f1").await;
                        staged_tree(&instance).await
                    }
                    StagedThere::Path => {
                        written(&instance, &["a/f1"]);
                        stage_path(&instance, "a/f1").await
                    }
                    StagedThere::Targets => {
                        written(&instance, &["a/b0", "a/f1"]);
                        let signature = lore_revision::file::stage::stage(
                            instance.repository.clone(),
                            &instance.write_token,
                            LoreArray::from_vec(
                                ["a/b0", "a/f1"]
                                    .map(|path| LoreString::from(&instance.path.join(path)))
                                    .to_vec(),
                            ),
                            Default::default(),
                        )
                        .await
                        .expect("the stage");
                        crate::dirty_move_fixture::staged(&instance, signature).await
                    }
                    StagedThere::Directory => {
                        written(&instance, &["a/f1"]);
                        dirtied(&instance, "a/f1").await;
                        stage_path(&instance, "a").await
                    }
                };

                let renamed = node_of(&instance, &state, held["a/e"]).await;
                assert!(
                    renamed.is_staged() && !renamed.is_staged_delete(),
                    "{staged:?} {width}, {files:?}: the renamed node, flags {:#x}",
                    renamed.flags
                );
                if matches!(staged, StagedThere::Moved) {
                    assert_eq!(
                        node_id_at(&instance, &state, "b/f1").await,
                        held["a/e"],
                        "{files:?}: the node at b/f1"
                    );
                }
                let deleted = node_of(&instance, &state, held["a/f1"]).await;
                assert!(
                    deleted.is_staged_delete(),
                    "{staged:?} {width}, {files:?}: a/f1, flags {:#x}",
                    deleted.flags
                );
            })
            .await;
        }
        assert_eq!(
            orders.lock().len(),
            2,
            "{staged:?} {width}: both chain orders"
        );
    }
}

/// A walk of a directory the working tree no longer holds a node renamed in place in, onto the
/// name of a file staged for deletion, marks the renamed node deleted, whichever of the two the
/// directory lists first, whether the walk searches the directory's chains, lists the staged one
/// for the many children the working tree lacks, or lists both for the many entries it holds.
#[tokio::test]
async fn a_walk_marks_a_node_renamed_onto_the_name_of_a_file_staged_for_deletion_deleted() {
    let shapes = [
        (0, 0),
        (SEARCHED_CHILDREN + 1, SEARCHED_CHILDREN + 1),
        (SEARCHED_CHILDREN + 1, 0),
    ];
    for (width, removed) in shapes {
        let orders = Arc::new(Mutex::new(BTreeSet::new()));
        for files in [["a/e", "a/f1"], ["a/f1", "a/e"]] {
            let orders = orders.clone();
            with_instance(|instance| async move {
                let mut all = vec!["a/e".to_string(), "a/f1".to_string(), "other.txt".into()];
                all.extend((0..width).map(|index| format!("a/p{index}")));
                let all: Vec<&str> = all.iter().map(String::as_str).collect();
                let first = committed(&instance, &all).await;
                let held = tree_paths(&instance, &first).await;
                put_first(&instance, files[0]).await;
                std::fs::remove_file(instance.path.join("a/f1")).expect("the delete on disk");
                stage_path(&instance, "a/f1").await;
                let state = dirty_moved(&instance, "a/e", "a/f1").await;
                let ahead =
                    lies_ahead(&instance, &state, held["a"], (held["a/e"], held["a/f1"])).await;
                orders.lock().insert(ahead);
                std::fs::remove_file(instance.path.join("a/f1")).expect("the removal on disk");
                for index in 0..removed {
                    std::fs::remove_file(instance.path.join(format!("a/p{index}")))
                        .expect("the removal on disk");
                }
                let state = dirtied(&instance, "a").await;

                let renamed = node_of(&instance, &state, held["a/e"]).await;
                assert!(
                    renamed.is_dirty_delete(),
                    "{files:?}, {width} wide, {removed} removed: the renamed node, flags {:#x}",
                    renamed.flags
                );
            })
            .await;
        }
        assert_eq!(orders.lock().len(), 2, "{width} wide: both chain orders");
    }
}

/// A file removed from a directory `lore dirty move` moved is marked deleted by a walk of the
/// directory at its new path, and by a walk of the tree, whether the walk searches the directory's
/// chain or lists it.
#[tokio::test]
async fn a_walk_marks_a_file_removed_from_a_dirty_moved_directory_deleted() {
    for (walked, width) in [("e", 0), (".", 0), ("e", SEARCHED_CHILDREN + 1)] {
        with_instance(|instance| async move {
            let mut files = vec!["d/f.txt".to_string(), "d/g.txt".into(), "other.txt".into()];
            files.extend((0..width).map(|index| format!("d/p{index}")));
            let files: Vec<&str> = files.iter().map(String::as_str).collect();
            let first = committed(&instance, &files).await;
            let held = tree_paths(&instance, &first).await;
            dirty_moved(&instance, "d", "e").await;
            std::fs::remove_file(instance.path.join("e/f.txt")).expect("the delete on disk");
            let state = dirtied(&instance, walked).await;

            let f = node_of(&instance, &state, held["d/f.txt"]).await;
            assert!(
                f.is_dirty_delete(),
                "{walked} {width}: e/f.txt, flags {:#x}",
                f.flags
            );
            let g = node_of(&instance, &state, held["d/g.txt"]).await;
            assert!(
                !g.is_dirty_delete(),
                "{walked} {width}: e/g.txt, flags {:#x}",
                g.flags
            );
        })
        .await;
    }
}

/// How a staged file leaves its directory.
#[derive(Clone, Copy, Debug)]
enum LeavesBy {
    /// `lore dirty move`, which stages the move of a file holding a staged edit.
    DirtyMove,
    /// `lore stage move`.
    StagedMove,
}

/// A commit of a directory a staged move took a staged file out of, which the directory is staged
/// for, as its address changes, deletes the directory with everything in it where the working tree
/// no longer holds it, the files never staged included, and keeps it as committed where it does.
#[tokio::test]
async fn a_commit_deletes_a_removed_directory_a_staged_move_left() {
    for (by, removed) in [
        (LeavesBy::DirtyMove, true),
        (LeavesBy::StagedMove, true),
        (LeavesBy::DirtyMove, false),
        (LeavesBy::StagedMove, false),
    ] {
        with_instance(|instance| async move {
            let first = committed(&instance, &["a/f1", "a/f2", "a/s/g1", "other.txt"]).await;
            let held = tree_paths(&instance, &first).await;
            restaged(&instance, "a/f2").await;
            match by {
                LeavesBy::DirtyMove => {
                    dirty_moved(&instance, "a/f2", "f2").await;
                }
                LeavesBy::StagedMove => {
                    staged_moved(&instance, "a/f2", "f2").await;
                }
            }
            if removed {
                removed_directory(&instance, "a");
                dirtied(&instance, "a").await;
            }

            let (revision, _) = committed_warning(&instance, "Moved").await;
            let kept = tree_paths(&instance, &revision).await;
            let mut expected = vec![("f2", "a/f2"), ("other.txt", "other.txt")];
            if !removed {
                expected.extend([
                    ("a", "a"),
                    ("a/f1", "a/f1"),
                    ("a/s", "a/s"),
                    ("a/s/g1", "a/s/g1"),
                ]);
            }
            let expected: BTreeMap<String, NodeID> = expected
                .into_iter()
                .map(|(path, committed)| (path.to_string(), held[committed]))
                .collect();
            assert_eq!(kept, expected, "{by:?}, removed {removed}");
        })
        .await;
    }
}

/// A staged add removed from a committed directory a move marked dirty took a file out of leaves
/// the directory unstaged: neither move is one the commit takes, so the directory is staged for
/// nothing, and a commit of another file, with the directory removed from the working tree since,
/// keeps it with what it holds, the dirty move left out.
#[tokio::test]
async fn a_reverted_staged_add_beside_a_dirty_move_out_leaves_its_directory_unstaged() {
    with_instance(|instance| async move {
        let first = committed(&instance, &["a/x.txt", "a/y.txt", "other.txt"]).await;
        let held = tree_paths(&instance, &first).await;
        written(&instance, &["a/n.txt"]);
        stage_path(&instance, "a/n.txt").await;
        dirty_moved(&instance, "a/x.txt", "x.txt").await;
        std::fs::remove_file(instance.path.join("a/n.txt")).expect("the removal on disk");
        let state = dirtied(&instance, "a/n.txt").await;
        let a = node_at(&instance, &state, "a").await;
        assert!(!a.is_staged(), "a: flags {:#x}", a.flags);
        removed_directory(&instance, "a");

        restaged(&instance, "other.txt").await;
        let (revision, _) = committed_warning(&instance, "Edited").await;
        assert_revision_holds(&instance, &revision, &held).await;
    })
    .await;
}

/// A directory removed from the working tree and marked dirty that holds a staged add leaves the
/// add staged, marked dirty alone, as a mark changes nothing staged: a commit of another file
/// takes what the working tree holds as it does where nothing was marked, the directory gone and
/// the add committed where its file came back, each directory at the address its children hash
/// to.
#[tokio::test]
async fn a_removed_directory_holding_a_staged_add_commits_as_with_no_mark() {
    for came_back in [false, true] {
        let mut committed_paths = Vec::new();
        for marked in [false, true] {
            let (sender, receiver) = std::sync::mpsc::channel();
            with_instance(|instance| async move {
                committed(&instance, &["a/x.txt", "a/y.txt", "other.txt"]).await;
                written(&instance, &["a/n.txt"]);
                stage_path(&instance, "a/n.txt").await;
                removed_directory(&instance, "a");
                if marked {
                    let state = dirtied(&instance, "a").await;
                    let added = node_at(&instance, &state, "a/n.txt").await;
                    assert!(
                        added.is_staged_add(),
                        "a/n.txt stays staged, flags {:#x}",
                        added.flags
                    );
                }
                if came_back {
                    written(&instance, &["a/n.txt"]);
                }

                restaged(&instance, "other.txt").await;
                let (revision, _) = committed_warning(&instance, "Edited").await;
                let held = tree_paths(&instance, &revision).await;
                assert_addresses(&instance, &revision, &held).await;
                sender
                    .send(held.into_keys().collect::<Vec<String>>())
                    .expect("the paths sent");
            })
            .await;
            committed_paths.push(receiver.recv().expect("the paths"));
        }
        assert_eq!(
            committed_paths[1], committed_paths[0],
            "came back {came_back}: the mark changes what the commit takes"
        );
    }
}

/// Unstages `path`, below `instance`'s working tree.
async fn unstaged(instance: &TestRepository, path: &str) {
    lore_revision::file::unstage::unstage(
        instance.repository.clone(),
        &instance.write_token,
        lore_revision::interface::LoreArray::from_vec(vec![
            lore_revision::interface::LoreString::from(&instance.path.join(path)),
        ]),
        lore_revision::file::unstage::UnstageOptions::default(),
    )
    .await
    .expect("the unstage");
}

/// Resets `path`, below `instance`'s working tree, to the current revision.
async fn reset_to_current(
    instance: &TestRepository,
    path: &str,
) -> Result<(), lore_revision::file::reset::ResetError> {
    lore_revision::file::reset::reset(
        instance.repository.clone(),
        &instance.write_token,
        LoreArray::from_vec(vec![LoreString::from(&instance.path.join(path))]),
        LoreString::default(),
        lore_revision::file::reset::ResetOptions::default(),
    )
    .await
    .map(drop)
}

/// What takes the name of the committed `a/f`, marked deleted, beside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TookName {
    /// `b`, marked dirty as moved there, which lies ahead of it in the chain.
    MovedIn,
    /// `a/g`, renamed there in place, which keeps its place in the chain: behind it where `behind`
    /// says, which moves `a/f` to the front first, and ahead of it otherwise, which moves `a/g`
    /// there. The commit's order, which follows the working tree's listing, sets neither.
    Renamed { behind: bool },
    /// A copy of `other.txt` marked dirty there, which lies ahead of it.
    Copied,
    /// A new file added as `a/h`, staged where `staged` says and marked dirty otherwise, renamed
    /// there after the deleted node was moved to the front of the chain, so it lies behind it.
    AddedBehind { staged: bool },
}

/// The committed `a/f`, `a/g`, `b` and `other.txt`, then `a/f` removed, its deletion staged where
/// `staged_deletion` says and marked dirty otherwise, and its name taken as `took` says. Answers the
/// committed nodes, the other node at `a/f`, and whether it lies ahead of the deleted one in the
/// chain.
async fn beside_deleted_f(
    instance: &TestRepository,
    took: TookName,
    staged_deletion: bool,
) -> (BTreeMap<String, NodeID>, NodeID, bool) {
    let first = committed(instance, &["a/f", "a/g", "b", "other.txt"]).await;
    let held = tree_paths(instance, &first).await;
    match took {
        TookName::AddedBehind { staged } => {
            written(instance, &["a/h"]);
            if staged {
                stage_path(instance, "a/h").await;
            } else {
                dirtied(instance, "a/h").await;
            }
            to_front(instance, "a/f").await;
            without_front(instance).await;
        }
        TookName::Renamed { behind } => {
            to_front(instance, if behind { "a/f" } else { "a/g" }).await;
            without_front(instance).await;
        }
        TookName::MovedIn | TookName::Copied => {}
    }
    std::fs::remove_file(instance.path.join("a/f")).expect("the removal on disk");
    if staged_deletion {
        stage_path(instance, "a/f").await;
    } else {
        dirtied(instance, "a/f").await;
    }
    match took {
        TookName::MovedIn => {
            dirty_moved(instance, "b", "a/f").await;
        }
        TookName::Renamed { .. } => {
            dirty_moved(instance, "a/g", "a/f").await;
        }
        TookName::Copied => {
            std::fs::copy(instance.path.join("other.txt"), instance.path.join("a/f"))
                .expect("the copy on disk");
            lore_revision::file::dirty::dirty_copy(
                instance.repository.clone(),
                instance
                    .path
                    .join("other.txt")
                    .to_string_lossy()
                    .into_owned(),
                instance.path.join("a/f").to_string_lossy().into_owned(),
            )
            .await
            .expect("the dirty copy");
        }
        TookName::AddedBehind { .. } => {
            dirty_moved(instance, "a/h", "a/f").await;
        }
    }
    let state = staged_tree(instance).await;
    let mut named_f = Vec::new();
    for child in state
        .node_children(instance.repository.clone(), held["a"])
        .await
        .expect("the children of a")
    {
        let name = state
            .node_name_clone(instance.repository.clone(), child)
            .await
            .expect("the child's name");
        if name == "f" {
            named_f.push(child);
        }
    }
    assert_eq!(named_f.len(), 2, "{took:?}: a holds f twice");
    let live = named_f
        .iter()
        .copied()
        .find(|&child| child != held["a/f"])
        .expect("the node beside the deleted one");
    (held, live, named_f[0] == live)
}

/// The tree a reset leaves: the staged one, or the current one where nothing is left staged.
async fn tree_after(instance: &TestRepository) -> Arc<State> {
    let (current, staged, _) = State::deserialize_current_and_staged(instance.repository.clone())
        .await
        .expect("the states");
    staged.unwrap_or(current)
}

/// A reset of a path whose committed node is marked deleted beside a dirty add or copy that took
/// its name discards the add and restores the committed file, whichever of the two the chain holds
/// first.
#[tokio::test]
async fn a_reset_discards_a_dirty_add_beside_a_deleted_node_in_either_order() {
    let orders = Arc::new(Mutex::new(BTreeSet::new()));
    for took in [TookName::Copied, TookName::AddedBehind { staged: false }] {
        let orders = orders.clone();
        with_instance(|instance| async move {
            let (held, live, ahead) = beside_deleted_f(&instance, took, false).await;
            reset_to_current(&instance, "a/f").await.expect("the reset");
            let state = tree_after(&instance).await;
            assert_eq!(
                node_id_at(&instance, &state, "a/f").await,
                held["a/f"],
                "{took:?}: a/f holds its committed node"
            );
            let restored = node_of(&instance, &state, held["a/f"]).await;
            assert!(
                !restored.is_dirty() && !restored.is_staged(),
                "{took:?}: a/f, flags {:#x}",
                restored.flags
            );
            assert_eq!(
                children_named(&instance, &state, held["a"], "f").await,
                1,
                "{took:?}: the add {live} is gone"
            );
            assert_eq!(
                std::fs::read(instance.path.join("a/f")).expect("a/f on disk"),
                b"a/f",
                "{took:?}: a/f holds its committed content"
            );
            orders.lock().insert(ahead);
        })
        .await;
    }
    assert_eq!(orders.lock().len(), 2, "both chain orders");
}

/// A reset of a path whose committed node is marked deleted beside a node `lore file dirty move`
/// took there leaves both, as a reset leaves every move: the move stays, the deletion stays marked,
/// and the working tree keeps the moved file. Whichever of the two the chain holds first.
#[tokio::test]
async fn a_reset_leaves_a_dirty_move_beside_a_deleted_node_in_either_order() {
    let orders = Arc::new(Mutex::new(BTreeSet::new()));
    for (took, content) in [
        (TookName::MovedIn, &b"b"[..]),
        (TookName::Renamed { behind: true }, &b"a/g"[..]),
        (TookName::Renamed { behind: false }, &b"a/g"[..]),
    ] {
        let orders = orders.clone();
        with_instance(|instance| async move {
            let (held, live, ahead) = beside_deleted_f(&instance, took, false).await;
            let before = staged_tree(&instance).await.revision();
            reset_to_current(&instance, "a/f").await.expect("the reset");
            let state = staged_tree(&instance).await;
            assert_eq!(state.revision(), before, "{took:?}: the staged tree");
            assert!(
                node_of(&instance, &state, live).await.is_dirty_move(),
                "{took:?}: the move stays"
            );
            assert!(
                node_of(&instance, &state, held["a/f"])
                    .await
                    .is_dirty_delete(),
                "{took:?}: the deletion stays marked"
            );
            assert_eq!(
                std::fs::read(instance.path.join("a/f")).expect("a/f on disk"),
                content,
                "{took:?}: a/f holds the moved file"
            );
            orders.lock().insert(ahead);
        })
        .await;
    }
    assert_eq!(orders.lock().len(), 2, "both chain orders");
}

/// A reset of a path whose name a node marked deleted and a node beside it hold, either of them
/// staged, is refused and changes nothing, whichever of the two the chain holds first.
#[tokio::test]
async fn a_reset_refuses_a_name_held_twice_where_either_node_is_staged() {
    let orders = Arc::new(Mutex::new(BTreeSet::new()));
    for (took, staged_deletion) in [
        (TookName::MovedIn, true),
        (TookName::Renamed { behind: true }, true),
        (TookName::Copied, false),
        (TookName::AddedBehind { staged: true }, false),
    ] {
        let orders = orders.clone();
        with_instance(|instance| async move {
            let (held, live, ahead) = beside_deleted_f(&instance, took, staged_deletion).await;
            if took == TookName::Copied {
                restaged(&instance, "a/f").await;
            }
            let state = staged_tree(&instance).await;
            let (deleted, beside) = (
                node_of(&instance, &state, held["a/f"]).await,
                node_of(&instance, &state, live).await,
            );
            let case = format!("{took:?}, ahead {ahead}");
            assert!(
                deleted.is_staged() == staged_deletion && beside.is_staged() != staged_deletion,
                "{case}: deletion {:#x}, beside {:#x}",
                deleted.flags,
                beside.flags
            );
            let content = std::fs::read(instance.path.join("a/f")).expect("a/f on disk");
            let refused = reset_to_current(&instance, "a/f").await;
            assert!(
                refused
                    .as_ref()
                    .is_err_and(|error| error.is_invalid_arguments()),
                "{case}: {refused:?}"
            );
            assert_eq!(
                staged_tree(&instance).await.revision(),
                state.revision(),
                "{case}: the staged tree"
            );
            assert_eq!(
                std::fs::read(instance.path.join("a/f")).expect("a/f on disk"),
                content,
                "{case}: a/f on disk"
            );
            orders.lock().insert(ahead);
        })
        .await;
    }
    assert_eq!(orders.lock().len(), 2, "both chain orders");
}

/// Whether `node` lies ahead of `other` among the children of `parent` in `state`.
async fn lies_ahead(
    instance: &TestRepository,
    state: &State,
    parent: NodeID,
    (node, other): (NodeID, NodeID),
) -> bool {
    let children = state
        .node_children(instance.repository.clone(), parent)
        .await
        .expect("the children");
    let at = |id| children.iter().position(|&child| child == id);
    at(node) < at(other)
}

/// A path whose last name a directory holds twice, as a file staged for deletion and one `lore
/// dirty move` took there and staged edited, names the live node: a lookup answers it, a status of
/// the path reports it and not the deletion, and an unstage of the path unstages the edit and
/// leaves the deletion staged. Whichever of the two the chain holds first: a move from another
/// directory puts the moved file ahead, and a rename in place keeps its place, which `to_front`
/// sets before the deletion.
#[tokio::test]
async fn a_path_whose_last_name_is_held_twice_names_the_live_node() {
    let orders = Arc::new(Mutex::new(BTreeSet::new()));
    for (source, front) in [("b", None), ("a/g", Some("a/f")), ("a/g", Some("a/g"))] {
        let orders = orders.clone();
        with_instance(|instance| async move {
            let first = committed(&instance, &["a/f", "a/g", "b", "other.txt"]).await;
            let held = tree_paths(&instance, &first).await;
            if let Some(front) = front {
                to_front(&instance, front).await;
                without_front(&instance).await;
            }
            std::fs::remove_file(instance.path.join("a/f")).expect("the removal on disk");
            stage_path(&instance, "a/f").await;
            dirty_moved(&instance, source, "a/f").await;
            restaged(&instance, "a/f").await;
            let case = format!("{source} onto a/f, {front:?} at the front");

            let state = staged_tree(&instance).await;
            let ahead = lies_ahead(&instance, &state, held["a"], (held[source], held["a/f"])).await;
            orders.lock().insert(ahead);
            assert_eq!(
                node_id_at(&instance, &state, "a/f").await,
                held[source],
                "{case}: a/f names the live node"
            );
            let moved = node_of(&instance, &state, held[source]).await;
            assert!(
                moved.is_staged(),
                "{case}: the edit is staged, flags {:#x}",
                moved.flags
            );
            let reported = lore_revision::state::diff_collect(
                instance.repository.clone(),
                first.clone(),
                instance.repository.clone(),
                state.clone(),
                Some(lore_revision::util::path::RelativePath::new_from_clean_parts("a", "f")),
                lore_revision::filter::FilterMode::CHANGES,
            )
            .await
            .expect("the status of a/f");
            assert!(
                !reported.is_empty()
                    && reported.iter().all(|change| {
                        change.to.mapping.node == held[source]
                            && change.action != lore_revision::change::FileAction::Delete
                    }),
                "{case}: a status of a/f reports the live node, {:?}",
                reported
                    .iter()
                    .map(|change| (change.action, change.to.mapping.node))
                    .collect::<Vec<_>>()
            );

            unstaged(&instance, "a/f").await;
            let state = staged_tree(&instance).await;
            let moved = node_of(&instance, &state, held[source]).await;
            assert!(
                !moved.is_staged() && moved.is_dirty(),
                "{case}: the edit is unstaged, flags {:#x}",
                moved.flags
            );
            let deleted = node_of(&instance, &state, held["a/f"]).await;
            assert!(
                deleted.is_staged_delete(),
                "{case}: the deletion stays staged, flags {:#x}",
                deleted.flags
            );
        })
        .await;
    }
    assert_eq!(orders.lock().len(), 2, "both chain orders");
}

/// A path whose last name a directory holds twice, as a directory staged for deletion and one
/// `lore dirty move` took there with an edit in it staged, names the live directory: a lookup
/// answers it, and an unstage of the path unstages the edit below it and leaves the deletion staged.
/// Whichever of the two the chain holds first, as `to_front` sets for a rename in place.
#[tokio::test]
async fn a_directory_name_held_twice_names_the_live_directory() {
    let orders = Arc::new(Mutex::new(BTreeSet::new()));
    for (source, front) in [("b", None), ("p/c", Some("p/a")), ("p/c", Some("p/c"))] {
        let orders = orders.clone();
        with_instance(|instance| async move {
            let first = committed(
                &instance,
                &["p/a/x.txt", "p/c/y.txt", "b/y.txt", "other.txt"],
            )
            .await;
            let held = tree_paths(&instance, &first).await;
            if let Some(front) = front {
                to_front(&instance, front).await;
                without_front(&instance).await;
            }
            removed_directory(&instance, "p/a");
            stage_path(&instance, "p/a").await;
            dirty_moved(&instance, source, "p/a").await;
            restaged(&instance, "p/a/y.txt").await;
            let case = format!("{source} onto p/a, {front:?} at the front");

            let state = staged_tree(&instance).await;
            let ahead = lies_ahead(&instance, &state, held["p"], (held[source], held["p/a"])).await;
            orders.lock().insert(ahead);
            assert_eq!(
                node_id_at(&instance, &state, "p/a").await,
                held[source],
                "{case}: p/a names the live directory"
            );
            let edited = held[&format!("{source}/y.txt")];
            unstaged(&instance, "p/a").await;
            let state = staged_tree(&instance).await;
            let file = node_of(&instance, &state, edited).await;
            assert!(
                !file.is_staged() && file.is_dirty(),
                "{case}: the edit is unstaged, flags {:#x}",
                file.flags
            );
            let deleted = node_of(&instance, &state, held["p/a"]).await;
            assert!(
                deleted.is_staged_delete(),
                "{case}: the deletion stays staged, flags {:#x}",
                deleted.flags
            );
        })
        .await;
    }
    assert_eq!(orders.lock().len(), 2, "both chain orders");
}

/// A stage of an edit inside a directory `lore dirty move` renamed onto the name of a directory
/// staged for deletion stages the renamed directory's whole move, the live one holding the name,
/// whichever of the two its parent lists first.
#[tokio::test]
async fn a_stage_inside_a_directory_moved_onto_the_name_of_a_deleted_one_stages_its_whole_move() {
    let orders = Arc::new(Mutex::new(BTreeSet::new()));
    for directories in [["p", "q"], ["q", "p"]] {
        let orders = orders.clone();
        with_instance(|instance| async move {
            let first = committed(&instance, &["p/f", "q/g", "q/h", "other.txt"]).await;
            let held = tree_paths(&instance, &first).await;
            put_first(&instance, directories[0]).await;
            removed_directory(&instance, "p");
            stage_path(&instance, "p").await;
            let state = dirty_moved(&instance, "q", "p").await;
            let ahead = lies_ahead(&instance, &state, ROOT_NODE, (held["q"], held["p"])).await;
            orders.lock().insert(ahead);
            let state = restaged(&instance, "p/g").await;

            let moved = node_of(&instance, &state, held["q"]).await;
            assert!(moved.is_staged_move(), "q, flags {:#x}", moved.flags);
            let beside = node_of(&instance, &state, held["q/h"]).await;
            assert!(
                beside.is_staged_move() && !beside.is_dirty_move(),
                "{directories:?}: q/h, flags {:#x}",
                beside.flags
            );
        })
        .await;
    }
    assert_eq!(orders.lock().len(), 2, "both chain orders");
}

/// A path through a name a directory staged for deletion and a directory `lore dirty move` renamed
/// onto it both carry resolves through the live one, whichever of the two its parent lists first.
#[tokio::test]
async fn a_path_through_a_name_held_twice_resolves_through_the_live_directory() {
    let orders = Arc::new(Mutex::new(BTreeSet::new()));
    for directories in [["p", "q"], ["q", "p"]] {
        let orders = orders.clone();
        with_instance(|instance| async move {
            let first = committed(&instance, &["p/f", "q/g", "other.txt"]).await;
            let held = tree_paths(&instance, &first).await;
            put_first(&instance, directories[0]).await;
            removed_directory(&instance, "p");
            stage_path(&instance, "p").await;
            let state = dirty_moved(&instance, "q", "p").await;
            let ahead = lies_ahead(&instance, &state, ROOT_NODE, (held["q"], held["p"])).await;
            orders.lock().insert(ahead);

            assert_eq!(
                node_id_at(&instance, &state, "p/g").await,
                held["q/g"],
                "{directories:?}"
            );
        })
        .await;
    }
    assert_eq!(orders.lock().len(), 2, "both chain orders");
}
