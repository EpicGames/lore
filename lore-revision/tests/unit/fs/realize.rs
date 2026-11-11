// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_revision::change;
use lore_revision::change::FileAction;
use lore_revision::change::NodeChange;
use lore_revision::change::NodeChangeState;
use lore_revision::filter::Filter;
use lore_revision::filter::FilterMode;
use lore_revision::fs::filesystem_provider::InstanceOperation;
use lore_revision::fs::filesystem_provider::test_util::TestIdentity;
use lore_revision::fs::realize::*;
use lore_revision::node::Node;
use lore_revision::node::NodeFileMode;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision::sync::SyncError;
use lore_revision::revision::sync::SyncRealizeStats;
use lore_revision::revision::sync::SyncVerifyArgs;
use lore_revision::state;
use lore_revision::state::Mark;
use lore_revision::state::NodeMapping;
use lore_revision::state::State;
use lore_revision::util;
use lore_revision::util::path::RelativePath;
use lore_revision::util::path::RelativePathBuf;
use lore_storage::hash;
use zerocopy::FromZeros;

use crate::fs::filesystem_provider::TestFilesystemProvider;
use crate::fs::filesystem_provider::test_repository_over;
use crate::fs::filesystem_provider::test_store_create;
use crate::helper::in_memory::directory;
use crate::helper::in_memory::file;
use crate::repository::test_helpers::file_node;
use crate::repository::test_helpers::stored_as_list;
use crate::repository::test_helpers::working_tree_repository;
use crate::repository::test_helpers::working_tree_repository_filtered;

async fn with_execution<F: Future>(body: F) -> F::Output {
    let execution = Arc::new(lore_revision::interface::ExecutionContext::new_client(
        lore_revision::interface::LoreGlobalArgs::default(),
        lore_revision::relay::EventDispatcher::no_dispatch(),
    ));
    lore_base::runtime::LORE_CONTEXT
        .scope(execution, body)
        .await
}

fn pseudo_random_bytes(length: usize, salt: usize) -> Vec<u8> {
    (0..length)
        .map(|index| (index.wrapping_add(salt).wrapping_mul(2_654_435_761) >> 11) as u8)
        .collect()
}

/// One side of a change: the state holding a node at a path, and what it addresses.
struct Staged {
    state: Arc<State>,
    node: NodeID,
    address: lore_base::types::Address,
    mode: u16,
}

/// A state holding `node` at `path`, below the directories `path` names, under a revision of its
/// own.
async fn state_holding(
    repository: &Arc<RepositoryContext>,
    path: &RelativePath,
    node: Node,
    revision: u8,
) -> Staged {
    let state = State::new();
    state.set_revision(lore_base::types::Hash::from([revision; 32]));
    let mut directory = RelativePathBuf::new();
    for name in path
        .parent()
        .into_iter()
        .flat_map(|parent| parent.split('/'))
    {
        directory.push(name);
        lore_revision::stage::stage_single_node(
            repository.clone(),
            state.clone(),
            directory.clone().freeze(),
            Node::new_zeroed(),
            Arc::default(),
            None,
            FilterMode::Full,
        )
        .await
        .expect("stage the directory");
    }
    let link = lore_revision::stage::stage_single_node(
        repository.clone(),
        state.clone(),
        path.clone(),
        node,
        Arc::default(),
        None,
        FilterMode::Full,
    )
    .await
    .expect("stage the node");
    Staged {
        state,
        node: link.node,
        address: node.address,
        mode: node.mode,
    }
}

async fn write_working_file(
    repository: &Arc<RepositoryContext>,
    path: &RelativePath,
    content: &[u8],
) {
    lore_io::IoDriver::global()
        .write_file_bytes(
            path.to_absolute_path(repository.require_path().expect("working tree"))
                .expect("a path below the root"),
            bytes::Bytes::copy_from_slice(content),
            false,
        )
        .await
        .expect("write working file");
}

fn side(
    repository: &Arc<RepositoryContext>,
    staged: &Staged,
    path: RelativePath,
) -> NodeChangeState {
    NodeChangeState {
        mapping: NodeMapping {
            repository: repository.clone(),
            state: staged.state.clone(),
            path,
            node: staged.node,
        },
        observed: None,
        flags: NodeFlags::File,
        address: staged.address,
        mode: staged.mode,
    }
}

/// Verify one merge change, whose from side is the base revision, against the
/// working tree at `state_current`, answering the change where it is to be realized.
async fn verify(
    repository: &Arc<RepositoryContext>,
    path: &RelativePath,
    base: &Staged,
    source: &Staged,
    current: &Staged,
) -> Result<Option<NodeChange>, SyncError> {
    let (verdict, change) = Box::pin(verify_action(
        repository,
        path,
        base,
        source,
        current,
        FileAction::Keep,
    ))
    .await?;
    Ok((verdict == Verdict::Realize).then_some(change))
}

/// [`verify`] for a change of a given action, answering the verdict with the change.
async fn verify_action(
    repository: &Arc<RepositoryContext>,
    path: &RelativePath,
    base: &Staged,
    source: &Staged,
    current: &Staged,
    action: FileAction,
) -> Result<(Verdict, NodeChange), SyncError> {
    let operation = repository
        .file_system()
        .begin_operation()
        .await
        .expect("filesystem operation");
    let mut change = NodeChange {
        action,
        flags: change::Flags::None,
        from: side(repository, base, path.clone()),
        to: side(repository, source, path.clone()),
    };

    let verdict = Box::pin(verify_filesystem(
        &mut change,
        repository.clone(),
        operation,
        NodeMapping::root(repository.clone(), current.state.clone()),
        false,
        false,
        false,
        Arc::default(),
        FilterMode::Full,
        &IncomingMoves::new(Arc::default()),
    ))
    .await?;

    Ok((verdict, change))
}

/// The changes a sync still has to realize of the one change of the file at `path` from `base` to
/// `source`, verified as a sync verifies its change set over a working tree on `current`.
async fn verify_as_a_sync(
    repository: &Arc<RepositoryContext>,
    path: &RelativePath,
    base: &Staged,
    source: &Staged,
    current: &Staged,
) -> Result<Arc<Vec<NodeChange>>, SyncError> {
    let operation = repository
        .file_system()
        .begin_operation()
        .await
        .expect("filesystem operation");
    let change = NodeChange {
        action: FileAction::Keep,
        flags: change::Flags::None,
        from: side(repository, base, path.clone()),
        to: side(repository, source, path.clone()),
    };
    Box::pin(verify_filesystem_for_changes(Arc::new(SyncVerifyArgs {
        changes: Arc::new(vec![change]),
        repository_current: repository.clone(),
        operation,
        current: NodeMapping::root(repository.clone(), current.state.clone()),
        options: Arc::default(),
        carries_modes: true,
    })))
    .await
}

/// Record that the working file holds the current revision's content, which is what a
/// sync or a switch leaves behind.
async fn record_time(repository: &Arc<RepositoryContext>, path: &RelativePath) {
    let operation = repository
        .file_system()
        .begin_operation()
        .await
        .expect("filesystem operation");
    let info = operation.file_info(path).await.expect("file info");
    state::file_modified_time_store(repository.clone(), path, info.mtime()).await;
}

/// Adds what `make` makes of `name` below `parent`, clean.
async fn add_clean(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    parent: NodeID,
    (make, name): (fn(&str) -> Node, &str),
) -> NodeID {
    state
        .node_add(repository.clone(), parent, make(name), name)
        .await
        .expect("the node adds")
}

/// A state holding `dir/keep.txt`, `dir/a.txt` and `other/`, clean, and `dir`'s id.
async fn dir_and_other(repository: &Arc<RepositoryContext>) -> (Arc<State>, NodeID) {
    let state = State::new();
    let dir = add_clean(repository, &state, ROOT_NODE, (directory, "dir")).await;
    add_clean(repository, &state, dir, (file, "keep.txt")).await;
    add_clean(repository, &state, dir, (file, "a.txt")).await;
    add_clean(repository, &state, ROOT_NODE, (directory, "other")).await;
    (state, dir)
}

/// The realized move of `dir/a.txt` to `other/a.txt` in `state`.
async fn realize_move_of_a(repository: &Arc<RepositoryContext>, state: &Arc<State>) {
    let relative = |path: &str| RelativePath::new_from_initial_path(path).expect("the path");
    let from = state
        .find_node_link(repository.clone(), "dir/a.txt")
        .await
        .expect("dir/a.txt")
        .node;
    relink_moved_node(
        state,
        repository,
        &relative("dir/a.txt"),
        from,
        &relative("other/a.txt"),
        false,
    )
    .await
    .expect("the realized move");
}

/// A realized move of a file out of a directory stages the directory it left: its children
/// changed, so the commit rehashes it, which it skips for a directory not staged.
#[tokio::test]
async fn a_realized_move_out_of_a_directory_stages_the_one_it_left() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let (state, from) = dir_and_other(&repository).await;

        realize_move_of_a(&repository, &state).await;

        let left = state.node(repository.clone(), from).await.expect("dir");
        assert!(left.is_staged(), "flags {:#x}", left.flags);
    }))
    .await;
}

/// A realized move of a file out of a directory staged as moved keeps that directory's move: the
/// mark of the directory it left sets the base bit alone, where it is not set.
#[tokio::test]
async fn a_realized_move_out_of_a_moved_directory_keeps_its_move() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let (state, from) = dir_and_other(&repository).await;
        state
            .node_mark(&repository, from, Mark::staged(NodeFlags::StagedMove, true))
            .await
            .expect("dir staged as moved");

        realize_move_of_a(&repository, &state).await;

        let left = state.node(repository.clone(), from).await.expect("dir");
        assert!(left.is_staged_move(), "flags {:#x}", left.flags);
    }))
    .await;
}

/// A scratch write reaches the filesystem directly, so a path in the tracked tree has to
/// be refused: the provider may be virtualizing what is under the root, and a direct
/// write would go behind it.
///
/// The node is empty so that it is written without reading the store, which leaves the
/// refusal as the only thing that can fail the call.
#[tokio::test]
async fn a_scratch_path_inside_the_repository_is_refused() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;

        let inside = dir.path().join("inside.bin");
        assert!(
            realize_scratch_file(repository.clone(), &inside, file_node(&[]), Arc::default())
                .await
                .is_err(),
            "A path under the repository root has to go through an operation"
        );
        assert!(!inside.exists(), "The refused path must not be written");

        let outside = util::fs::generate_temppath("outside");
        assert!(
            !is_inside_repository(&repository, &outside),
            "A generated scratch path is outside the tree the repository tracks"
        );
    }))
    .await;
}

/// A file the branch never touched, holding what the current revision says it should.
/// The merge has to be free to overwrite it.
#[tokio::test]
async fn a_clean_file_is_overwritten() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let path = RelativePathBuf::new().push_and_freeze("clean.bin");
        let base_content = pseudo_random_bytes(20 * 1024, 0);
        let source_content = pseudo_random_bytes(20 * 1024, 1);
        write_working_file(&repository, &path, &base_content).await;

        let base = state_holding(&repository, &path, file_node(&base_content), 1).await;
        let source = state_holding(&repository, &path, file_node(&source_content), 2).await;
        let current = state_holding(&repository, &path, file_node(&base_content), 3).await;

        assert!(
            Box::pin(verify(&repository, &path, &base, &source, &current))
                .await
                .expect("a clean file is not a failure")
                .is_some(),
            "The change has to reach realize"
        );
    }))
    .await;
}

/// A file reset to an earlier revision holds neither the current revision's content nor
/// the incoming content, and is still content the change accounts for.
#[tokio::test]
async fn a_file_holding_the_replaced_content_is_realized() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let path = RelativePathBuf::new().push_and_freeze("reset.bin");
        let base_content = pseudo_random_bytes(20 * 1024, 0);
        let source_content = pseudo_random_bytes(20 * 1024, 1);
        let current_content = pseudo_random_bytes(20 * 1024, 2);
        write_working_file(&repository, &path, &base_content).await;

        let base = state_holding(&repository, &path, file_node(&base_content), 1).await;
        let source = state_holding(&repository, &path, file_node(&source_content), 2).await;
        let current = state_holding(&repository, &path, file_node(&current_content), 3).await;

        assert!(
            Box::pin(verify(&repository, &path, &base, &source, &current))
                .await
                .expect("content the change replaces is not a failure")
                .is_some(),
            "The file holds what the change replaces, so the change reaches realize"
        );
    }))
    .await;
}

/// A file that holds neither the current revision's content nor the incoming content
/// is local work, and the merge has to refuse it.
#[tokio::test]
async fn a_locally_edited_file_is_refused() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let path = RelativePathBuf::new().push_and_freeze("edited.bin");
        let base_content = pseudo_random_bytes(20 * 1024, 0);
        let source_content = pseudo_random_bytes(20 * 1024, 1);
        write_working_file(&repository, &path, &pseudo_random_bytes(20 * 1024, 2)).await;

        let base = state_holding(&repository, &path, file_node(&base_content), 1).await;
        let source = state_holding(&repository, &path, file_node(&source_content), 2).await;
        let current = state_holding(&repository, &path, file_node(&base_content), 3).await;

        assert!(
            Box::pin(verify(&repository, &path, &base, &source, &current))
                .await
                .is_err(),
            "Local work must not be overwritten"
        );
    }))
    .await;
}

/// A file already holding the incoming content has nothing to realize.
#[tokio::test]
async fn a_file_already_holding_the_incoming_content_is_dropped() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let path = RelativePathBuf::new().push_and_freeze("incoming.bin");
        let base_content = pseudo_random_bytes(20 * 1024, 0);
        let source_content = pseudo_random_bytes(20 * 1024, 1);
        write_working_file(&repository, &path, &source_content).await;

        let base = state_holding(&repository, &path, file_node(&base_content), 1).await;
        let source = state_holding(&repository, &path, file_node(&source_content), 2).await;
        let current = state_holding(&repository, &path, file_node(&base_content), 3).await;

        assert!(
            Box::pin(verify(&repository, &path, &base, &source, &current))
                .await
                .expect("a file at the incoming content is not a failure")
                .is_none()
        );
    }))
    .await;
}

/// A change the target branch made, which the working tree already holds. Realizing it
/// would rewrite the file with the bytes already in it.
#[tokio::test]
async fn a_target_side_change_the_tree_already_holds_is_dropped() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let path = RelativePathBuf::new().push_and_freeze("target-side.bin");
        let base_content = pseudo_random_bytes(20 * 1024, 0);
        let target_content = pseudo_random_bytes(20 * 1024, 1);
        write_working_file(&repository, &path, &target_content).await;

        let base = state_holding(&repository, &path, file_node(&base_content), 1).await;
        let current = state_holding(&repository, &path, file_node(&target_content), 3).await;

        assert!(
            Box::pin(verify(&repository, &path, &base, &current, &current))
                .await
                .expect("a file at the target content is not a failure")
                .is_none()
        );
    }))
    .await;
}

/// The executable bit the working tree holds at `path`.
#[cfg(target_family = "unix")]
async fn working_executable(repository: &Arc<RepositoryContext>, path: &RelativePath) -> bool {
    let absolute = path
        .to_absolute_path(repository.require_path().expect("working tree"))
        .expect("a path below the root");
    let metadata = lore_io::IoDriver::global()
        .metadata(absolute)
        .await
        .expect("working file metadata");
    util::fs::file_is_executable(&metadata)
}

/// Marks the working file at `path` executable behind the repository's back, which is what a
/// user running `chmod +x` leaves: a bit no revision gave the file.
#[cfg(target_family = "unix")]
async fn make_working_executable(repository: &Arc<RepositoryContext>, path: &RelativePath) {
    let absolute = path
        .to_absolute_path(repository.require_path().expect("working tree"))
        .expect("a path below the root");
    util::fs::metadata_set_executable(&absolute, None, true).await;
}

/// A node addressing `content` and carrying the executable bit.
#[cfg(target_family = "unix")]
fn executable_node(content: &[u8]) -> Node {
    let mut node = file_node(content);
    node.mode = NodeFileMode::Executable.bits();
    node
}

/// A change that moves the executable bit alone has the bit carried by the verify that drops it,
/// once the change set has passed: the content is in place, so writing the file would replace it
/// with the bytes it already holds. The verify of the one change answers the bit as still to carry.
#[cfg(target_family = "unix")]
#[tokio::test]
async fn a_mode_change_over_the_current_content_is_carried() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let path = RelativePathBuf::new().push_and_freeze("current.sh");
        let content = pseudo_random_bytes(20 * 1024, 0);
        write_working_file(&repository, &path, &content).await;

        let base = state_holding(&repository, &path, file_node(&content), 1).await;
        let source = state_holding(&repository, &path, executable_node(&content), 2).await;
        let current = state_holding(&repository, &path, file_node(&content), 3).await;

        let (verdict, _) = Box::pin(verify_action(
            &repository,
            &path,
            &base,
            &source,
            &current,
            FileAction::Keep,
        ))
        .await
        .expect("a mode change over matching content is not a failure");
        assert_eq!(verdict, Verdict::CarryMode);
        assert!(!working_executable(&repository, &path).await);
        assert!(
            Box::pin(verify_as_a_sync(
                &repository,
                &path,
                &base,
                &source,
                &current
            ))
            .await
            .expect("a mode change over matching content is not a failure")
            .is_empty()
        );
        assert!(working_executable(&repository, &path).await);
    }))
    .await;
}

/// The same where the working tree ran ahead of the current revision to the incoming
/// content, which is the other place a change is dropped for holding what it carries.
#[cfg(target_family = "unix")]
#[tokio::test]
async fn a_mode_change_over_the_incoming_content_is_carried() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let path = RelativePathBuf::new().push_and_freeze("incoming.sh");
        let base_content = pseudo_random_bytes(20 * 1024, 0);
        let source_content = pseudo_random_bytes(20 * 1024, 1);
        write_working_file(&repository, &path, &source_content).await;

        let base = state_holding(&repository, &path, file_node(&base_content), 1).await;
        let source = state_holding(&repository, &path, executable_node(&source_content), 2).await;
        let current = state_holding(&repository, &path, file_node(&base_content), 3).await;

        let (verdict, _) = Box::pin(verify_action(
            &repository,
            &path,
            &base,
            &source,
            &current,
            FileAction::Keep,
        ))
        .await
        .expect("a mode change over the incoming content is not a failure");
        assert_eq!(verdict, Verdict::CarryMode);
        assert!(!working_executable(&repository, &path).await);
        assert!(
            Box::pin(verify_as_a_sync(
                &repository,
                &path,
                &base,
                &source,
                &current
            ))
            .await
            .expect("a mode change over the incoming content is not a failure")
            .is_empty()
        );
        assert!(working_executable(&repository, &path).await);
    }))
    .await;
}

/// A bit the user set is a modification of the file in its own right, and one the content an
/// incoming revision carries answers nothing about: the change is realized rather than
/// refused, and marked so that writing the content leaves the bit standing.
#[cfg(target_family = "unix")]
#[tokio::test]
async fn a_local_mode_is_kept_over_an_incoming_content_change() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let path = RelativePathBuf::new().push_and_freeze("chmodded.sh");
        let base_content = pseudo_random_bytes(20 * 1024, 0);
        let source_content = pseudo_random_bytes(20 * 1024, 1);
        write_working_file(&repository, &path, &base_content).await;
        make_working_executable(&repository, &path).await;

        let base = state_holding(&repository, &path, file_node(&base_content), 1).await;
        let source = state_holding(&repository, &path, file_node(&source_content), 2).await;
        let current = state_holding(&repository, &path, file_node(&base_content), 3).await;

        let change = Box::pin(verify(&repository, &path, &base, &source, &current))
            .await
            .expect("a local mode must not hold back the content a change carries")
            .expect("the change has to reach realize");
        assert!(
            change.flags.is_local_mode(),
            "the write has to be told to keep the bit the working tree holds"
        );
        assert!(
            working_executable(&repository, &path).await,
            "the verify leaves the bit as the user set it"
        );
    }))
    .await;
}

/// A change whose content the working tree already holds has only a mode to apply, and the
/// one it names is the revision's rather than the working tree's. The bit the user set stands
/// rather than being reverted to it.
#[cfg(target_family = "unix")]
#[tokio::test]
async fn a_local_mode_is_not_reverted_by_a_change_carrying_the_same_content() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let path = RelativePathBuf::new().push_and_freeze("standing.sh");
        let content = pseudo_random_bytes(20 * 1024, 0);
        write_working_file(&repository, &path, &content).await;
        make_working_executable(&repository, &path).await;

        let base = state_holding(&repository, &path, file_node(&content), 1).await;
        let source = state_holding(&repository, &path, file_node(&content), 2).await;
        let current = state_holding(&repository, &path, file_node(&content), 3).await;

        assert!(
            Box::pin(verify(&repository, &path, &base, &source, &current))
                .await
                .expect("a local mode is not a failure")
                .is_none(),
            "the content is in place, so the change has nothing left to write"
        );
        assert!(
            working_executable(&repository, &path).await,
            "the bit the user set is not reverted to the one the revision holds"
        );
    }))
    .await;
}

/// The same where the working tree ran ahead to the incoming content, which is the other
/// place a change is dropped for holding what it carries.
#[cfg(target_family = "unix")]
#[tokio::test]
async fn a_local_mode_is_not_reverted_over_the_incoming_content() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let path = RelativePathBuf::new().push_and_freeze("ahead.sh");
        let base_content = pseudo_random_bytes(20 * 1024, 0);
        let source_content = pseudo_random_bytes(20 * 1024, 1);
        write_working_file(&repository, &path, &source_content).await;
        make_working_executable(&repository, &path).await;

        let base = state_holding(&repository, &path, file_node(&base_content), 1).await;
        let source = state_holding(&repository, &path, file_node(&source_content), 2).await;
        let current = state_holding(&repository, &path, file_node(&base_content), 3).await;

        assert!(
            Box::pin(verify(&repository, &path, &base, &source, &current))
                .await
                .expect("a local mode is not a failure")
                .is_none(),
            "the incoming content is in place, so the change has nothing left to write"
        );
        assert!(
            working_executable(&repository, &path).await,
            "the bit the user set is not reverted to the one the revision holds"
        );
    }))
    .await;
}

/// A leaf length this build never cuts a fragment list at, as a client of another version left
/// one.
const FOREIGN_LEAF_LEN: usize = 17 * 1024;

/// The reported shape: the current revision addresses the file as a fragment list some
/// other version of the client cut, and the file is untouched. The list is what answers
/// for it, and the merge has to be free to overwrite it.
#[tokio::test]
async fn a_clean_file_addressed_as_a_list_is_overwritten() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let path = RelativePathBuf::new().push_and_freeze("listed.bin");
        let content = pseudo_random_bytes(150 * 1024, 0);
        let source_content = pseudo_random_bytes(150 * 1024, 1);
        write_working_file(&repository, &path, &content).await;

        let listed = stored_as_list(&repository, &content, FOREIGN_LEAF_LEN).await;
        assert_ne!(
            listed.address.hash,
            hash::hash_slice(&content),
            "The node has to address a list for this to be the case under test"
        );

        let base = state_holding(&repository, &path, listed, 1).await;
        let source = state_holding(&repository, &path, file_node(&source_content), 2).await;
        let current = state_holding(&repository, &path, listed, 3).await;

        assert!(
            Box::pin(verify(&repository, &path, &base, &source, &current))
                .await
                .expect("a clean file is not a failure")
                .is_some(),
            "The stored list answers for the file, so the change reaches realize"
        );
    }))
    .await;
}

/// A change that starts at the current revision measures against the node the from side
/// names, reached by id rather than by path.
#[tokio::test]
async fn a_change_starting_at_the_current_revision_is_measured_by_its_own_node() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let path = RelativePathBuf::new().push_and_freeze("from-current.bin");
        let content = pseudo_random_bytes(20 * 1024, 0);
        let incoming = pseudo_random_bytes(20 * 1024, 1);
        write_working_file(&repository, &path, &content).await;

        let current = state_holding(&repository, &path, file_node(&content), 1).await;
        let source = state_holding(&repository, &path, file_node(&incoming), 2).await;

        assert!(
            Box::pin(verify(&repository, &path, &current, &source, &current))
                .await
                .expect("a clean file is not a failure")
                .is_some()
        );
    }))
    .await;
}

/// A path the current revision holds no node at is measured against the from side. That
/// node is not the current revision's, so the file is realized rather than dropped and
/// nothing is recorded against a revision that never held it.
#[tokio::test]
async fn a_path_the_current_revision_does_not_hold_is_realized() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let path = RelativePathBuf::new().push_and_freeze("untracked.bin");
        let elsewhere = RelativePathBuf::new().push_and_freeze("elsewhere.bin");
        let content = pseudo_random_bytes(20 * 1024, 0);
        write_working_file(&repository, &path, &content).await;

        let base = state_holding(&repository, &path, file_node(&content), 1).await;
        let source = state_holding(&repository, &path, file_node(&content), 2).await;
        let current = state_holding(&repository, &elsewhere, file_node(&content), 3).await;

        assert!(
            Box::pin(verify(&repository, &path, &base, &source, &current))
                .await
                .expect("a readable file is not a failure")
                .is_some(),
            "The from side is not the current revision's node, so nothing may be dropped"
        );
    }))
    .await;
}

/// A rename carries a source to remove and a destination to create, which equal content
/// says nothing about, so it is realized however the addresses compare.
#[tokio::test]
async fn a_move_of_unchanged_content_is_realized() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let path = RelativePathBuf::new().push_and_freeze("moved.bin");
        let content = pseudo_random_bytes(20 * 1024, 0);
        write_working_file(&repository, &path, &content).await;

        let node = file_node(&content);
        let base = state_holding(&repository, &path, node, 1).await;
        let source = state_holding(&repository, &path, node, 2).await;
        let current = state_holding(&repository, &path, node, 3).await;

        assert_eq!(
            Box::pin(verify_action(
                &repository,
                &path,
                &base,
                &source,
                &current,
                FileAction::Move
            ))
            .await
            .expect("a clean file is not a failure")
            .0,
            Verdict::Realize,
            "A move must reach realize even where the content is already in place"
        );
    }))
    .await;
}

/// How a verified change relocates a file: its action and the paths it carries the file from and
/// to, and what the verify is asked to do with local work.
struct Relocation {
    action: FileAction,
    from: &'static str,
    to: &'static str,
    /// Keep local work rather than refuse it.
    forward_changes: bool,
    /// The caller realizes every change, whatever the verify answers, as a merge does.
    realizes_every_change: bool,
    /// The change starts at a base revision, as a three-way merge's does, and the current
    /// revision holds the file at the destination alone, as the current branch's own move leaves
    /// it.
    current_at_destination: bool,
}

/// A move of `from.bin` to `to.bin`.
const MOVE: Relocation = Relocation {
    action: FileAction::Move,
    from: "from.bin",
    to: "to.bin",
    forward_changes: false,
    realizes_every_change: false,
    current_at_destination: false,
};

/// What the verify answers for `relocation`, starting at the current revision, which holds
/// `realized` at the source, and bringing in `incoming`, where the working tree holds `working`
/// at the source, or nothing there for `None`. A relocation from a base revision starts there
/// instead, with the current revision and the working tree holding `incoming` at the destination.
async fn verify_relocation(
    relocation: Relocation,
    working: Option<&[u8]>,
    realized: &[u8],
    incoming: &[u8],
) -> Result<Verdict, SyncError> {
    let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
    let repository = working_tree_repository(dir.path()).await;
    let (from, to) = (relative(relocation.from), relative(relocation.to));
    if let Some(working) = working {
        if let Some(parent) = from.parent() {
            std::fs::create_dir_all(dir.path().join(parent)).expect("the source's directory");
        }
        write_working_file(&repository, &from, working).await;
    }
    let start = state_holding(&repository, &from, file_node(realized), 1).await;
    let target = state_holding(&repository, &to, file_node(incoming), 2).await;
    let current = if relocation.current_at_destination {
        write_working_file(&repository, &to, incoming).await;
        &state_holding(&repository, &to, file_node(incoming), 3).await
    } else {
        &start
    };
    let operation = repository
        .file_system()
        .begin_operation()
        .await
        .expect("filesystem operation");
    let mut change = NodeChange {
        action: relocation.action,
        flags: change::Flags::None,
        from: side(&repository, &start, from),
        to: side(&repository, &target, to),
    };
    Box::pin(verify_filesystem(
        &mut change,
        repository.clone(),
        operation,
        NodeMapping::root(repository.clone(), current.state.clone()),
        relocation.forward_changes,
        relocation.realizes_every_change,
        false,
        Arc::default(),
        FilterMode::Full,
        &IncomingMoves::new(Arc::default()),
    ))
    .await
}

/// An edit at the path a move carries a file from is local work, whether or not the move
/// changes the content, and the move is refused.
#[tokio::test]
async fn a_move_of_a_locally_edited_file_is_refused() {
    Box::pin(with_execution(async {
        let realized = pseudo_random_bytes(20 * 1024, 0);
        let edited = pseudo_random_bytes(20 * 1024, 1);
        for incoming in [realized.clone(), pseudo_random_bytes(20 * 1024, 2)] {
            let verified = verify_relocation(MOVE, Some(&edited), &realized, &incoming).await;
            assert!(
                is_local_modifications(&verified),
                "an edit the move carries away must refuse it, got {verified:?}"
            );
        }
    }))
    .await;
}

/// A source holding what it was realized from, the content the move brings in, or nothing at
/// all holds no local work, and the move is realized.
#[tokio::test]
async fn a_move_of_a_file_holding_no_local_work_is_realized() {
    Box::pin(with_execution(async {
        let realized = pseudo_random_bytes(20 * 1024, 0);
        let incoming = pseudo_random_bytes(20 * 1024, 1);
        for working in [Some(&realized[..]), Some(&incoming[..]), None] {
            assert_eq!(
                verify_relocation(MOVE, working, &realized, &incoming)
                    .await
                    .expect("a source holding no local work is not a failure"),
                Verdict::Realize,
                "the move must reach realize"
            );
        }
    }))
    .await;
}

/// Keeping local changes keeps an edit a move would carry away, and leaves the move unrealized.
#[tokio::test]
async fn a_move_of_a_locally_edited_file_is_dropped_when_keeping_local_changes() {
    Box::pin(with_execution(async {
        let realized = pseudo_random_bytes(20 * 1024, 0);
        let edited = pseudo_random_bytes(20 * 1024, 1);
        let incoming = pseudo_random_bytes(20 * 1024, 2);
        let keeping = Relocation {
            forward_changes: true,
            ..MOVE
        };
        assert_eq!(
            verify_relocation(keeping, Some(&edited), &realized, &incoming)
                .await
                .expect("an edit kept is not a failure"),
            Verdict::Drop,
            "the move must not reach realize"
        );
    }))
    .await;
}

/// A caller that realizes every change it verifies cannot keep an edit a move would carry away,
/// so the move is refused even where keeping local changes is asked for.
#[tokio::test]
async fn a_move_of_a_locally_edited_file_is_refused_where_every_change_is_realized() {
    Box::pin(with_execution(async {
        let realized = pseudo_random_bytes(20 * 1024, 0);
        let edited = pseudo_random_bytes(20 * 1024, 1);
        let incoming = pseudo_random_bytes(20 * 1024, 2);
        let realizing = Relocation {
            forward_changes: true,
            realizes_every_change: true,
            ..MOVE
        };
        let verified = verify_relocation(realizing, Some(&edited), &realized, &incoming).await;
        assert!(
            is_local_modifications(&verified),
            "an edit the move carries away must refuse it, got {verified:?}"
        );
    }))
    .await;
}

/// A file below a directory a move renamed is carried from the path it stood at, so an edit
/// there is local work the change is refused for.
#[tokio::test]
async fn a_change_below_a_renamed_directory_to_a_locally_edited_file_is_refused() {
    Box::pin(with_execution(async {
        let realized = pseudo_random_bytes(20 * 1024, 0);
        let edited = pseudo_random_bytes(20 * 1024, 1);
        let incoming = pseudo_random_bytes(20 * 1024, 2);
        let below_renamed = Relocation {
            action: FileAction::Keep,
            from: "dir/a.bin",
            to: "newdir/a.bin",
            ..MOVE
        };
        let verified = verify_relocation(below_renamed, Some(&edited), &realized, &incoming).await;
        assert!(
            is_local_modifications(&verified),
            "an edit below the renamed directory must refuse the change, got {verified:?}"
        );
    }))
    .await;
}

/// A file at the source of the current branch's own move, which a three-way merge verifies, is
/// none the current revision holds there, so the move carries nothing away from it and is
/// realized.
#[tokio::test]
async fn an_untracked_file_at_the_source_of_the_current_branch_s_move_holds_no_local_work() {
    Box::pin(with_execution(async {
        let realized = pseudo_random_bytes(20 * 1024, 0);
        let untracked = pseudo_random_bytes(20 * 1024, 1);
        let incoming = pseudo_random_bytes(20 * 1024, 2);
        let own_move = Relocation {
            current_at_destination: true,
            ..MOVE
        };
        let verified = verify_relocation(own_move, Some(&untracked), &realized, &incoming).await;
        assert!(
            matches!(verified, Ok(Verdict::Realize)),
            "an untracked file at the source must not refuse the move, got {verified:?}"
        );
    }))
    .await;
}

/// The directory the directory tests delete, and the file below it they track.
const PLUGIN: &str = "Plugin";
const SOURCE: &str = "Plugin/Source";
const TRACKED: &str = "Plugin/Source/Plugin.Build.cs";
const VARIANT: &str = "Plugin/Source/Plugin.build.cs";

fn relative(path: &str) -> RelativePath {
    RelativePath::new_from_initial_path(path).unwrap()
}

fn is_local_modifications<T>(result: &Result<T, SyncError>) -> bool {
    matches!(result, Err(SyncError::LocalModifications(_)))
}

/// A state holding `files`, below [`SOURCE`], with its directories staged before them, unfiltered as
/// a revision is.
async fn plugin_tree(repository: &Arc<RepositoryContext>, files: &[(&str, &[u8])]) -> Arc<State> {
    let state = State::new();
    state.set_revision(lore_base::types::Hash::from([1; 32]));
    let directories = [(PLUGIN, Node::new_zeroed()), (SOURCE, Node::new_zeroed())];
    let files = files
        .iter()
        .map(|(path, content)| (*path, file_node(content)));
    for (path, node) in directories.into_iter().chain(files) {
        lore_revision::stage::stage_single_node(
            repository.clone(),
            state.clone(),
            relative(path),
            node,
            Arc::default(),
            None,
            FilterMode::empty(),
        )
        .await
        .expect("stage the node");
    }
    state
}

/// Writes `files` into the working tree, creating their directories.
async fn write_working_files(repository: &Arc<RepositoryContext>, files: &[(&str, &[u8])]) {
    for (path, content) in files {
        let path = relative(path);
        let absolute = path
            .to_absolute_path(repository.require_path().unwrap())
            .expect("a path below the root");
        lore_io::IoDriver::global()
            .create_dir_all(absolute.parent().unwrap())
            .await
            .unwrap();
        write_working_file(repository, &path, content).await;
    }
}

async fn working_file(repository: &Arc<RepositoryContext>, path: &str) -> Vec<u8> {
    lore_io::IoDriver::global()
        .read_file_bytes(
            relative(path)
                .to_absolute_path(repository.require_path().unwrap())
                .expect("a path below the root"),
        )
        .await
        .unwrap()
        .to_vec()
}

async fn working_path_exists(repository: &Arc<RepositoryContext>, path: &str) -> bool {
    lore_io::IoDriver::global()
        .metadata(
            relative(path)
                .to_absolute_path(repository.require_path().unwrap())
                .expect("a path below the root"),
        )
        .await
        .is_ok()
}

/// The delete of `node`, which `state` holds at `path`.
async fn delete_of(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    path: &str,
    node: NodeID,
) -> NodeChange {
    let path = relative(path);
    let node_flags = state.node(repository.clone(), node).await.unwrap().flags;
    let from = NodeChangeState {
        mapping: NodeMapping {
            repository: repository.clone(),
            state: state.clone(),
            path: path.clone(),
            node,
        },
        observed: None,
        flags: NodeFlags::from_bits_retain(node_flags),
        address: Default::default(),
        mode: 0,
    };
    NodeChange {
        action: FileAction::Delete,
        flags: change::Flags::None,
        to: from.invalid(path),
        from,
    }
}

/// The delete of the node `state` holds at `path`.
async fn delete_change(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    path: &str,
) -> NodeChange {
    let node = state
        .find_node_link(repository.clone(), path)
        .await
        .unwrap()
        .node;
    delete_of(repository, state, path, node).await
}

/// Verifies `change` against the working tree standing on `state` as a sync does, `incoming`
/// holding the rest of the change set.
async fn verify_change(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    mut change: NodeChange,
    incoming: &[NodeChange],
) -> Result<Option<NodeChange>, SyncError> {
    let operation = repository.file_system().begin_operation().await.unwrap();
    let verdict = Box::pin(verify_filesystem(
        &mut change,
        repository.clone(),
        operation,
        NodeMapping::root(repository.clone(), state.clone()),
        false,
        false,
        false,
        Arc::default(),
        FilterMode::View,
        &IncomingMoves::new(Arc::new(incoming.to_vec())),
    ))
    .await?;
    Ok((verdict == Verdict::Realize).then_some(change))
}

async fn verify_delete(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    path: &str,
    incoming: &[NodeChange],
) -> Result<Option<NodeChange>, SyncError> {
    let change = delete_change(repository, state, path).await;
    Box::pin(verify_change(repository, state, change, incoming)).await
}

/// Realizes `changes`, verified first where `verify` asks for it as a sync does and a forced one
/// does not.
async fn realize_verified(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    changes: Vec<NodeChange>,
    verify: bool,
) -> Result<(), SyncError> {
    let mut realized = Vec::new();
    for change in changes {
        if !verify {
            realized.push(change);
        } else if let Some(change) = Box::pin(verify_change(repository, state, change, &[])).await?
        {
            realized.push(change);
        }
    }
    let operation = repository.file_system().begin_operation().await.unwrap();
    realize_changes(
        repository.clone(),
        operation,
        Arc::new(realized),
        None,
        false,
        false,
        Arc::default(),
    )
    .await
}

/// Deletes [`PLUGIN`] with [`SOURCE`] and the `files` `state` holds below it.
async fn delete_plugin(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    files: &[&str],
    verify: bool,
) -> Result<(), SyncError> {
    let mut changes = Vec::new();
    for path in [PLUGIN, SOURCE].iter().chain(files) {
        changes.push(delete_change(repository, state, path).await);
    }
    Box::pin(realize_verified(repository, state, changes, verify)).await
}

/// A working tree spelling a tracked name in another case holds the tree's own file where the
/// content agrees, which deleting its directory loses nothing of.
#[tokio::test]
async fn a_deleted_directory_holding_an_unmodified_case_variant_is_realized() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let content = pseudo_random_bytes(1174, 0);
        let state = plugin_tree(&repository, &[(TRACKED, &content)]).await;
        write_working_files(&repository, &[(VARIANT, &content)]).await;

        assert!(
            Box::pin(verify_delete(&repository, &state, PLUGIN, &[]))
                .await
                .expect("a case variant holding the tree's content is no local change")
                .is_some()
        );
    }))
    .await;
}

/// A directory spelled in another case is local work, which the delete of its directory refuses to
/// lose, leaving the working tree as it was.
#[tokio::test]
async fn a_deleted_directory_holding_a_case_variant_directory_is_refused() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let content = pseudo_random_bytes(1174, 0);
        let state = plugin_tree(&repository, &[(TRACKED, &content)]).await;
        let variant = "Plugin/source/Plugin.Build.cs";
        write_working_files(&repository, &[(variant, &content)]).await;

        assert!(is_local_modifications(
            &Box::pin(delete_plugin(&repository, &state, &[TRACKED], true)).await
        ));
        assert_eq!(working_file(&repository, variant).await, content);
    }))
    .await;
}

/// A case variant holding content neither revision holds is local work.
#[tokio::test]
async fn a_deleted_directory_holding_a_modified_case_variant_is_refused() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let state = plugin_tree(&repository, &[(TRACKED, &pseudo_random_bytes(1174, 0))]).await;
        write_working_files(&repository, &[(VARIANT, &pseudo_random_bytes(1174, 1))]).await;

        assert!(is_local_modifications(
            &Box::pin(verify_delete(&repository, &state, PLUGIN, &[])).await
        ));
    }))
    .await;
}

/// A file holding the content the incoming revision moves it out of the directory with matches
/// the target, so the delete loses nothing of it.
#[tokio::test]
async fn a_deleted_directory_holding_the_content_the_target_moves_away_is_realized() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let incoming_content = pseudo_random_bytes(1174, 1);
        let state = plugin_tree(&repository, &[(TRACKED, &pseudo_random_bytes(1174, 0))]).await;
        write_working_files(&repository, &[(TRACKED, &incoming_content)]).await;

        let destination = relative("Plugin.Build.cs");
        let moved = state_holding(&repository, &destination, file_node(&incoming_content), 2).await;
        let node = state
            .find_node_link(repository.clone(), TRACKED)
            .await
            .unwrap()
            .node;
        let base = Staged {
            state: state.clone(),
            node,
            address: file_node(&pseudo_random_bytes(1174, 0)).address,
            mode: 0,
        };
        let incoming = [NodeChange {
            action: FileAction::Move,
            flags: change::Flags::Modify,
            from: side(&repository, &base, relative(TRACKED)),
            to: side(&repository, &moved, destination),
        }];

        assert!(
            Box::pin(verify_delete(&repository, &state, PLUGIN, &incoming))
                .await
                .expect("a file holding the target's content is no local change")
                .is_some()
        );
        assert!(
            is_local_modifications(
                &Box::pin(verify_delete(&repository, &state, PLUGIN, &[])).await
            ),
            "Without the move the file holds content neither revision holds"
        );
    }))
    .await;
}

/// A file the view keeps out of the change set stays when its directory is deleted, though its
/// name folds to that of a file the tree holds there, verified or forced.
#[tokio::test]
async fn a_deleted_directory_keeps_a_case_variant_of_a_file_the_view_excludes() {
    Box::pin(with_execution(async {
        for verify in [true, false] {
            let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
            let mut filter = Filter::default();
            filter.view.add_exclusion("/Plugin/Secret.cfg").unwrap();
            let repository = working_tree_repository_filtered(dir.path(), filter).await;
            let other = "Plugin/other.txt";
            let state = plugin_tree(
                &repository,
                &[("Plugin/Secret.cfg", b"tracked secret"), (other, b"other")],
            )
            .await;
            let mine = "Plugin/secret.cfg";
            write_working_files(&repository, &[(other, b"other"), (mine, b"my own work")]).await;

            Box::pin(delete_plugin(&repository, &state, &[other], verify))
                .await
                .expect("the delete realizes");
            assert_eq!(
                working_file(&repository, mine).await,
                b"my own work",
                "The user's file stays, verified: {verify}"
            );
        }
    }))
    .await;
}

/// A directory holding the source of a move is removed once the move has carried it out,
/// verified or forced.
#[tokio::test]
async fn a_deleted_directory_a_move_empties_is_removed() {
    Box::pin(with_execution(async {
        for verify in [true, false] {
            let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
            let repository = working_tree_repository(dir.path()).await;
            let content = pseudo_random_bytes(1174, 0);
            let state = plugin_tree(&repository, &[(TRACKED, &content)]).await;
            write_working_files(&repository, &[(TRACKED, &content)]).await;

            let destination = "Plugin.Build.cs";
            let moved =
                state_holding(&repository, &relative(destination), file_node(&content), 2).await;
            let source = Staged {
                state: state.clone(),
                node: state
                    .find_node_link(repository.clone(), TRACKED)
                    .await
                    .unwrap()
                    .node,
                address: moved.address,
                mode: 0,
            };
            let changes = vec![
                delete_change(&repository, &state, PLUGIN).await,
                delete_change(&repository, &state, SOURCE).await,
                NodeChange {
                    action: FileAction::Move,
                    flags: change::Flags::None,
                    from: side(&repository, &source, relative(TRACKED)),
                    to: side(&repository, &moved, relative(destination)),
                },
            ];

            Box::pin(realize_verified(&repository, &state, changes, verify))
                .await
                .expect("the move realizes");
            assert_eq!(working_file(&repository, destination).await, content);
            assert!(
                !working_path_exists(&repository, PLUGIN).await,
                "The emptied directory goes, verified: {verify}"
            );
        }
    }))
    .await;
}

/// A second spelling beside the tracked file is judged against the node as the tracked one is.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_second_spelling_beside_the_tracked_file_is_judged_against_its_node() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let content = pseudo_random_bytes(1174, 0);
        let state = plugin_tree(&repository, &[(TRACKED, &content)]).await;
        write_working_files(&repository, &[(TRACKED, &content), (VARIANT, &content)]).await;

        Box::pin(delete_plugin(&repository, &state, &[TRACKED], true))
            .await
            .expect("a second spelling holding the tree's content is no local change");
        assert!(!working_path_exists(&repository, PLUGIN).await);

        write_working_files(
            &repository,
            &[
                (TRACKED, &content),
                (VARIANT, &pseudo_random_bytes(1174, 1)),
            ],
        )
        .await;
        assert!(
            is_local_modifications(
                &Box::pin(verify_delete(&repository, &state, PLUGIN, &[])).await
            ),
            "A second spelling holding other content is local work"
        );
    }))
    .await;
}

/// A case-sensitive file system keeps a case variant the delete of the tracked spelling misses,
/// which the directory delete then removes, verified or forced.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_deleted_directory_takes_an_unchanged_case_variant_with_it() {
    Box::pin(with_execution(async {
        for verify in [true, false] {
            let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
            let repository = working_tree_repository(dir.path()).await;
            let content = pseudo_random_bytes(1174, 0);
            let state = plugin_tree(&repository, &[(TRACKED, &content)]).await;
            write_working_files(&repository, &[(VARIANT, &content)]).await;

            Box::pin(delete_plugin(&repository, &state, &[TRACKED], verify))
                .await
                .expect("the delete realizes");
            assert!(
                !working_path_exists(&repository, PLUGIN).await,
                "The directory goes with the variant, verified: {verify}"
            );
        }
    }))
    .await;
}

/// An untracked file keeps the deleted directory, the case variant beside it going.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_deleted_directory_keeps_an_untracked_file_beside_a_case_variant() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let content = pseudo_random_bytes(1174, 0);
        let state = plugin_tree(&repository, &[(TRACKED, &content)]).await;
        let untracked = "Plugin/Source/notes.txt";
        write_working_files(&repository, &[(VARIANT, &content), (untracked, b"notes")]).await;

        Box::pin(delete_plugin(&repository, &state, &[TRACKED], false))
            .await
            .expect("the delete realizes");
        assert!(!working_path_exists(&repository, VARIANT).await);
        assert!(working_path_exists(&repository, untracked).await);
    }))
    .await;
}

/// Among many files the deleted directory holds, only the case variant of a file deleted directly
/// in it goes, not one folding to a file deleted in its parent.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_deleted_directory_removes_only_the_case_variants_among_its_files() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let content = pseudo_random_bytes(1174, 0);
        let readme = "Plugin/Readme.txt";
        let state = plugin_tree(&repository, &[(TRACKED, &content), (readme, b"readme")]).await;
        let unrelated: Vec<String> = (0..512)
            .map(|index| format!("{SOURCE}/unrelated-{index}.txt"))
            .collect();
        let parent_variant = "Plugin/Source/readme.TXT";
        let files: Vec<(&str, &[u8])> = unrelated
            .iter()
            .map(|path| (path.as_str(), b"mine".as_slice()))
            .chain([(VARIANT, content.as_slice()), (parent_variant, b"mine")])
            .collect();
        write_working_files(&repository, &files).await;

        Box::pin(delete_plugin(
            &repository,
            &state,
            &[TRACKED, readme],
            false,
        ))
        .await
        .expect("the delete realizes");
        assert!(!working_path_exists(&repository, VARIANT).await);
        assert!(working_path_exists(&repository, parent_variant).await);
        for path in &unrelated {
            assert!(working_path_exists(&repository, path).await, "{path} stays");
        }
    }))
    .await;
}

/// A directory named as a variant of a deleted file, and a file named as a variant of a deleted
/// directory, are no files the revision deletes.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_deleted_directory_keeps_entries_of_another_type_under_a_case_variant_name() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let content = pseudo_random_bytes(1174, 0);
        let readme = "Plugin/Source/Readme.txt";
        let state = plugin_tree(&repository, &[(TRACKED, &content), (readme, b"readme")]).await;
        let directory = "Plugin/Source/readme.TXT";
        let file = "Plugin/source";
        write_working_files(&repository, &[(VARIANT, &content), (file, b"mine")]).await;
        lore_io::IoDriver::global()
            .create_dir_all(
                relative(directory)
                    .to_absolute_path(dir.path())
                    .expect("a path below the root"),
            )
            .await
            .unwrap();

        Box::pin(delete_plugin(
            &repository,
            &state,
            &[TRACKED, readme],
            false,
        ))
        .await
        .expect("the delete realizes");
        assert!(!working_path_exists(&repository, VARIANT).await);
        assert!(working_path_exists(&repository, directory).await);
        assert_eq!(working_file(&repository, file).await, b"mine");
    }))
    .await;
}

/// The delete of a link takes the mount it names, not a directory spelled as a case variant of it.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_deleted_link_keeps_a_directory_under_a_case_variant_of_its_mount() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let work = "libs/shared/mywork.txt";
        write_working_files(&repository, &[(work, b"untracked work")]).await;
        let mount = relative("libs/Shared");
        let from = NodeChangeState {
            mapping: NodeMapping {
                repository: repository.clone(),
                state: State::new(),
                path: mount.clone(),
                node: NodeID::new_zeroed(),
            },
            observed: None,
            flags: NodeFlags::Link,
            address: Default::default(),
            mode: 0,
        };
        let deleted = NodeChange {
            action: FileAction::Delete,
            flags: change::Flags::None,
            to: from.invalid(mount),
            from,
        };

        Box::pin(realize_verified(
            &repository,
            &State::new(),
            vec![deleted],
            false,
        ))
        .await
        .expect("the delete realizes");
        assert_eq!(working_file(&repository, work).await, b"untracked work");
    }))
    .await;
}

/// Adds a file holding `content` at [`VARIANT`] to `state`, beside the one at [`TRACKED`].
#[cfg(target_os = "linux")]
async fn track_variant(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    content: &[u8],
) -> NodeID {
    let source = state
        .find_node_link(repository.clone(), SOURCE)
        .await
        .unwrap()
        .node;
    let name = "Plugin.build.cs";
    let mut node = file_node(content);
    node.name_hash = hash::hash_string(name);
    state
        .node_add(repository.clone(), source, node, name)
        .await
        .unwrap()
}

/// A tree holding two spellings of one name tracks the one on disk, which the delete of the
/// other leaves alone.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_delete_leaves_a_second_tracked_spelling_alone() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let content = pseudo_random_bytes(1174, 0);
        let state = plugin_tree(&repository, &[(TRACKED, &content)]).await;
        let deleted = delete_change(&repository, &state, TRACKED).await;
        track_variant(&repository, &state, &content).await;
        write_working_files(&repository, &[(VARIANT, &content)]).await;

        Box::pin(realize_verified(&repository, &state, vec![deleted], true))
            .await
            .expect("the delete realizes");
        assert!(working_path_exists(&repository, VARIANT).await);
    }))
    .await;
}

/// A file a move carries out of the deleted directory is judged against that move's target, not
/// against the target of a move from another spelling of its name, whichever spelling the working
/// tree holds and in either order of the moves.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_deleted_directory_judges_a_file_against_the_move_of_its_own_spelling() {
    Box::pin(with_execution(async {
        let tracked = pseudo_random_bytes(1174, 0);
        let variant = pseudo_random_bytes(1174, 1);
        let edited = pseudo_random_bytes(1174, 2);
        for edits_variant in [true, false] {
            let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
            let repository = working_tree_repository(dir.path()).await;
            let state = plugin_tree(&repository, &[(TRACKED, &tracked)]).await;
            let tracked_node = state
                .find_node_link(repository.clone(), TRACKED)
                .await
                .unwrap()
                .node;
            let variant_node = track_variant(&repository, &state, &variant).await;
            let tracked_move = (TRACKED, tracked_node, &tracked, "Tracked.cs");
            let variant_move = (VARIANT, variant_node, &variant, "Variant.cs");
            let (edited_path, sources) = if edits_variant {
                (VARIANT, [variant_move, tracked_move])
            } else {
                (TRACKED, [tracked_move, variant_move])
            };
            write_working_files(&repository, &[(edited_path, &edited)]).await;

            let mut moves = Vec::new();
            for (index, (source, node, content, destination)) in sources.into_iter().enumerate() {
                let from = Staged {
                    state: state.clone(),
                    node,
                    address: file_node(content).address,
                    mode: 0,
                };
                let carried = if index == 0 { content } else { &edited };
                let destination = relative(destination);
                let to = state_holding(&repository, &destination, file_node(carried), 2).await;
                moves.push(NodeChange {
                    action: FileAction::Move,
                    flags: change::Flags::None,
                    from: side(&repository, &from, relative(source)),
                    to: side(&repository, &to, destination),
                });
            }

            for _ in 0..2 {
                assert!(
                    is_local_modifications(
                        &Box::pin(verify_delete(&repository, &state, PLUGIN, &moves)).await
                    ),
                    "The edit of {edited_path} goes with the move of its own spelling, which \
                     carries other content"
                );
                moves.reverse();
            }
        }
    }))
    .await;
}

/// A forced delete keeps a file spelled as a case variant of a deleted file where it holds other
/// content of the same size, and the directory with it.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_forced_delete_keeps_a_case_variant_holding_other_content() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let content = pseudo_random_bytes(1174, 0);
        let state = plugin_tree(&repository, &[(TRACKED, &content)]).await;
        let mine = pseudo_random_bytes(1174, 1);
        write_working_files(&repository, &[(TRACKED, &content), (VARIANT, &mine)]).await;

        Box::pin(delete_plugin(&repository, &state, &[TRACKED], false))
            .await
            .expect("the delete realizes");
        assert!(!working_path_exists(&repository, TRACKED).await);
        assert_eq!(working_file(&repository, VARIANT).await, mine);
    }))
    .await;
}

/// A merge verifies under the ignore file, which can let an edited case variant of a committed
/// file it ignores reach the realize. Deleting the directory keeps the edited file.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_merge_deleting_a_directory_keeps_an_edited_case_variant_of_an_ignored_file() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let mut filter = Filter::default();
        filter.ignore.add_exclusion("*.cs").unwrap();
        let repository = working_tree_repository_filtered(dir.path(), filter).await;
        let state = plugin_tree(&repository, &[(TRACKED, &pseudo_random_bytes(1174, 0))]).await;
        let edited = pseudo_random_bytes(1174, 1);
        write_working_files(&repository, &[(VARIANT, &edited)]).await;

        let mut changes = Vec::new();
        let mut refused = false;
        for path in [PLUGIN, SOURCE, TRACKED] {
            let mut change = delete_change(&repository, &state, path).await;
            let operation = repository.file_system().begin_operation().await.unwrap();
            refused |= Box::pin(verify_filesystem(
                &mut change,
                repository.clone(),
                operation,
                NodeMapping::root(repository.clone(), state.clone()),
                false,
                false,
                false,
                Arc::default(),
                FilterMode::Full,
                &IncomingMoves::new(Arc::default()),
            ))
            .await
            .is_err();
            changes.push(change);
        }
        if !refused {
            let operation = repository.file_system().begin_operation().await.unwrap();
            realize_changes(
                repository.clone(),
                operation,
                Arc::new(changes),
                None,
                false,
                true,
                Arc::default(),
            )
            .await
            .expect("the merge realizes");
        }
        assert_eq!(working_file(&repository, VARIANT).await, edited);
    }))
    .await;
}

/// A file addressed as a list nothing in the store holds, so no hash check can
/// establish anything about it.
///
/// Unresolved has to read as modified, and the merge refuses. What spares an
/// untouched file that fate is the modified time recorded against the current
/// revision, which the base revision's node could never carry.
#[tokio::test]
async fn an_unresolvable_chunking_is_refused_until_a_recorded_time_answers() {
    Box::pin(with_execution(async {
        let dir = lore_base::test_util::TempDir::new("lore-realize-test-");
        let repository = working_tree_repository(dir.path()).await;
        let path = RelativePathBuf::new().push_and_freeze("unresolvable.bin");
        let base_content = pseudo_random_bytes(150 * 1024, 0);
        let source_content = pseudo_random_bytes(150 * 1024, 1);
        write_working_file(&repository, &path, &base_content).await;

        let mut unresolvable = file_node(&base_content);
        unresolvable.address = lore_base::types::Address::zero_context_hash(hash::hash_slice(
            b"a list nothing stored",
        ));

        let base = state_holding(&repository, &path, unresolvable, 1).await;
        let source = state_holding(&repository, &path, file_node(&source_content), 2).await;
        let current = state_holding(&repository, &path, unresolvable, 3).await;

        assert!(
            Box::pin(verify(&repository, &path, &base, &source, &current))
                .await
                .is_err(),
            "Nothing established means the file may hold local work"
        );

        record_time(&repository, &path).await;

        assert!(
            Box::pin(verify(&repository, &path, &base, &source, &current))
                .await
                .expect("the recorded time answers for the file")
                .is_some(),
            "The time recorded against the current revision spares the file the hash check"
        );
    }))
    .await;
}

/// A directory that cannot be materialized fails before the consumer can schedule the
/// following files. That failure must not prevent independent additions from being written.
#[tokio::test]
async fn sync_continues_after_a_materialization_error() {
    Box::pin(with_execution(async {
        let temp = lore_base::test_util::TempDir::new("lore-sync-continue-");
        let repository = working_tree_repository(temp.path()).await;
        let blocked = RelativePath::new_from_initial_path("blocked").unwrap();
        write_working_file(&repository, &blocked, b"local file blocks directory").await;
        let mut changes = Vec::new();
        for (name, node) in [
            ("blocked", Node::new_zeroed()),
            (
                "first.txt",
                Node {
                    address: Default::default(),
                    ..file_node(b"")
                },
            ),
            (
                "last.txt",
                Node {
                    address: Default::default(),
                    ..file_node(b"")
                },
            ),
        ] {
            let path = RelativePath::new_from_initial_path(name).unwrap();
            let staged = state_holding(&repository, &path, node, 1).await;
            changes.push(NodeChange {
                action: FileAction::Add,
                flags: change::Flags::None,
                from: side(&repository, &staged, path.clone()),
                to: side(&repository, &staged, path),
            });
        }
        let operation = repository.file_system().begin_operation().await.unwrap();
        let result = realize_changes(
            repository.clone(),
            operation.clone(),
            Arc::new(changes),
            None,
            false,
            false,
            Arc::default(),
        )
        .await;
        assert!(
            result.is_err(),
            "the obstructed directory must fail the operation"
        );
        for name in ["first.txt", "last.txt"] {
            let path = RelativePath::new_from_initial_path(name).unwrap();
            let info = operation.file_info(&path).await.unwrap();
            assert!(info.exists(), "available file {name} was not materialized");
            assert_eq!(info.size(), 0);
        }
    }))
    .await;
}

/// A move whose rename is refused is realized by writing the node at its destination, which is
/// removed first unless it names the source's own entry or cannot be told from it. On a file system
/// that folds case a case-only move's destination is its source, which for a directory holds every
/// child the move carries; elsewhere it is an entry of its own, removed as any other destination
/// is. A move that keeps its destination is counted, with the first, for the realize's warning.
#[tokio::test]
async fn a_refused_move_removes_its_destination_unless_it_is_the_source() {
    Box::pin(with_execution(async {
        let (immutable_store, mutable_store, _) =
            test_store_create().await.expect("Making test stores");
        let cases = [
            (TestIdentity::Folded, "dir", &[][..]),
            (TestIdentity::Exact, "dir", &["dir"][..]),
            (TestIdentity::Folded, "other", &["other"][..]),
            (TestIdentity::NamesNothing, "dir", &["dir"][..]),
            (TestIdentity::Fails, "other", &[][..]),
        ];
        for (identity, to, removed) in cases {
            let filesystem = Arc::new(TestFilesystemProvider::new().with_identity(identity));
            let repository = test_repository_over(
                immutable_store.clone(),
                mutable_store.clone(),
                filesystem.clone(),
            );
            let state = State::new();
            let node = add_clean(&repository, &state, ROOT_NODE, (directory, "dir")).await;
            let side = |path: &str| NodeChangeState {
                mapping: NodeMapping {
                    repository: repository.clone(),
                    state: state.clone(),
                    path: relative(path),
                    node,
                },
                observed: None,
                flags: NodeFlags::NoFlags,
                address: Default::default(),
                mode: 0,
            };
            let change = NodeChange {
                action: FileAction::Move,
                flags: change::Flags::None,
                from: side("Dir"),
                to: side(to),
            };
            let stats = Arc::new(SyncRealizeStats::default());
            let operation = repository.file_system().begin_operation().await.unwrap();
            realize_changes(
                repository.clone(),
                operation,
                Arc::new(vec![change]),
                None,
                false,
                false,
                stats.clone(),
            )
            .await
            .expect("a refused rename is realized by writing the node");

            let context = format!("the move of Dir to {to} answered {identity:?}");
            assert_eq!(filesystem.removed.lock().as_slice(), removed, "{context}");
            let kept = removed
                .is_empty()
                .then(|| ("Dir".to_string(), to.to_string()));
            assert_eq!(
                (
                    stats.kept_moves.count.load(Ordering::Relaxed),
                    stats
                        .kept_moves
                        .first
                        .get()
                        .map(|kept| (kept.from.to_string(), kept.to.to_string())),
                ),
                (usize::from(kept.is_some()), kept),
                "{context}"
            );
        }
    }))
    .await;
}
