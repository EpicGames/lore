// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_node_info` — fetch the per-node record for a single
//! `NodeID`. The record is uniform across every node, including the root;
//! revision-level metadata is served separately by `lore_revision_tree_info`.

use lore_base::error::InvalidArguments;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeNodeInfoEventData;
use lore_revision::interface::LoreError;
use lore_revision::interface::LoreNodeType;
use lore_revision::interface::LoreString;
use lore_revision::node::INVALID_NODE;
use lore_revision::node::NodeID;
use lore_revision::node::NodeIDExt;
use lore_revision::node::ROOT_NODE;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;

/// Arguments for `lore_revision_tree_node_info`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(node_info_impl)]
pub struct LoreRevisionTreeNodeInfoArgs {
    /// Per-call correlation id echoed back in events
    pub id: u64,
    /// Loaded revision-tree handle to read from
    pub handle: LoreRevisionTree,
    /// Node whose record is fetched
    pub node_id: NodeID,
}

#[error_set]
enum NodeInfoError {
    InvalidArguments,
}

impl EventError for NodeInfoError {
    fn translated(&self) -> LoreError {
        match self {
            NodeInfoError::InvalidArguments(_) => LoreError::InvalidArguments,
            NodeInfoError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

fn invalid(reason: &str) -> NodeInfoError {
    NodeInfoError::from(InvalidArguments {
        reason: reason.into(),
    })
}

/// Emit the id-carrying terminal for a failed `node_info`: a record with a
/// zeroed body and the populated `error_code`.
fn emit_node_info_error(id: u64, error_code: LoreErrorCode) {
    LoreEvent::RevisionTreeNodeInfo(LoreRevisionTreeNodeInfoEventData {
        id,
        node_id: INVALID_NODE,
        parent_id: INVALID_NODE,
        error_code,
        ..Default::default()
    })
    .send();
}

/// Fetch the per-node record for a single node id.
///
/// On success the caller receives `LORE_EVENT_REVISION_TREE_NODE_INFO` carrying
/// the node's name, kind, mode, size, address, preserved `file_id`, the
/// `(repository, revision)` it belongs to (the handle's own — `node_info` does
/// not follow links, so a link id reports the link node itself), and
/// `error_code = NONE`, before `Complete {status: 0}`. The record is uniform
/// across every node, including the root (which reports an empty name without a
/// name-table read); revision-level metadata is served by
/// `lore_revision_tree_info`, not here. An invalid or unknown node id completes
/// with `error_code = INVALID_ARGUMENTS`; a name-table read failure on a
/// non-root node completes with `error_code = INTERNAL`. The verb materializes
/// no bytes to disk.
///
/// Node ids are opaque values issued by the API. An id the API never issued
/// that happens to land on an unallocated slot of an existing block reads back
/// as a zeroed record with an empty name; since every non-root node has a
/// non-empty name, such an id is rejected with `INVALID_ARGUMENTS` rather than
/// reported as a bogus record (consistent with `node_path`).
pub async fn node_info(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeNodeInfoArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, node_info_impl).await
}

async fn node_info_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeNodeInfoArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        node_info,
        |args: &LoreRevisionTreeNodeInfoArgs| {
            emit_node_info_error(args.id, LoreErrorCode::InvalidArguments);
        },
        async move |internal, args: LoreRevisionTreeNodeInfoArgs| {
            let id = args.id;
            let node_id = args.node_id;

            if !node_id.is_valid_or_root_node_id() {
                emit_node_info_error(id, LoreErrorCode::InvalidArguments);
                return Err(invalid("node id is invalid"));
            }

            let access = internal.access_shared().await;
            let state = access.state();

            let Ok(node) = state
                .node(internal.repository_context.clone(), node_id)
                .await
            else {
                emit_node_info_error(id, LoreErrorCode::InvalidArguments);
                return Err(invalid("node id is unknown"));
            };

            let name = if node_id == ROOT_NODE {
                String::new()
            } else {
                match state
                    .node_name_clone(internal.repository_context.clone(), node_id)
                    .await
                {
                    Ok(name) => name,
                    Err(error) => {
                        emit_node_info_error(id, LoreErrorCode::Internal);
                        return Err(NodeInfoError::internal_with_context(
                            error,
                            "State::node_name_clone",
                        ));
                    }
                }
            };

            if node_id != ROOT_NODE && name.is_empty() {
                emit_node_info_error(id, LoreErrorCode::InvalidArguments);
                return Err(invalid("node id does not resolve to a named node"));
            }

            let kind = if node.is_file() {
                LoreNodeType::File as u32
            } else if node.is_link() {
                LoreNodeType::Link as u32
            } else {
                LoreNodeType::Directory as u32
            };

            LoreEvent::RevisionTreeNodeInfo(LoreRevisionTreeNodeInfoEventData {
                id,
                node_id,
                repository: internal.repository,
                revision: state.revision(),
                name: LoreString::from(name.as_str()),
                parent_id: node.parent,
                kind,
                staged_action: node.staged_action() as u32,
                mode: node.mode,
                size: node.size,
                address: node.address,
                file_id: node.address.context,
                error_code: LoreErrorCode::None,
            })
            .send();
            Ok(())
        },
    )
    .await
}
