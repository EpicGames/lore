// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_list_children` — stream the children of a directory
//! node as per-entry events terminated by `Complete`.

use std::sync::Arc;

use lore_base::error::InvalidArguments;
use lore_base::types::Hash;
use lore_base::types::RepositoryId;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeChildEventData;
use lore_revision::event::revision_tree::LoreRevisionTreeListChildrenBeginEventData;
use lore_revision::interface::LoreError;
use lore_revision::interface::LoreNodeType;
use lore_revision::interface::LoreString;
use lore_revision::node::Node;
use lore_revision::node::NodeID;
use lore_revision::node::NodeIDExt;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::MAX_LINK_DEPTH;
use lore_revision::state::State;
use lore_revision::state::StateNodeChildrenWithNameIterator;
use serde::Deserialize;
use serde::Serialize;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;

/// Arguments for `lore_revision_tree_list_children`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Deserialize, Serialize, LoreArgs)]
#[handler(list_children_impl)]
pub struct LoreRevisionTreeListChildrenArgs {
    /// Per-call correlation id echoed back in events
    pub id: u64,
    /// Loaded revision-tree handle to read from
    pub handle: LoreRevisionTree,
    /// Directory node whose children are streamed
    pub parent_node_id: NodeID,
}

#[error_set]
enum ListChildrenError {
    InvalidArguments,
}

impl EventError for ListChildrenError {
    fn translated(&self) -> LoreError {
        match self {
            ListChildrenError::InvalidArguments(_) => LoreError::InvalidArguments,
            ListChildrenError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Emit one child entry. `kind` is derived from the node's flags.
fn emit_child(id: u64, node_id: NodeID, name: &str, parent_id: NodeID, node: &Node) {
    let kind = if node.is_file() {
        LoreNodeType::File as u32
    } else if node.is_link() {
        LoreNodeType::Link as u32
    } else {
        LoreNodeType::Directory as u32
    };
    LoreEvent::RevisionTreeChild(LoreRevisionTreeChildEventData {
        id,
        node_id,
        name: LoreString::from(name),
        parent_id,
        kind,
        mode: node.mode,
        size: node.size,
        address: node.address,
        error_code: LoreErrorCode::None,
    })
    .send();
}

/// Emit the one-time list header. On success it carries the `(repository,
/// revision)` the children belong to with `error_code = None`; on failure it
/// carries the failure code with a zeroed repository/revision and no children
/// follow. This is the verb's id-carrying terminal.
fn emit_begin(id: u64, repository: RepositoryId, revision: Hash, error_code: LoreErrorCode) {
    LoreEvent::RevisionTreeListChildrenBegin(LoreRevisionTreeListChildrenBeginEventData {
        id,
        repository,
        revision,
        error_code,
    })
    .send();
}

fn invalid(reason: &str) -> ListChildrenError {
    ListChildrenError::from(InvalidArguments {
        reason: reason.into(),
    })
}

/// Resolve `parent_id` to the directory whose children should be listed,
/// following links to their target revision. Returns `Ok(None)` when the id is
/// unknown, resolves to a non-directory (leaf) node, or points through a link
/// to a node that no longer exists. Following is bounded by `MAX_LINK_DEPTH`: a
/// chain longer than that, or a cycle, fails with `InvalidArguments` rather than
/// looping forever.
async fn resolve_listing_target(
    mut state: Arc<State>,
    mut repository: Arc<RepositoryContext>,
    mut node_id: NodeID,
) -> Result<Option<(Arc<State>, Arc<RepositoryContext>, NodeID)>, ListChildrenError> {
    let mut link_depth = 0usize;
    loop {
        let Ok(node) = state.node(repository.clone(), node_id).await else {
            return Ok(None);
        };
        if node.is_directory() {
            return Ok(Some((state, repository, node_id)));
        }
        if !node.is_link() {
            return Ok(None);
        }
        if link_depth >= MAX_LINK_DEPTH {
            return Err(invalid("parent node id resolves through too many links"));
        }
        link_depth += 1;
        let link = node.linked_node();
        repository = Arc::new(repository.to_link_context(link.repository).await);
        state = State::deserialize(repository.clone(), link.revision)
            .await
            .map_err(|error| {
                ListChildrenError::internal_with_context(error, "deserialize link target state")
            })?;
        node_id = link.node;
    }
}

/// Stream the children of a directory node.
///
/// Emits a `RevisionTreeListChildrenBegin` header carrying the target's
/// `(repository, revision)`, then one `RevisionTreeChild` per child, then
/// `Complete {status: 0}`. An empty directory emits the header then no children.
/// A link parent is resolved to its target, so the header carries the target's
/// `(repository, revision)` and the children are the target's; link following is
/// bounded by `MAX_LINK_DEPTH`, so a chain longer than that or a cycle is
/// rejected with `INVALID_ARGUMENTS` instead of looping forever. An unknown
/// parent node id or a non-directory (leaf) parent emits the header with
/// `error_code = INVALID_ARGUMENTS` and a zeroed repository/revision, then fails.
/// Iteration is streaming: at most one child is held in memory at a time. The
/// verb materializes no bytes to disk.
///
/// Node ids are opaque values issued by the API. An id the API never issued
/// that happens to land on an unallocated slot of an existing block reads back
/// as an empty directory rather than `INVALID_ARGUMENTS`; only ids resolving to
/// a non-existent block or the reserved invalid sentinel are rejected.
///
/// The header is the only id-carrying terminal: a failure that surfaces after a
/// successful header has fired — a tree-block read error mid-iteration — is
/// reported on the trailing `Error`/`Complete{status:1}` pair, which carry no
/// `id`. Such a mid-stream failure is therefore not attributable to this call on
/// a multiplexed transport; callers treat a non-zero `Complete` after a
/// successful header as "the listing was truncated".
pub async fn list_children(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeListChildrenArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, list_children_impl).await
}

async fn list_children_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeListChildrenArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let miss_id = args.id;
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        list_children,
        move || {
            emit_begin(
                miss_id,
                RepositoryId::default(),
                Hash::default(),
                LoreErrorCode::InvalidArguments,
            );
        },
        async move |internal, args: LoreRevisionTreeListChildrenArgs| {
            let id = args.id;
            let parent_id = args.parent_node_id;

            if !parent_id.is_valid_or_root_node_id() {
                emit_begin(
                    id,
                    RepositoryId::default(),
                    Hash::default(),
                    LoreErrorCode::InvalidArguments,
                );
                return Err(invalid("parent node id is invalid"));
            }

            let (list_state, list_repository, list_node) = match resolve_listing_target(
                internal.state.clone(),
                internal.repository_context.clone(),
                parent_id,
            )
            .await
            {
                Ok(Some(target)) => target,
                Ok(None) => {
                    emit_begin(
                        id,
                        RepositoryId::default(),
                        Hash::default(),
                        LoreErrorCode::InvalidArguments,
                    );
                    return Err(invalid("parent node id is unknown or not a directory"));
                }
                Err(error) => {
                    let error_code = match error {
                        ListChildrenError::InvalidArguments(_) => LoreErrorCode::InvalidArguments,
                        ListChildrenError::Internal(_) => LoreErrorCode::Internal,
                    };
                    emit_begin(id, RepositoryId::default(), Hash::default(), error_code);
                    return Err(error);
                }
            };

            // Capture the target's identity before the iterator consumes the state/context.
            let begin_repository = list_repository.id;
            let begin_revision = list_state.revision();

            let mut children = match StateNodeChildrenWithNameIterator::new(
                list_state,
                list_repository,
                list_node,
            )
            .await
            {
                Ok(children) => children,
                Err(error) => {
                    emit_begin(
                        id,
                        RepositoryId::default(),
                        Hash::default(),
                        LoreErrorCode::Internal,
                    );
                    return Err(ListChildrenError::internal_with_context(
                        error,
                        "StateNodeChildrenWithNameIterator::new",
                    ));
                }
            };

            emit_begin(id, begin_repository, begin_revision, LoreErrorCode::None);

            loop {
                match children.next().await {
                    Ok(Some((child_id, child_node, name))) => {
                        emit_child(id, child_id, &name, list_node, &child_node);
                    }
                    Ok(None) => break,
                    Err(error) => {
                        return Err(ListChildrenError::internal_with_context(
                            error,
                            "StateNodeChildrenWithNameIterator::next",
                        ));
                    }
                }
            }
            Ok(())
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::Mutex;

    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::node::INVALID_NODE;
    use lore_revision::node::NodeFlags;
    use lore_revision::node::ROOT_NODE;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryWriteToken;
    use lore_revision::state::State;

    use super::*;
    use crate::revision_tree::handle as rt_handle;
    use crate::revision_tree::load::LoreRevisionTreeLoadArgs;
    use crate::revision_tree::load::load;
    use crate::storage::handle as storage_handle;
    use crate::storage::store::in_memory_for_tests;

    #[derive(Debug, Clone, PartialEq)]
    enum CapturedEvent {
        Error(u32),
        Complete(i32),
        RevisionTreeLoaded(u64),
        Child {
            id: u64,
            name: String,
            kind: u32,
            mode: u16,
            size: u64,
            address: Address,
            error_code: LoreErrorCode,
        },
        Begin {
            id: u64,
            repository: RepositoryId,
            revision: Hash,
            error_code: LoreErrorCode,
        },
        Other(u32),
    }

    impl CapturedEvent {
        fn from_event(event: &LoreEvent) -> Self {
            match event {
                LoreEvent::Error(data) => Self::Error(data.error_type),
                LoreEvent::Complete(data) => Self::Complete(data.status),
                LoreEvent::RevisionTreeLoaded(data) => Self::RevisionTreeLoaded(data.handle_id),
                LoreEvent::RevisionTreeChild(data) => Self::Child {
                    id: data.id,
                    name: data.name.as_str().to_string(),
                    kind: data.kind,
                    mode: data.mode,
                    size: data.size,
                    address: data.address,
                    error_code: data.error_code,
                },
                LoreEvent::RevisionTreeListChildrenBegin(data) => Self::Begin {
                    id: data.id,
                    repository: data.repository,
                    revision: data.revision,
                    error_code: data.error_code,
                },
                other => Self::Other(other.discriminant()),
            }
        }
    }

    fn make_callback(sink: Arc<Mutex<Vec<CapturedEvent>>>) -> LoreEventCallback {
        Some(Box::new(move |event: &LoreEvent| {
            sink.lock().unwrap().push(CapturedEvent::from_event(event));
        }))
    }

    fn children(events: &[CapturedEvent]) -> Vec<(u64, String, u32, LoreErrorCode)> {
        events
            .iter()
            .filter_map(|event| match event {
                CapturedEvent::Child {
                    id,
                    name,
                    kind,
                    error_code,
                    ..
                } => Some((*id, name.clone(), *kind, *error_code)),
                _ => None,
            })
            .collect()
    }

    fn begin(events: &[CapturedEvent]) -> Option<(u64, RepositoryId, Hash, LoreErrorCode)> {
        events.iter().find_map(|event| match event {
            CapturedEvent::Begin {
                id,
                repository,
                revision,
                error_code,
            } => Some((*id, *repository, *revision, *error_code)),
            _ => None,
        })
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

    /// Add a child directly via `State::node_add` (the staging-path allocator)
    /// so `list_children` has a populated tree to read; the revision-tree `add`
    /// verb does not exist yet. Returns the new node id.
    async fn add_child(handle: LoreRevisionTree, name: &str, is_file: bool) -> NodeID {
        let (state, repository) = {
            let entry = rt_handle::REGISTRY
                .get(&handle.handle_id)
                .expect("handle registered");
            (entry.state.clone(), entry.repository_context.clone())
        };
        let flags = if is_file { NodeFlags::File.bits() } else { 0 };
        let node = Node {
            flags,
            ..Default::default()
        };
        state
            .node_add(repository, ROOT_NODE, node, name)
            .await
            .expect("node_add must succeed")
    }

    /// Add a file with explicit metadata so the per-child event's `mode`,
    /// `size`, and `address` can be verified. Returns the new node id.
    async fn add_file_with_metadata(
        handle: LoreRevisionTree,
        name: &str,
        mode: u16,
        size: u64,
        address: Address,
    ) -> NodeID {
        let (state, repository) = {
            let entry = rt_handle::REGISTRY
                .get(&handle.handle_id)
                .expect("handle registered");
            (entry.state.clone(), entry.repository_context.clone())
        };
        let node = Node {
            flags: NodeFlags::File.bits(),
            mode,
            size,
            address,
            ..Default::default()
        };
        state
            .node_add(repository, ROOT_NODE, node, name)
            .await
            .expect("node_add must succeed")
    }

    fn handle_state(handle: LoreRevisionTree) -> (Arc<State>, Arc<RepositoryContext>) {
        let entry = rt_handle::REGISTRY
            .get(&handle.handle_id)
            .expect("handle registered");
        (entry.state.clone(), entry.repository_context.clone())
    }

    /// Add a link node under root targeting `(repository, revision, target_node)`.
    /// Returns the link node id.
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

    /// Serialize the handle's current live state to a committed revision a link
    /// can point at. Returns the revision hash. Node ids are positional, so an
    /// id added before the seal resolves to the same node in the sealed revision.
    async fn seal(handle: LoreRevisionTree) -> Hash {
        let (state, repository_context) = handle_state(handle);
        let token = RepositoryWriteToken::acquire(Path::new("link-target")).await;
        state
            .serialize(repository_context, &token)
            .await
            .expect("serialize must succeed")
    }

    fn release(handle: LoreRevisionTree, store_handle_id: u64) {
        rt_handle::unregister(handle);
        storage_handle::unregister(crate::storage::handle::LoreStore {
            handle_id: store_handle_id,
        });
    }

    #[tokio::test]
    async fn list_children_empty_directory_emits_begin_then_complete() {
        let (handle, store_handle_id) =
            load_handle("lc-empty", Partition::from([0x11u8; 16])).await;
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));

        let status = list_children(
            LoreGlobalArgs::default(),
            LoreRevisionTreeListChildrenArgs {
                id: 1,
                handle,
                parent_node_id: ROOT_NODE,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 0, "listing an empty directory must succeed");
        let events = sink.lock().unwrap().clone();
        assert!(
            children(&events).is_empty(),
            "an empty directory must emit no child events, got {events:?}"
        );
        assert!(
            events.contains(&CapturedEvent::Complete(0)),
            "must complete with status=0, got {events:?}"
        );
        let (begin_id, begin_repository, _begin_revision, begin_error) =
            begin(&events).expect("begin event must fire");
        assert_eq!(begin_id, 1);
        assert_eq!(begin_repository, Partition::from([0x11u8; 16]));
        assert_eq!(begin_error, LoreErrorCode::None);

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn list_children_emits_one_event_per_child() {
        let (handle, store_handle_id) =
            load_handle("lc-children", Partition::from([0x22u8; 16])).await;
        add_child(handle, "alpha", true).await;
        add_child(handle, "beta", true).await;
        add_child(handle, "subdir", false).await;

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = list_children(
            LoreGlobalArgs::default(),
            LoreRevisionTreeListChildrenArgs {
                id: 2,
                handle,
                parent_node_id: ROOT_NODE,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 0);
        let events = sink.lock().unwrap().clone();
        let observed: BTreeSet<(String, u32)> = children(&events)
            .into_iter()
            .map(|(id, name, kind, error_code)| {
                assert_eq!(id, 2, "child event must carry the caller id");
                assert_eq!(error_code, LoreErrorCode::None, "child event must succeed");
                (name, kind)
            })
            .collect();
        let expected: BTreeSet<(String, u32)> = [
            ("alpha".to_string(), LoreNodeType::File as u32),
            ("beta".to_string(), LoreNodeType::File as u32),
            ("subdir".to_string(), LoreNodeType::Directory as u32),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            observed, expected,
            "every child must be emitted once with its name and kind, got {events:?}"
        );
        assert!(events.contains(&CapturedEvent::Complete(0)));
        let (begin_id, begin_repository, _begin_revision, begin_error) =
            begin(&events).expect("begin event must fire");
        assert_eq!(begin_id, 2);
        assert_eq!(begin_repository, Partition::from([0x22u8; 16]));
        assert_eq!(begin_error, LoreErrorCode::None);

        let begin_index = events
            .iter()
            .position(|event| matches!(event, CapturedEvent::Begin { .. }))
            .expect("begin event must fire");
        let first_child_index = events
            .iter()
            .position(|event| matches!(event, CapturedEvent::Child { .. }))
            .expect("at least one child must fire");
        assert!(
            begin_index < first_child_index,
            "the begin header must precede every child, got {events:?}"
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn list_children_leaf_node_returns_invalid_arguments() {
        let (handle, store_handle_id) = load_handle("lc-leaf", Partition::from([0x33u8; 16])).await;
        let file_id = add_child(handle, "file", true).await;

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = list_children(
            LoreGlobalArgs::default(),
            LoreRevisionTreeListChildrenArgs {
                id: 3,
                handle,
                parent_node_id: file_id,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "listing a leaf node must fail");
        let events = sink.lock().unwrap().clone();
        let (begin_id, _begin_repository, _begin_revision, begin_error) =
            begin(&events).expect("begin event must fire");
        assert_eq!(begin_id, 3);
        assert_eq!(
            begin_error,
            LoreErrorCode::InvalidArguments,
            "a leaf parent must report InvalidArguments on the begin event, got {events:?}"
        );
        assert!(
            children(&events).is_empty(),
            "a failed listing must emit no children, got {events:?}"
        );
        assert!(events.contains(&CapturedEvent::Complete(1)));

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn list_children_unknown_node_returns_invalid_arguments() {
        let (handle, store_handle_id) =
            load_handle("lc-unknown", Partition::from([0x44u8; 16])).await;

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = list_children(
            LoreGlobalArgs::default(),
            LoreRevisionTreeListChildrenArgs {
                id: 4,
                handle,
                parent_node_id: INVALID_NODE,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "listing an unknown node must fail");
        let events = sink.lock().unwrap().clone();
        let (begin_id, _begin_repository, _begin_revision, begin_error) =
            begin(&events).expect("begin event must fire");
        assert_eq!(begin_id, 4);
        assert_eq!(
            begin_error,
            LoreErrorCode::InvalidArguments,
            "an unknown parent must report InvalidArguments on the begin event, got {events:?}"
        );
        assert!(children(&events).is_empty());
        assert!(events.contains(&CapturedEvent::Complete(1)));

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn list_children_streams_every_child_of_a_large_directory() {
        let (handle, store_handle_id) =
            load_handle("lc-large", Partition::from([0x55u8; 16])).await;
        const CHILD_COUNT: usize = 1000;
        for index in 0..CHILD_COUNT {
            add_child(handle, &format!("child-{index}"), true).await;
        }

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = list_children(
            LoreGlobalArgs::default(),
            LoreRevisionTreeListChildrenArgs {
                id: 5,
                handle,
                parent_node_id: ROOT_NODE,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 0);
        let events = sink.lock().unwrap().clone();
        let emitted = children(&events);
        assert_eq!(
            emitted.len(),
            CHILD_COUNT,
            "every child of a large directory must be emitted exactly once"
        );
        let names: BTreeSet<String> = emitted
            .into_iter()
            .map(|(_, name, _, error_code)| {
                assert_eq!(error_code, LoreErrorCode::None);
                name
            })
            .collect();
        assert_eq!(names.len(), CHILD_COUNT, "child names must be distinct");

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn list_children_child_event_carries_node_metadata() {
        let (handle, store_handle_id) =
            load_handle("lc-metadata", Partition::from([0x66u8; 16])).await;
        let address = Address {
            hash: Hash::from([0x42u8; 32]),
            context: Context::default(),
        };
        add_file_with_metadata(handle, "doc.md", 0o644, 1234, address).await;

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = list_children(
            LoreGlobalArgs::default(),
            LoreRevisionTreeListChildrenArgs {
                id: 6,
                handle,
                parent_node_id: ROOT_NODE,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 0);
        let events = sink.lock().unwrap().clone();
        let entry = events
            .iter()
            .find_map(|event| match event {
                CapturedEvent::Child {
                    name,
                    kind,
                    mode,
                    size,
                    address,
                    error_code,
                    ..
                } if name == "doc.md" => Some((*kind, *mode, *size, *address, *error_code)),
                _ => None,
            })
            .expect("child event for doc.md");
        let (kind, mode, size, child_address, error_code) = entry;
        assert_eq!(error_code, LoreErrorCode::None);
        assert_eq!(kind, LoreNodeType::File as u32);
        assert_eq!(mode, 0o644, "mode must be forwarded, got {events:?}");
        assert_eq!(size, 1234, "size must be forwarded, got {events:?}");
        assert_eq!(
            child_address, address,
            "address must be forwarded, got {events:?}"
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn list_children_reports_a_link_child_as_a_link() {
        let repository = Partition::from([0x77u8; 16]);
        let (handle, store_handle_id) = load_handle("lc-link-child", repository).await;
        add_link(
            handle,
            "link",
            repository,
            Hash::from([0xABu8; 32]),
            ROOT_NODE,
        )
        .await;

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = list_children(
            LoreGlobalArgs::default(),
            LoreRevisionTreeListChildrenArgs {
                id: 7,
                handle,
                parent_node_id: ROOT_NODE,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 0);
        let events = sink.lock().unwrap().clone();
        let entries: BTreeSet<(String, u32)> = children(&events)
            .into_iter()
            .map(|(_, name, kind, _)| (name, kind))
            .collect();
        assert!(
            entries.contains(&("link".to_string(), LoreNodeType::Link as u32)),
            "a link child must be reported as a link, got {events:?}"
        );
        let (_, begin_repository, _begin_revision, begin_error) =
            begin(&events).expect("begin event must fire");
        assert_eq!(begin_error, LoreErrorCode::None);
        assert_eq!(begin_repository, repository);

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn list_children_through_a_link_lists_the_target_children() {
        let repository = Partition::from([0x88u8; 16]);
        let (handle, store_handle_id) = load_handle("lc-link-through", repository).await;
        let target_revision = seal_target(handle, "doc").await;
        let link_id = add_link(handle, "link", repository, target_revision, ROOT_NODE).await;

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = list_children(
            LoreGlobalArgs::default(),
            LoreRevisionTreeListChildrenArgs {
                id: 8,
                handle,
                parent_node_id: link_id,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 0, "listing through a link must succeed");
        let events = sink.lock().unwrap().clone();
        let (_, begin_repository, begin_revision, begin_error) =
            begin(&events).expect("begin event must fire");
        assert_eq!(begin_error, LoreErrorCode::None);
        assert_eq!(
            begin_repository, repository,
            "begin must report the link target's repository, got {events:?}"
        );
        assert_eq!(
            begin_revision, target_revision,
            "begin must report the link target's revision, got {events:?}"
        );
        let names: BTreeSet<String> = children(&events)
            .into_iter()
            .map(|(_, name, _, _)| name)
            .collect();
        assert!(
            names.contains("doc"),
            "must list the link target's children, got {events:?}"
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn list_children_on_unknown_handle_emits_begin_with_invalid_arguments() {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));

        let status = list_children(
            LoreGlobalArgs::default(),
            LoreRevisionTreeListChildrenArgs {
                id: 9,
                handle: LoreRevisionTree::INVALID,
                parent_node_id: ROOT_NODE,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "listing against an unknown handle must fail");
        let events = sink.lock().unwrap().clone();
        let (begin_id, _begin_repository, _begin_revision, begin_error) =
            begin(&events).expect("a handle miss must still emit the begin header carrying the id");
        assert_eq!(begin_id, 9);
        assert_eq!(
            begin_error,
            LoreErrorCode::InvalidArguments,
            "a handle miss must report InvalidArguments on the begin event, got {events:?}"
        );
        assert!(children(&events).is_empty());
        assert!(events.contains(&CapturedEvent::Complete(1)));
    }

    #[tokio::test]
    async fn list_children_through_a_link_to_a_leaf_returns_invalid_arguments() {
        let repository = Partition::from([0x99u8; 16]);
        let (handle, store_handle_id) = load_handle("lc-link-leaf", repository).await;
        let file_id = add_child(handle, "file", true).await;
        let target_revision = seal(handle).await;
        let link_id = add_link(handle, "link", repository, target_revision, file_id).await;

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = list_children(
            LoreGlobalArgs::default(),
            LoreRevisionTreeListChildrenArgs {
                id: 10,
                handle,
                parent_node_id: link_id,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "a link resolving to a leaf must fail");
        let events = sink.lock().unwrap().clone();
        let (_, _begin_repository, _begin_revision, begin_error) =
            begin(&events).expect("begin event must fire");
        assert_eq!(
            begin_error,
            LoreErrorCode::InvalidArguments,
            "a link resolving to a leaf must report InvalidArguments, got {events:?}"
        );
        assert!(children(&events).is_empty());

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn list_children_through_a_dangling_link_returns_invalid_arguments() {
        let repository = Partition::from([0xAAu8; 16]);
        let (handle, store_handle_id) = load_handle("lc-link-dangling", repository).await;
        let target_revision = seal_target(handle, "doc").await;
        let link_id = add_link(handle, "link", repository, target_revision, 1_000_000).await;

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = list_children(
            LoreGlobalArgs::default(),
            LoreRevisionTreeListChildrenArgs {
                id: 11,
                handle,
                parent_node_id: link_id,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "a link to a missing node must fail");
        let events = sink.lock().unwrap().clone();
        let (_, _begin_repository, _begin_revision, begin_error) =
            begin(&events).expect("begin event must fire");
        assert_eq!(
            begin_error,
            LoreErrorCode::InvalidArguments,
            "a link to a missing node must report InvalidArguments, got {events:?}"
        );
        assert!(children(&events).is_empty());

        release(handle, store_handle_id);
    }
}
