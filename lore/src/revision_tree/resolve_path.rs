// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_resolve_path` — translate a UTF-8 path string to a
//! `NodeID` against the loaded revision tree. An empty path resolves to the
//! root node id. The verb does not touch disk.

use lore_base::error::InvalidArguments;
use lore_base::types::Hash;
use lore_base::types::RepositoryId;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::errors::StateErrors;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeResolvePathCompleteEventData;
use lore_revision::interface::LoreError;
use lore_revision::interface::LoreString;
use lore_revision::node::INVALID_NODE;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use serde::Deserialize;
use serde::Serialize;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;

/// Arguments for `lore_revision_tree_resolve_path`.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, LoreArgs)]
#[handler(resolve_path_impl)]
pub struct LoreRevisionTreeResolvePathArgs {
    /// Per-call correlation id echoed back in events
    pub id: u64,
    /// Loaded revision-tree handle to resolve against
    pub handle: LoreRevisionTree,
    /// UTF-8 path relative to the tree root; empty resolves to the root node
    pub path: LoreString,
}

#[error_set]
enum ResolvePathError {
    InvalidArguments,
}

impl EventError for ResolvePathError {
    fn translated(&self) -> LoreError {
        match self {
            ResolvePathError::InvalidArguments(_) => LoreError::InvalidArguments,
            ResolvePathError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

fn emit_resolve_complete(
    id: u64,
    node_id: NodeID,
    repository: RepositoryId,
    revision: Hash,
    error_code: LoreErrorCode,
) {
    LoreEvent::RevisionTreeResolvePathComplete(LoreRevisionTreeResolvePathCompleteEventData {
        id,
        node_id,
        repository,
        revision,
        error_code,
    })
    .send();
}

/// Resolve a UTF-8 path against the loaded revision tree to a `NodeID`.
///
/// On success the caller receives `LORE_EVENT_REVISION_TREE_RESOLVE_PATH_COMPLETE`
/// carrying the resolved node plus the `(repository, revision)` it belongs to
/// (which differ from the handle's when the path crosses a link) and
/// `error_code = NONE`, before `Complete {status: 0}`. An empty path resolves to
/// the root node. A path that does not resolve to a node — because it does not
/// exist or is not valid UTF-8 — completes with `error_code = INVALID_ARGUMENTS`.
/// The verb materializes no bytes to disk.
pub async fn resolve_path(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeResolvePathArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, resolve_path_impl).await
}

async fn resolve_path_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeResolvePathArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let miss_id = args.id;
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        resolve_path,
        move || {
            emit_resolve_complete(
                miss_id,
                INVALID_NODE,
                RepositoryId::default(),
                Hash::default(),
                LoreErrorCode::InvalidArguments,
            );
        },
        async move |internal, args: LoreRevisionTreeResolvePathArgs| {
            let id = args.id;
            let Ok(path) = std::str::from_utf8(args.path.as_bytes()) else {
                emit_resolve_complete(
                    id,
                    INVALID_NODE,
                    RepositoryId::default(),
                    Hash::default(),
                    LoreErrorCode::InvalidArguments,
                );
                return Err(ResolvePathError::from(InvalidArguments {
                    reason: "path is not valid UTF-8".into(),
                }));
            };

            if path.is_empty() {
                emit_resolve_complete(
                    id,
                    ROOT_NODE,
                    internal.repository,
                    internal.state.revision(),
                    LoreErrorCode::None,
                );
                return Ok(());
            }

            match internal
                .state
                .find_node_link(internal.repository_context.clone(), path)
                .await
            {
                Ok(link) => {
                    emit_resolve_complete(
                        id,
                        link.node,
                        link.repository,
                        link.revision,
                        LoreErrorCode::None,
                    );
                    Ok(())
                }
                Err(error) => {
                    let not_found = matches!(
                        error,
                        StateErrors::NotFound(_)
                            | StateErrors::NodeNotFound(_)
                            | StateErrors::LinkNotFound(_)
                            | StateErrors::RevisionNotFound(_)
                            | StateErrors::AddressNotFound(_)
                    );
                    if not_found {
                        emit_resolve_complete(
                            id,
                            INVALID_NODE,
                            RepositoryId::default(),
                            Hash::default(),
                            LoreErrorCode::InvalidArguments,
                        );
                        Err(ResolvePathError::from(InvalidArguments {
                            reason: "path does not resolve to a node".into(),
                        }))
                    } else {
                        emit_resolve_complete(
                            id,
                            INVALID_NODE,
                            RepositoryId::default(),
                            Hash::default(),
                            LoreErrorCode::Internal,
                        );
                        Err(ResolvePathError::internal_with_context(
                            error,
                            "State::find_node_link",
                        ))
                    }
                }
            }
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::Mutex;

    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::node::Node;
    use lore_revision::node::NodeFlags;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryWriteToken;
    use lore_revision::state::State;
    use lore_storage::hash::hash_string;

    use super::*;
    use crate::revision_tree::handle as rt_handle;
    use crate::revision_tree::handle::LoreRevisionTree;
    use crate::revision_tree::load::LoreRevisionTreeLoadArgs;
    use crate::revision_tree::load::load;
    use crate::storage::handle as storage_handle;
    use crate::storage::store::in_memory_for_tests;

    #[derive(Debug, Clone, PartialEq)]
    enum CapturedEvent {
        Error(u32),
        Complete(i32),
        RevisionTreeLoaded(u64),
        ResolvePathComplete(u64, NodeID, RepositoryId, Hash, LoreErrorCode),
        Other(u32),
    }

    impl CapturedEvent {
        fn from_event(event: &LoreEvent) -> Self {
            match event {
                LoreEvent::Error(data) => Self::Error(data.error_type),
                LoreEvent::Complete(data) => Self::Complete(data.status),
                LoreEvent::RevisionTreeLoaded(data) => Self::RevisionTreeLoaded(data.handle_id),
                LoreEvent::RevisionTreeResolvePathComplete(data) => Self::ResolvePathComplete(
                    data.id,
                    data.node_id,
                    data.repository,
                    data.revision,
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

    fn resolve_outcome(
        events: &[CapturedEvent],
        id: u64,
    ) -> Option<(NodeID, RepositoryId, Hash, LoreErrorCode)> {
        events.iter().find_map(|event| match event {
            CapturedEvent::ResolvePathComplete(
                event_id,
                node_id,
                repository,
                revision,
                error_code,
            ) if *event_id == id => Some((*node_id, *repository, *revision, *error_code)),
            _ => None,
        })
    }

    fn handle_state(handle: LoreRevisionTree) -> (Arc<State>, Arc<RepositoryContext>) {
        let entry = rt_handle::REGISTRY
            .get(&handle.handle_id)
            .expect("handle registered");
        (entry.state.clone(), entry.repository_context.clone())
    }

    /// Add a link node under root targeting `(repository, revision, target_node)`.
    /// Sets `name_hash` so `find_node_link` resolves it by name.
    async fn add_link(
        handle: LoreRevisionTree,
        name: &str,
        repository: Partition,
        revision: Hash,
        target_node: NodeID,
    ) -> NodeID {
        let (state, repository_context) = handle_state(handle);
        let node = Node {
            flags: NodeFlags::Link.bits(),
            name_hash: hash_string(name),
            child: target_node,
            address: Address {
                hash: revision,
                context: Context::from(repository),
            },
            ..Default::default()
        };
        state
            .node_add(repository_context, ROOT_NODE, node, name)
            .await
            .expect("node_add must succeed")
    }

    /// Add `child_name` under root, then serialize the state to a committed
    /// revision a link can point at. Returns the revision hash.
    async fn seal_target(handle: LoreRevisionTree, child_name: &str) -> Hash {
        let (state, repository_context) = handle_state(handle);
        let child = Node {
            flags: NodeFlags::File.bits(),
            name_hash: hash_string(child_name),
            ..Default::default()
        };
        state
            .node_add(repository_context.clone(), ROOT_NODE, child, child_name)
            .await
            .expect("node_add child must succeed");
        let token = RepositoryWriteToken::acquire(Path::new("link-target")).await;
        state
            .serialize(repository_context, &token)
            .await
            .expect("serialize must succeed")
    }

    async fn load_handle(label: &str, repository: Partition) -> (LoreRevisionTree, u64) {
        let store = in_memory_for_tests(label).await;
        let store_handle = storage_handle::register(store);
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = load(
            LoreGlobalArgs::default(),
            LoreRevisionTreeLoadArgs {
                store: store_handle,
                repository,
                revision_hash: Hash::default(),
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
        (LoreRevisionTree { handle_id: id }, store_handle.handle_id)
    }

    fn release(handle: LoreRevisionTree, store_handle_id: u64) {
        rt_handle::unregister(handle);
        storage_handle::unregister(crate::storage::handle::LoreStore {
            handle_id: store_handle_id,
        });
    }

    #[tokio::test]
    async fn resolve_empty_path_returns_root() {
        let (handle, store_handle_id) =
            load_handle("resolve-empty", Partition::from([0x11u8; 16])).await;
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));

        let status = resolve_path(
            LoreGlobalArgs::default(),
            LoreRevisionTreeResolvePathArgs {
                id: 7,
                handle,
                path: LoreString::default(),
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 0, "resolving the empty path must succeed");
        let events = sink.lock().unwrap().clone();
        let (node_id, repository, _revision, error_code) =
            resolve_outcome(&events, 7).expect("ResolvePathComplete must fire");
        assert_eq!(
            node_id, ROOT_NODE,
            "empty path must resolve to the root node, got {events:?}"
        );
        assert_eq!(error_code, LoreErrorCode::None);
        assert_eq!(
            repository,
            Partition::from([0x11u8; 16]),
            "root resolves in the handle's repository, got {events:?}"
        );
        let complete_pos = events
            .iter()
            .position(|event| matches!(event, CapturedEvent::Complete(_)))
            .expect("Complete must fire");
        let resolve_pos = events
            .iter()
            .position(|event| matches!(event, CapturedEvent::ResolvePathComplete(..)))
            .expect("ResolvePathComplete must fire");
        assert!(
            resolve_pos < complete_pos,
            "ResolvePathComplete must fire before Complete, got {events:?}"
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn resolve_missing_path_returns_invalid_arguments() {
        let (handle, store_handle_id) =
            load_handle("resolve-missing", Partition::from([0x22u8; 16])).await;
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));

        let status = resolve_path(
            LoreGlobalArgs::default(),
            LoreRevisionTreeResolvePathArgs {
                id: 8,
                handle,
                path: LoreString::from_str("no/such/path"),
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "resolving a missing path must fail");
        let events = sink.lock().unwrap().clone();
        let (node_id, _repository, _revision, error_code) =
            resolve_outcome(&events, 8).expect("ResolvePathComplete must fire for the caller id");
        assert_eq!(
            error_code,
            LoreErrorCode::InvalidArguments,
            "a missing path must report InvalidArguments, got {events:?}"
        );
        assert_eq!(
            node_id, INVALID_NODE,
            "a failed resolve must report the invalid-node sentinel, got {events:?}"
        );
        assert!(
            events.contains(&CapturedEvent::Complete(1)),
            "missing path must complete with status=1, got {events:?}"
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn resolve_non_utf8_path_returns_invalid_arguments() {
        let (handle, store_handle_id) =
            load_handle("resolve-non-utf8", Partition::from([0x33u8; 16])).await;
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));

        let status = resolve_path(
            LoreGlobalArgs::default(),
            LoreRevisionTreeResolvePathArgs {
                id: 9,
                handle,
                path: LoreString::from_bytes(&[0xFF, 0xFE, 0xFD]),
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "a non-UTF-8 path must fail");
        let events = sink.lock().unwrap().clone();
        let (node_id, _repository, _revision, error_code) =
            resolve_outcome(&events, 9).expect("ResolvePathComplete must fire for the caller id");
        assert_eq!(
            error_code,
            LoreErrorCode::InvalidArguments,
            "a non-UTF-8 path must report InvalidArguments, got {events:?}"
        );
        assert_eq!(
            node_id, INVALID_NODE,
            "a failed resolve must report the invalid-node sentinel, got {events:?}"
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn resolve_path_on_unknown_handle_emits_resolve_complete_with_invalid_arguments() {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));

        let status = resolve_path(
            LoreGlobalArgs::default(),
            LoreRevisionTreeResolvePathArgs {
                id: 10,
                handle: LoreRevisionTree::INVALID,
                path: LoreString::default(),
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "resolving against an unknown handle must fail");
        let events = sink.lock().unwrap().clone();
        let (node_id, _repository, _revision, error_code) = resolve_outcome(&events, 10)
            .expect("a handle miss must still emit ResolvePathComplete carrying the caller id");
        assert_eq!(
            error_code,
            LoreErrorCode::InvalidArguments,
            "a handle miss must report InvalidArguments, got {events:?}"
        );
        assert_eq!(
            node_id, INVALID_NODE,
            "a handle miss must report the invalid-node sentinel, got {events:?}"
        );
        assert!(
            events.contains(&CapturedEvent::Complete(1)),
            "a handle miss must complete with status=1, got {events:?}"
        );
    }

    #[tokio::test]
    async fn resolve_path_to_a_link_node_returns_the_link_node() {
        let repository = Partition::from([0x77u8; 16]);
        let (handle, store_handle_id) = load_handle("resolve-link-node", repository).await;
        let link_id = add_link(
            handle,
            "link",
            repository,
            Hash::from([0xABu8; 32]),
            ROOT_NODE,
        )
        .await;

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = resolve_path(
            LoreGlobalArgs::default(),
            LoreRevisionTreeResolvePathArgs {
                id: 11,
                handle,
                path: LoreString::from_str("link"),
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 0, "resolving to a link node must succeed");
        let events = sink.lock().unwrap().clone();
        let (node_id, resolved_repository, _revision, error_code) =
            resolve_outcome(&events, 11).expect("ResolvePathComplete must fire");
        assert_eq!(error_code, LoreErrorCode::None);
        assert_eq!(
            node_id, link_id,
            "must resolve to the link node itself, not its target, got {events:?}"
        );
        assert_eq!(
            resolved_repository, repository,
            "the link node lives in the handle's repository, got {events:?}"
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn resolve_path_through_a_link_returns_the_target_repository_and_revision() {
        let repository = Partition::from([0x88u8; 16]);
        let (handle, store_handle_id) = load_handle("resolve-link-through", repository).await;
        let target_revision = seal_target(handle, "doc").await;
        add_link(handle, "link", repository, target_revision, ROOT_NODE).await;

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = resolve_path(
            LoreGlobalArgs::default(),
            LoreRevisionTreeResolvePathArgs {
                id: 12,
                handle,
                path: LoreString::from_str("link/doc"),
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 0, "resolving through a link must succeed");
        let events = sink.lock().unwrap().clone();
        let (node_id, resolved_repository, resolved_revision, error_code) =
            resolve_outcome(&events, 12).expect("ResolvePathComplete must fire");
        assert_eq!(error_code, LoreErrorCode::None);
        assert_ne!(
            node_id, INVALID_NODE,
            "must resolve to the target's node, got {events:?}"
        );
        assert_eq!(
            resolved_repository, repository,
            "must report the link target's repository, got {events:?}"
        );
        assert_eq!(
            resolved_revision, target_revision,
            "must report the link target's revision, got {events:?}"
        );

        release(handle, store_handle_id);
    }
}
