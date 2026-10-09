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

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;

/// Arguments for `lore_revision_tree_resolve_path`.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
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
/// the root node. A path that does not resolve to a node completes with
/// `error_code = INVALID_ARGUMENTS`. A path that is not valid UTF-8 never
/// reaches the verb — the entry point rejects the call before dispatching it, so
/// no `RESOLVE_PATH_COMPLETE` fires. The verb materializes no bytes to disk.
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
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        resolve_path,
        |args: &LoreRevisionTreeResolvePathArgs| {
            emit_resolve_complete(
                args.id,
                INVALID_NODE,
                RepositoryId::default(),
                Hash::default(),
                LoreErrorCode::InvalidArguments,
            );
        },
        async move |internal, args: LoreRevisionTreeResolvePathArgs| {
            let id = args.id;
            let path = args.path.as_str();
            let access = internal.access_shared().await;
            let state = access.state();

            if path.is_empty() {
                emit_resolve_complete(
                    id,
                    ROOT_NODE,
                    internal.repository,
                    state.revision(),
                    LoreErrorCode::None,
                );
                return Ok(());
            }

            match state
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
