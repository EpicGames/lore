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

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;

/// Arguments for `lore_revision_tree_list_children`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
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
        staged_action: node.staged_action() as u32,
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
        repository = repository.to_link_context(link.repository).await;
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
/// reported on the trailing `Complete{status:InvalidArguments}`, which carries no `id`. Such a
/// mid-stream failure is therefore not attributable to this call on a
/// multiplexed transport; callers treat a non-zero `Complete` after a successful
/// header as "the listing was truncated".
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
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        list_children,
        |args: &LoreRevisionTreeListChildrenArgs| {
            emit_begin(
                args.id,
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

            let access = internal.access_shared().await;
            let (list_state, list_repository, list_node) = match resolve_listing_target(
                access.state(),
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
