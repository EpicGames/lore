// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_revision::change;
use lore_revision::change::FileAction;
use lore_revision::change::NodeChange;
use lore_revision::change::NodeChangeState;
use lore_revision::filter::FilterMode;
use lore_revision::fs::filesystem_provider::InstanceOperation;
use lore_revision::fs::realize::*;
use lore_revision::node::Node;
use lore_revision::node::NodeFileMode;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision::sync::SyncError;
use lore_revision::state;
use lore_revision::state::NodeMapping;
use lore_revision::state::State;
use lore_revision::util;
use lore_revision::util::path::RelativePath;
use lore_revision::util::path::RelativePathBuf;
use lore_storage::hash;
use zerocopy::FromZeros;

use crate::repository::test_helpers::file_node;
use crate::repository::test_helpers::working_tree_repository;

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

/// A state holding `node` at `path`, under a revision of its own.
async fn state_holding(
    repository: &Arc<RepositoryContext>,
    path: &RelativePath,
    node: Node,
    revision: u8,
) -> Staged {
    let state = State::new();
    state.set_revision(lore_base::types::Hash::from([revision; 32]));
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
            path.to_absolute_path(repository.require_path().expect("working tree")),
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
/// working tree at `state_current`.
async fn verify(
    repository: &Arc<RepositoryContext>,
    path: &RelativePath,
    base: &Staged,
    source: &Staged,
    current: &Staged,
) -> Result<Option<NodeChange>, SyncError> {
    Box::pin(verify_action(
        repository,
        path,
        base,
        source,
        current,
        FileAction::Keep,
    ))
    .await
}

/// [`verify`] for a change of a given action.
async fn verify_action(
    repository: &Arc<RepositoryContext>,
    path: &RelativePath,
    base: &Staged,
    source: &Staged,
    current: &Staged,
    action: FileAction,
) -> Result<Option<NodeChange>, SyncError> {
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

    let realize = Box::pin(verify_filesystem(
        &mut change,
        repository.clone(),
        operation,
        NodeMapping::root(repository.clone(), current.state.clone()),
        false,
        false,
        Arc::default(),
        FilterMode::Full,
    ))
    .await?;

    Ok(realize.then_some(change))
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

/// A file the branch never touched, holding what the current revision says it should.
/// The merge has to be free to overwrite it.
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
    let absolute = path.to_absolute_path(repository.require_path().expect("working tree"));
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
    let absolute = path.to_absolute_path(repository.require_path().expect("working tree"));
    let metadata = lore_io::IoDriver::global()
        .metadata(&absolute)
        .await
        .expect("working file metadata");
    util::fs::metadata_set_executable(&absolute, &metadata, true).await;
}

/// A node addressing `content` and carrying the executable bit.
#[cfg(target_family = "unix")]
fn executable_node(content: &[u8]) -> Node {
    let mut node = file_node(content);
    node.mode = NodeFileMode::Executable.bits();
    node
}

/// A change that moves the executable bit alone is carried by the verify that drops it:
/// the content is in place, so writing the file would replace it with the bytes it
/// already holds.
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

        assert!(
            Box::pin(verify(&repository, &path, &base, &source, &current))
                .await
                .expect("a mode change over matching content is not a failure")
                .is_none()
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

        assert!(
            Box::pin(verify(&repository, &path, &base, &source, &current))
                .await
                .expect("a mode change over the incoming content is not a failure")
                .is_none()
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

/// Store `content` under a fragment list cut at boundaries this build never cuts at,
/// as a client of another version left it, and return the node addressing it.
async fn stored_under_a_list(repository: &Arc<RepositoryContext>, content: &[u8]) -> Node {
    use zerocopy::IntoBytes;

    let mut list = Vec::new();
    let mut offset = 0;
    while offset < content.len() {
        let end = (offset + 17 * 1024).min(content.len());
        list.push(lore_base::types::FragmentReference {
            hash: hash::hash_slice(&content[offset..end]),
            offset_content: offset as u64,
        });
        offset = end;
    }

    let payload = bytes::Bytes::copy_from_slice(list.as_slice().as_bytes());
    let address = lore_base::types::Address::zero_context_hash(hash::hash_slice(&payload));
    lore_revision::immutable::store_raw_store_retry(
        repository.immutable_store(),
        repository.id,
        address,
        lore_base::types::Fragment {
            flags: lore_base::types::FragmentFlags::PayloadFragmented.bits(),
            size_payload: payload.len() as u32,
            size_content: content.len() as u64,
        },
        Some(payload),
    )
    .await
    .expect("store the fragment list");

    let mut node = Node::new_zeroed();
    node.flags = NodeFlags::File.bits();
    node.address = address;
    node.size = content.len() as u64;
    node
}

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

        let listed = stored_under_a_list(&repository, &content).await;
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

        assert!(
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
            .is_some(),
            "A move must reach realize even where the content is already in place"
        );
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
