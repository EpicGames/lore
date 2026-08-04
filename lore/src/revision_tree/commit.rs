// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_commit` — freeze the handle's tree, write the 320-
//! byte revision record, and atomically advance the target branch tip. The
//! options struct carries the `remote_write` flag (`u8`, 0 or 1, not
//! `bool`) selecting between local-only and remote-uploading commits.

use std::path::Path;
use std::sync::atomic::Ordering;

use lore_base::error::InvalidArguments;
use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_macro::LoreArgs;
use lore_macro::ValidateText;
use lore_revision::branch;
use lore_revision::commit::CommitError;
use lore_revision::commit::commit_tree;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeCommitCompleteEventData;
use lore_revision::lore::execution_context;
use lore_revision::metadata::Metadata;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::state::State;
use serde::Deserialize;
use serde::Serialize;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;
use crate::revision_tree::handle::synth_repository_write_context;

/// Tuneables for `lore_revision_tree_commit`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Deserialize, Serialize, ValidateText)]
pub struct LoreRevisionTreeCommitOptions {
    /// Also upload the new revision to remote (local-only by default)
    pub remote_write: u8,
}

/// Arguments for `lore_revision_tree_commit`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Deserialize, Serialize, LoreArgs)]
#[handler(commit_impl)]
pub struct LoreRevisionTreeCommitArgs {
    /// Per-call correlation id echoed back in events
    pub id: u64,
    /// Loaded revision-tree handle to freeze and commit
    pub handle: LoreRevisionTree,
    /// Branch whose tip is atomically advanced to the new revision
    pub branch: BranchId,
    /// Commit tuneables (local-only vs remote-uploading)
    pub options: LoreRevisionTreeCommitOptions,
}

/// Map a commit failure to the per-call event's error code. The full ffi
/// error code (branch-advanced, nothing-staged, …) still travels on the
/// trailing `Complete` detail; the event code is the coarse discriminator
/// the caller branches on.
fn error_code_for(error: &CommitError) -> LoreErrorCode {
    match error {
        CommitError::BranchAdvanced(_) => LoreErrorCode::BranchAdvanced,
        CommitError::InvalidArguments(_) | CommitError::NothingStaged(_) => {
            LoreErrorCode::InvalidArguments
        }
        _ => LoreErrorCode::Internal,
    }
}

fn emit_commit_complete(
    id: u64,
    revision_hash: Hash,
    new_tip_hash: Hash,
    error_code: LoreErrorCode,
) {
    LoreEvent::RevisionTreeCommitComplete(LoreRevisionTreeCommitCompleteEventData {
        id,
        revision_hash,
        new_tip_hash,
        error_code,
    })
    .send();
}

/// Publish a locally constructed revision through the remote branch gate.
///
/// A transport error can happen after the server accepted the push but before
/// the response reached us. Querying the remote branch makes that case
/// retry-safe: observing `revision_hash` as the remote latest is success;
/// observing a tip other than the expected base is a real tip collision; an
/// unchanged base preserves non-tip rejections (for example protection) as
/// their generic commit failure.
async fn push_remote_revision(
    repository: std::sync::Arc<RepositoryContext>,
    branch_id: BranchId,
    expected_remote_tip: Hash,
    revision_hash: Hash,
) -> Result<(), (CommitError, Hash)> {
    let remote = repository.remote().await.map_err(|error| {
        (
            CommitError::internal_with_context(error, "connecting to revision remote"),
            Hash::default(),
        )
    })?;
    let revision = remote.revision(repository.id).await.map_err(|error| {
        (
            CommitError::internal_with_context(error, "connecting to remote revision service"),
            Hash::default(),
        )
    })?;

    match revision
        .branch_push(branch_id, revision_hash, false, false)
        .await
    {
        Ok(response) if response.revision == revision_hash => Ok(()),
        Ok(response) => Err((
            CommitError::from(lore_base::error::BranchAdvanced),
            response.revision,
        )),
        Err(push_error) => match revision.branch_query(Some(branch_id), None).await {
            Ok(branch) if branch.latest == revision_hash => Ok(()),
            Ok(branch) if branch.latest != expected_remote_tip => Err((
                CommitError::from(lore_base::error::BranchAdvanced),
                branch.latest,
            )),
            Ok(_) | Err(_) => Err((
                CommitError::internal_with_context(push_error, "pushing revision branch to remote"),
                Hash::default(),
            )),
        },
    }
}

/// Freeze the handle's tree and commit it as a new revision on `branch`.
///
/// On success the caller receives `LORE_EVENT_REVISION_TREE_COMMIT_COMPLETE`
/// carrying the newly-committed revision hash and `error_code = NONE`,
/// before `Complete {status: 0}`; the handle then behaves as if freshly
/// loaded from the new revision — no pending edits, reads reflect the new
/// tree. The commit shares the file-system-based path's branch semantics:
/// the tip-collision check, the branch-advance write, and the remote-write
/// contract (`options.remote_write` uploads through the storage handle's
/// remote when one is configured and the session is neither local nor
/// offline).
///
/// When the branch tip advanced past the handle's loaded revision the
/// terminal event reports `error_code = BRANCH_ADVANCED` with the observed
/// tip in `new_tip_hash`, so the caller can reload against it without an
/// extra round-trip. Any commit failure marks the handle invalid — the
/// in-memory state may hold a partially frozen tree — so the caller closes
/// it and reloads; subsequent operations on the handle fail with
/// `INVALID_ARGUMENTS`.
pub async fn commit(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeCommitArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, commit_impl).await
}

async fn commit_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeCommitArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let miss_id = args.id;
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        commit,
        move |_| {
            emit_commit_complete(
                miss_id,
                Hash::default(),
                Hash::default(),
                LoreErrorCode::InvalidArguments,
            );
        },
        async move |internal, args: LoreRevisionTreeCommitArgs| {
            let id = args.id;

            if args.branch.is_zero() {
                emit_commit_complete(
                    id,
                    Hash::default(),
                    Hash::default(),
                    LoreErrorCode::InvalidArguments,
                );
                return Err(CommitError::from(InvalidArguments {
                    reason: "branch id is zero".into(),
                }));
            }

            // There is no working-tree path to key the write token on;
            // keying on the repository id serializes memory-based commits
            // to the same repository in-process. The mutable store's own
            // lock keeps the branch advance atomic beyond that.
            let token_key = format!("revision-tree/{}", internal.repository);
            let token = RepositoryWriteToken::acquire(Path::new(&token_key)).await;

            // The handle's own context is read-only; the commit runs on a
            // write-capable sibling minted around the held token.
            let write_context = synth_repository_write_context(
                &internal.store_internal,
                internal.repository,
                &token,
            )
            .await;

            // Remote gating mirrors the file-system-based commit verb: the
            // caller's remote_write option opts in, the session's local /
            // offline switches veto.
            let context = execution_context();
            let globals = context.globals();
            let upload = args.options.remote_write != 0 && !globals.local() && !globals.offline();
            write_context.set_disable_upload(!upload);

            let pending_metadata = internal.pending_metadata.read().clone();
            let deleted = internal.pending_delta.read().clone();
            let state = internal.state();
            let expected_remote_tip = state.revision();

            match commit_tree(
                write_context.clone(),
                &token,
                state,
                pending_metadata,
                args.branch,
                deleted,
            )
            .await
            {
                Ok(revision_hash) => {
                    if upload
                        && let Err((error, remote_tip)) = push_remote_revision(
                            write_context,
                            args.branch,
                            expected_remote_tip,
                            revision_hash,
                        )
                        .await
                    {
                        internal.invalid.store(true, Ordering::Release);
                        emit_commit_complete(id, revision_hash, remote_tip, error_code_for(&error));
                        return Err(error);
                    }

                    // The handle now behaves as freshly loaded from the new
                    // revision: swap in a state deserialized from it and drop
                    // the pending edits it just committed. The committed
                    // state's in-memory serialization bookkeeping is spent —
                    // a fresh deserialize is the "freshly loaded" contract.
                    match State::deserialize(internal.repository_context.clone(), revision_hash)
                        .await
                    {
                        Ok(fresh_state) => {
                            *internal.state.write() = fresh_state;
                            *internal.pending_metadata.write() = Metadata::default();
                            internal.pending_delta.write().clear();
                            emit_commit_complete(
                                id,
                                revision_hash,
                                Hash::default(),
                                LoreErrorCode::None,
                            );
                            Ok(())
                        }
                        Err(error) => {
                            internal.invalid.store(true, Ordering::Release);
                            emit_commit_complete(
                                id,
                                revision_hash,
                                Hash::default(),
                                LoreErrorCode::Internal,
                            );
                            Err(CommitError::internal_with_context(
                                error,
                                "State::deserialize after commit",
                            ))
                        }
                    }
                }
                Err(error) => {
                    // Any commit failure invalidates the handle — the freeze
                    // may have partially cleared staged flags, so the
                    // in-memory state no longer round-trips (see the handle
                    // contract). The caller closes and reloads.
                    internal.invalid.store(true, Ordering::Release);
                    let new_tip_hash = if matches!(error, CommitError::BranchAdvanced(_)) {
                        branch::load_latest(internal.repository_context.clone(), args.branch)
                            .await
                            .unwrap_or_default()
                    } else {
                        Hash::default()
                    };
                    emit_commit_complete(id, Hash::default(), new_tip_hash, error_code_for(&error));
                    Err(error)
                }
            }
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Partition;
    use lore_error_set::FfiError;
    use lore_revision::commit::construct_tree_revision;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreMetadataType;
    use lore_revision::interface::LoreNodeType;
    use lore_revision::interface::LoreString;
    use lore_revision::node::NodeID;
    use lore_revision::node::NodeIDExt;
    use lore_revision::node::ROOT_NODE;
    use lore_storage::hash::hash_string;

    use super::*;
    use crate::revision_tree::add::LoreRevisionTreeAddArgs;
    use crate::revision_tree::add::LoreRevisionTreeAddEntry;
    use crate::revision_tree::add::add;
    use crate::revision_tree::delete::LoreRevisionTreeDeleteArgs;
    use crate::revision_tree::delete::delete;
    use crate::revision_tree::handle as rt_handle;
    use crate::revision_tree::load::LoreRevisionTreeLoadArgs;
    use crate::revision_tree::load::load;
    use crate::revision_tree::metadata_set::LoreRevisionTreeMetadataSetArgs;
    use crate::revision_tree::metadata_set::metadata_set;
    use crate::storage::handle as storage_handle;
    use crate::storage::handle::LoreStore;
    use crate::storage::store::in_memory_for_tests;

    #[derive(Debug, Clone, PartialEq)]
    enum CapturedEvent {
        Error(u32),
        Complete(i32),
        RevisionTreeLoaded(u64),
        AddComplete(u64, NodeID, LoreErrorCode),
        CommitComplete(u64, Hash, Hash, LoreErrorCode),
        Other(u32),
    }

    impl CapturedEvent {
        fn from_event(event: &LoreEvent) -> Self {
            match event {
                LoreEvent::Error(data) => Self::Error(data.error_type),
                LoreEvent::Complete(data) => Self::Complete(data.status),
                LoreEvent::RevisionTreeLoaded(data) => Self::RevisionTreeLoaded(data.handle_id),
                LoreEvent::RevisionTreeAddComplete(data) => {
                    Self::AddComplete(data.id, data.node_id, data.error_code)
                }
                LoreEvent::RevisionTreeCommitComplete(data) => Self::CommitComplete(
                    data.id,
                    data.revision_hash,
                    data.new_tip_hash,
                    data.error_code,
                ),
                other => Self::Other(other.discriminant()),
            }
        }
    }

    fn make_callback(sink: Arc<Mutex<Vec<CapturedEvent>>>) -> LoreEventCallback {
        Some(Box::new(move |event: &LoreEvent| {
            sink.lock().unwrap().push(CapturedEvent::from_event(event));
        }))
    }

    fn commit_outcome(events: &[CapturedEvent], id: u64) -> Option<(Hash, Hash, LoreErrorCode)> {
        events.iter().find_map(|event| match event {
            CapturedEvent::CommitComplete(event_id, revision, new_tip, error_code)
                if *event_id == id =>
            {
                Some((*revision, *new_tip, *error_code))
            }
            _ => None,
        })
    }

    async fn open_store(label: &str) -> LoreStore {
        storage_handle::register(in_memory_for_tests(label).await)
    }

    async fn load_tree(
        store: LoreStore,
        repository: Partition,
        revision: Hash,
    ) -> LoreRevisionTree {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = load(
            LoreGlobalArgs::default(),
            LoreRevisionTreeLoadArgs {
                store,
                repository,
                revision_hash: revision,
            },
            make_callback(sink.clone()),
        )
        .await;
        assert_eq!(status, 0, "load fixture must succeed");
        let id = sink
            .lock()
            .unwrap()
            .iter()
            .find_map(|event| match event {
                CapturedEvent::RevisionTreeLoaded(id) => Some(*id),
                _ => None,
            })
            .expect("load fixture must emit RevisionTreeLoaded");
        LoreRevisionTree { handle_id: id }
    }

    async fn add_file(handle: LoreRevisionTree, name: &str, payload: u8) -> NodeID {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = add(
            LoreGlobalArgs::default(),
            LoreRevisionTreeAddArgs {
                id: 1000,
                handle,
                entries: LoreArray::from_vec(vec![LoreRevisionTreeAddEntry {
                    id: 1000,
                    parent_node_id: ROOT_NODE,
                    name: LoreString::from_str(name),
                    kind: LoreNodeType::File as u32,
                    mode: 0o644,
                    size: 16,
                    address: Address {
                        hash: Hash::from([payload; 32]),
                        context: Context::from([payload; 16]),
                    },
                    ..Default::default()
                }]),
            },
            make_callback(sink.clone()),
        )
        .await;
        assert_eq!(status, 0, "add fixture must succeed");
        let events = sink.lock().unwrap().clone();
        events
            .iter()
            .find_map(|event| match event {
                CapturedEvent::AddComplete(_, node_id, LoreErrorCode::None) => Some(*node_id),
                _ => None,
            })
            .expect("add fixture must emit AddComplete")
    }

    async fn add_directory(handle: LoreRevisionTree, parent_node_id: NodeID, name: &str) -> NodeID {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = add(
            LoreGlobalArgs::default(),
            LoreRevisionTreeAddArgs {
                id: 1001,
                handle,
                entries: LoreArray::from_vec(vec![LoreRevisionTreeAddEntry {
                    id: 1001,
                    parent_node_id,
                    name: LoreString::from_str(name),
                    kind: LoreNodeType::Directory as u32,
                    mode: 0o755,
                    ..Default::default()
                }]),
            },
            make_callback(sink.clone()),
        )
        .await;
        assert_eq!(status, 0, "add directory fixture must succeed");
        let events = sink.lock().unwrap().clone();
        events
            .iter()
            .find_map(|event| match event {
                CapturedEvent::AddComplete(_, node_id, LoreErrorCode::None) => Some(*node_id),
                _ => None,
            })
            .expect("add directory fixture must emit AddComplete")
    }

    async fn run_commit(
        handle: LoreRevisionTree,
        id: u64,
        branch: BranchId,
    ) -> (i32, Vec<CapturedEvent>) {
        run_commit_with_options(handle, id, branch, LoreRevisionTreeCommitOptions::default()).await
    }

    async fn run_commit_with_options(
        handle: LoreRevisionTree,
        id: u64,
        branch: BranchId,
        options: LoreRevisionTreeCommitOptions,
    ) -> (i32, Vec<CapturedEvent>) {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = commit(
            LoreGlobalArgs::default(),
            LoreRevisionTreeCommitArgs {
                id,
                handle,
                branch,
                options,
            },
            make_callback(sink.clone()),
        )
        .await;
        let events = sink.lock().unwrap().clone();
        (status, events)
    }

    fn release(handle: LoreRevisionTree, store: LoreStore) {
        rt_handle::unregister(handle);
        storage_handle::unregister(store);
    }

    /// Scope an execution context around direct store reads the way the
    /// dispatch helpers do — the verification reads below run outside any
    /// verb, and a freshly swapped-in state faults its blocks from the store.
    async fn scoped<T>(future: impl Future<Output = T>) -> T {
        let execution = crate::call::setup_execution(LoreGlobalArgs::default(), None);
        lore_base::runtime::LORE_CONTEXT
            .scope(execution, future)
            .await
    }

    #[tokio::test]
    async fn commit_added_file_advances_branch_and_keeps_the_handle_live() {
        let store = open_store("ct-basic").await;
        let repository = Partition::from([0x11u8; 16]);
        let branch = BranchId::from([0x01u8; 16]);
        let handle = load_tree(store, repository, Hash::default()).await;
        add_file(handle, "doc.md", 0x42).await;

        let (status, events) = run_commit(handle, 1, branch).await;

        assert_eq!(
            status, 0,
            "committing an added file must succeed, got {events:?}"
        );
        let (revision, new_tip, error_code) =
            commit_outcome(&events, 1).expect("CommitComplete must fire");
        assert_eq!(error_code, LoreErrorCode::None);
        assert!(!revision.is_zero(), "a real revision hash must be reported");
        assert_eq!(
            new_tip,
            Hash::default(),
            "new_tip_hash is only populated on BranchAdvanced"
        );
        assert!(events.contains(&CapturedEvent::Complete(0)));

        let entry = rt_handle::REGISTRY
            .get(&handle.handle_id)
            .expect("handle registered");
        let (state, repository_context) = (entry.state(), entry.repository_context.clone());
        drop(entry);
        assert_eq!(
            state.revision(),
            revision,
            "the handle must behave as freshly loaded from the new revision"
        );
        let latest = scoped(branch::load_latest(repository_context.clone(), branch))
            .await
            .expect("branch tip must be readable");
        assert_eq!(
            latest, revision,
            "the branch tip must advance to the commit"
        );
        assert!(
            scoped(state.find_subnode(repository_context, ROOT_NODE, hash_string("doc.md")))
                .await
                .is_ok(),
            "reads after commit must reflect the committed tree"
        );

        release(handle, store);
    }

    #[tokio::test]
    async fn construct_tree_revision_does_not_advance_branch_latest() {
        let store = open_store("ct-construct-only").await;
        let repository = Partition::from([0x19u8; 16]);
        let branch = BranchId::from([0x09u8; 16]);
        let handle = load_tree(store, repository, Hash::default()).await;
        add_file(handle, "unpublished.md", 0x51).await;

        let entry = rt_handle::REGISTRY
            .get(&handle.handle_id)
            .expect("handle registered");
        let state = entry.state();
        let store_internal = entry.store_internal.clone();
        drop(entry);

        let token_key = format!("revision-tree/{repository}");
        let token = RepositoryWriteToken::acquire(Path::new(&token_key)).await;
        let write_context =
            synth_repository_write_context(&store_internal, repository, &token).await;
        let signature = scoped(construct_tree_revision(
            write_context.clone(),
            &token,
            state,
            Metadata::default(),
            branch,
            Vec::new(),
        ))
        .await
        .expect("constructing the immutable revision must succeed");
        assert!(!signature.is_zero());

        let latest = scoped(branch::load_latest(write_context, branch))
            .await
            .unwrap_or_default();
        assert!(
            latest.is_zero(),
            "construction must leave branch publication to the server push path"
        );

        release(handle, store);
    }

    #[tokio::test]
    async fn commit_and_reload_preserves_nested_empty_directory() {
        let store = open_store("ct-empty-directory").await;
        let repository = Partition::from([0x18u8; 16]);
        let branch = BranchId::from([0x08u8; 16]);
        let handle = load_tree(store, repository, Hash::default()).await;
        let docs = add_directory(handle, ROOT_NODE, "docs").await;
        add_directory(handle, docs, "empty").await;

        let (status, events) = run_commit(handle, 15, branch).await;
        assert_eq!(
            status, 0,
            "committing nested empty directories must succeed, got {events:?}"
        );
        let (revision, _, error_code) =
            commit_outcome(&events, 15).expect("CommitComplete must fire");
        assert_eq!(error_code, LoreErrorCode::None);

        let reloaded = load_tree(store, repository, revision).await;
        let entry = rt_handle::REGISTRY
            .get(&reloaded.handle_id)
            .expect("reloaded handle registered");
        let (state, repository_context) = (entry.state(), entry.repository_context.clone());
        drop(entry);

        let docs =
            scoped(state.find_subnode(repository_context.clone(), ROOT_NODE, hash_string("docs")))
                .await
                .expect("top-level empty directory must survive reload");
        let empty =
            scoped(state.find_subnode(repository_context.clone(), docs, hash_string("empty")))
                .await
                .expect("nested empty directory must survive reload");
        let node = scoped(state.node(repository_context, empty))
            .await
            .expect("empty directory node must be readable");
        assert!(node.is_directory());
        assert!(
            !node.child.is_valid_node_id(),
            "directory must remain empty"
        );
        assert_eq!(node.size, 0);
        assert!(
            !node.address.hash.is_zero(),
            "empty directory must be hashed"
        );

        rt_handle::unregister(handle);
        rt_handle::unregister(reloaded);
        storage_handle::unregister(store);
    }

    #[tokio::test]
    async fn commit_chain_links_the_second_revision_to_the_first() {
        let store = open_store("ct-chain").await;
        let repository = Partition::from([0x22u8; 16]);
        let branch = BranchId::from([0x02u8; 16]);
        let handle = load_tree(store, repository, Hash::default()).await;

        add_file(handle, "a.md", 0x41).await;
        let (status, events) = run_commit(handle, 2, branch).await;
        assert_eq!(status, 0, "first commit must succeed, got {events:?}");
        let (first, _, _) = commit_outcome(&events, 2).expect("CommitComplete must fire");

        add_file(handle, "b.md", 0x43).await;
        let (status, events) = run_commit(handle, 3, branch).await;
        assert_eq!(status, 0, "second commit must succeed, got {events:?}");
        let (second, _, error_code) = commit_outcome(&events, 3).expect("CommitComplete must fire");
        assert_eq!(error_code, LoreErrorCode::None);
        assert_ne!(second, first, "each commit must produce a new revision");

        let entry = rt_handle::REGISTRY
            .get(&handle.handle_id)
            .expect("handle registered");
        let (state, repository_context) = (entry.state(), entry.repository_context.clone());
        drop(entry);
        assert_eq!(
            state.parent_self(),
            first,
            "the second revision must record the first as its parent"
        );
        assert_eq!(state.revision_number(), 2, "revision numbers must chain");
        let latest = branch::load_latest(repository_context, branch)
            .await
            .expect("branch tip must be readable");
        assert_eq!(latest, second);

        release(handle, store);
    }

    #[tokio::test]
    async fn commit_with_nothing_staged_fails_and_invalidates_the_handle() {
        let store = open_store("ct-nothing").await;
        let repository = Partition::from([0x33u8; 16]);
        let branch = BranchId::from([0x03u8; 16]);
        let handle = load_tree(store, repository, Hash::default()).await;

        let (status, events) = run_commit(handle, 4, branch).await;

        let expected = CommitError::from(lore_base::error::NothingStaged).ffi_code();
        assert_eq!(
            status, expected,
            "an empty commit must report NothingStaged"
        );
        let (revision, _, error_code) =
            commit_outcome(&events, 4).expect("CommitComplete must fire");
        assert_eq!(
            error_code,
            LoreErrorCode::InvalidArguments,
            "got {events:?}"
        );
        assert!(revision.is_zero());
        assert!(events.contains(&CapturedEvent::Complete(expected)));

        // The failed commit poisons the handle: subsequent ops miss.
        let (status, events) = run_commit(handle, 5, branch).await;
        assert_eq!(status, 1, "a poisoned handle must reject new ops");
        let (_, _, error_code) = commit_outcome(&events, 5).expect("CommitComplete must fire");
        assert_eq!(error_code, LoreErrorCode::InvalidArguments);

        release(handle, store);
    }

    #[tokio::test]
    async fn remote_write_without_a_remote_reports_failure_and_invalidates_the_handle() {
        let store = open_store("ct-remote-required").await;
        let repository = Partition::from([0x39u8; 16]);
        let branch = BranchId::from([0x09u8; 16]);
        let handle = load_tree(store, repository, Hash::default()).await;
        add_file(handle, "remote.md", 0x52).await;

        let (status, events) = run_commit_with_options(
            handle,
            16,
            branch,
            LoreRevisionTreeCommitOptions { remote_write: 1 },
        )
        .await;

        assert_ne!(
            status, 0,
            "remote_write must require a remote, got {events:?}"
        );
        let (revision, remote_tip, error_code) =
            commit_outcome(&events, 16).expect("CommitComplete must fire");
        assert!(
            !revision.is_zero(),
            "the locally constructed revision must be reported"
        );
        assert!(remote_tip.is_zero());
        assert_eq!(error_code, LoreErrorCode::Internal);

        let (status, _) = run_commit(handle, 17, branch).await;
        assert_eq!(
            status, 1,
            "a failed remote publication must poison the handle"
        );

        release(handle, store);
    }

    #[tokio::test]
    async fn commit_against_an_advanced_branch_reports_the_observed_tip() {
        let store = open_store("ct-advanced").await;
        let repository = Partition::from([0x44u8; 16]);
        let branch = BranchId::from([0x04u8; 16]);

        // Two handles derive from the same (empty) revision; the first
        // commit wins the tip.
        let winner = load_tree(store, repository, Hash::default()).await;
        let loser = load_tree(store, repository, Hash::default()).await;

        add_file(winner, "won.md", 0x45).await;
        let (status, events) = run_commit(winner, 6, branch).await;
        assert_eq!(status, 0, "the winning commit must succeed, got {events:?}");
        let (winning_revision, _, _) =
            commit_outcome(&events, 6).expect("CommitComplete must fire");

        add_file(loser, "lost.md", 0x46).await;
        let (status, events) = run_commit(loser, 7, branch).await;
        let expected = CommitError::from(lore_base::error::BranchAdvanced).ffi_code();
        assert_eq!(
            status, expected,
            "the losing commit must report BranchAdvanced, got {events:?}"
        );
        let (revision, new_tip, error_code) =
            commit_outcome(&events, 7).expect("CommitComplete must fire");
        assert_eq!(error_code, LoreErrorCode::BranchAdvanced, "got {events:?}");
        assert!(revision.is_zero());
        assert_eq!(
            new_tip, winning_revision,
            "the observed tip must ride the terminal event so the caller can reload"
        );

        release(winner, store);
        release(loser, store);
    }

    #[tokio::test]
    async fn commit_records_pending_metadata_on_the_revision() {
        let store = open_store("ct-metadata").await;
        let repository = Partition::from([0x55u8; 16]);
        let branch = BranchId::from([0x05u8; 16]);
        let handle = load_tree(store, repository, Hash::default()).await;

        let set_status = metadata_set(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataSetArgs {
                id: 8,
                handle,
                key: LoreString::from_str("message"),
                value: LoreString::from_str("import nightly assets"),
                format: LoreMetadataType::String as u32,
            },
            None,
        )
        .await;
        assert_eq!(set_status, 0, "the metadata_set fixture must succeed");
        add_file(handle, "doc.md", 0x47).await;

        let (status, events) = run_commit(handle, 9, branch).await;
        assert_eq!(status, 0, "the commit must succeed, got {events:?}");

        let entry = rt_handle::REGISTRY
            .get(&handle.handle_id)
            .expect("handle registered");
        let (state, repository_context) = (entry.state(), entry.repository_context.clone());
        assert!(
            entry.pending_metadata.read().is_empty(),
            "a successful commit must drain the pending metadata"
        );
        drop(entry);
        let metadata = scoped(lore_revision::metadata::Metadata::deserialize(
            repository_context,
            state.metadata_hash(),
        ))
        .await
        .expect("the committed revision must carry a metadata fragment");
        assert_eq!(
            metadata.get_string("message").expect("message key"),
            "import nightly assets",
            "the caller-set message must survive the commit metadata preparation"
        );
        assert_eq!(
            metadata.get_branch().expect("branch key"),
            branch,
            "the commit must record the branch"
        );
        assert!(
            metadata.get_timestamp().expect("timestamp key") > 0,
            "the commit must record a timestamp"
        );

        release(handle, store);
    }

    #[tokio::test]
    async fn commit_after_delete_drops_the_node_from_the_new_revision() {
        let store = open_store("ct-delete").await;
        let repository = Partition::from([0x66u8; 16]);
        let branch = BranchId::from([0x06u8; 16]);
        let handle = load_tree(store, repository, Hash::default()).await;

        add_file(handle, "keep.md", 0x48).await;
        let gone = add_file(handle, "gone.md", 0x49).await;
        let (status, _) = run_commit(handle, 10, branch).await;
        assert_eq!(status, 0, "the base commit must succeed");

        let delete_status = delete(
            LoreGlobalArgs::default(),
            LoreRevisionTreeDeleteArgs {
                id: 11,
                handle,
                node_id: gone,
            },
            None,
        )
        .await;
        assert_eq!(delete_status, 0, "the delete must succeed");

        let (status, events) = run_commit(handle, 12, branch).await;
        assert_eq!(status, 0, "the delete commit must succeed, got {events:?}");
        let (revision, _, _) = commit_outcome(&events, 12).expect("CommitComplete must fire");

        let entry = rt_handle::REGISTRY
            .get(&handle.handle_id)
            .expect("handle registered");
        let (state, repository_context) = (entry.state(), entry.repository_context.clone());
        assert!(
            entry.pending_delta.read().is_empty(),
            "a successful commit must drain the carried delete entries"
        );
        drop(entry);
        assert_eq!(state.revision(), revision);
        assert!(
            scoped(state.find_subnode(
                repository_context.clone(),
                ROOT_NODE,
                hash_string("gone.md")
            ))
            .await
            .is_err(),
            "the deleted file must not exist in the new revision"
        );
        assert!(
            scoped(state.find_subnode(repository_context, ROOT_NODE, hash_string("keep.md")))
                .await
                .is_ok(),
            "the untouched file must survive"
        );

        release(handle, store);
    }

    #[tokio::test]
    async fn commit_with_a_zero_branch_fails_without_poisoning_the_handle() {
        let store = open_store("ct-zero-branch").await;
        let repository = Partition::from([0x77u8; 16]);
        let handle = load_tree(store, repository, Hash::default()).await;
        add_file(handle, "doc.md", 0x4A).await;

        let (status, events) = run_commit(handle, 13, BranchId::default()).await;
        assert_eq!(status, 1, "a zero branch id must fail, got {events:?}");
        let (_, _, error_code) = commit_outcome(&events, 13).expect("CommitComplete must fire");
        assert_eq!(error_code, LoreErrorCode::InvalidArguments);

        // Argument validation happens before any state is touched, so the
        // handle stays usable and the commit succeeds on a real branch.
        let (status, events) = run_commit(handle, 14, BranchId::from([0x07u8; 16])).await;
        assert_eq!(
            status, 0,
            "the handle must survive the rejected call, got {events:?}"
        );

        release(handle, store);
    }

    #[tokio::test]
    async fn commit_on_unknown_handle_emits_commit_complete_with_invalid_arguments() {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = commit(
            LoreGlobalArgs::default(),
            LoreRevisionTreeCommitArgs {
                id: 15,
                handle: LoreRevisionTree::INVALID,
                branch: BranchId::from([0x08u8; 16]),
                options: LoreRevisionTreeCommitOptions::default(),
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "committing against an unknown handle must fail");
        let events = sink.lock().unwrap().clone();
        let (revision, _, error_code) = commit_outcome(&events, 15)
            .expect("a handle miss must still emit CommitComplete carrying the caller id");
        assert_eq!(
            error_code,
            LoreErrorCode::InvalidArguments,
            "got {events:?}"
        );
        assert!(revision.is_zero());
        assert!(events.contains(&CapturedEvent::Complete(1)));
    }
}
