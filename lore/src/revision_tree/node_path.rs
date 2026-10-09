// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_node_path` — reconstruct the full UTF-8 path for a
//! `NodeID` by walking parent pointers. Iteration costs scale with depth;
//! per-child listings deliberately skip this work to keep their memory flat.

use lore_base::error::InvalidArguments;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeNodePathEventData;
use lore_revision::interface::LoreError;
use lore_revision::interface::LoreString;
use lore_revision::node::NodeID;
use lore_revision::node::NodeIDExt;
use lore_revision::node::ROOT_NODE;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;

/// Arguments for `lore_revision_tree_node_path`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(node_path_impl)]
pub struct LoreRevisionTreeNodePathArgs {
    /// Per-call correlation id echoed back in events
    pub id: u64,
    /// Loaded revision-tree handle to read from
    pub handle: LoreRevisionTree,
    /// Node whose full UTF-8 path is reconstructed by walking parents
    pub node_id: NodeID,
}

#[error_set]
enum NodePathError {
    InvalidArguments,
}

impl EventError for NodePathError {
    fn translated(&self) -> LoreError {
        match self {
            NodePathError::InvalidArguments(_) => LoreError::InvalidArguments,
            NodePathError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

fn invalid(reason: &str) -> NodePathError {
    NodePathError::from(InvalidArguments {
        reason: reason.into(),
    })
}

/// Emit the id-carrying terminal for a failed `node_path`: an empty path plus
/// the populated `error_code`.
fn emit_node_path_error(id: u64, error_code: LoreErrorCode) {
    LoreEvent::RevisionTreeNodePath(LoreRevisionTreeNodePathEventData {
        id,
        error_code,
        ..Default::default()
    })
    .send();
}

/// Reconstruct the full UTF-8 path for a node id by walking parent pointers.
///
/// On success the caller receives `LORE_EVENT_REVISION_TREE_NODE_PATH` carrying
/// the path from the root to the node plus the `(repository, revision)` it was
/// reconstructed in (the handle's own — `node_path` does not follow links), and
/// `error_code = NONE`, before `Complete {status: 0}`. The root resolves to the
/// empty path; every non-root node has a non-empty name, so a node id that
/// resolves to an empty path (e.g. an unallocated slot) is rejected with
/// `error_code = INVALID_ARGUMENTS` rather than returning a bogus empty path. An
/// invalid or unknown node id likewise completes with
/// `error_code = INVALID_ARGUMENTS` — `State::node_path` does not distinguish an
/// out-of-range id from a read failure, so both collapse to it, as in
/// `list_children`. The verb materializes no bytes to disk.
pub async fn node_path(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeNodePathArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, node_path_impl).await
}

async fn node_path_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeNodePathArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        node_path,
        |args: &LoreRevisionTreeNodePathArgs| {
            emit_node_path_error(args.id, LoreErrorCode::InvalidArguments);
        },
        async move |internal, args: LoreRevisionTreeNodePathArgs| {
            let id = args.id;
            let node_id = args.node_id;

            if !node_id.is_valid_or_root_node_id() {
                emit_node_path_error(id, LoreErrorCode::InvalidArguments);
                return Err(invalid("node id is invalid"));
            }

            let access = internal.access_shared().await;
            let state = access.state();

            let Ok(path) = state
                .node_path(internal.repository_context.clone(), node_id)
                .await
            else {
                emit_node_path_error(id, LoreErrorCode::InvalidArguments);
                return Err(invalid("node id is unknown"));
            };

            if node_id != ROOT_NODE && path.is_empty() {
                emit_node_path_error(id, LoreErrorCode::InvalidArguments);
                return Err(invalid("node id does not resolve to a named node"));
            }

            LoreEvent::RevisionTreeNodePath(LoreRevisionTreeNodePathEventData {
                id,
                repository: internal.repository,
                revision: state.revision(),
                path: LoreString::from(path.as_str()),
                error_code: LoreErrorCode::None,
            })
            .send();
            Ok(())
        },
    )
    .await
}
