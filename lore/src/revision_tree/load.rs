// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_load` — open a revision tree handle on a given
//! `(store, repository, revision_hash)` tuple. `revision_hash == 0` opens an
//! empty tree suitable for committing an initial revision. The verb returns
//! the new handle on the load-complete event; no per-call correlation `id`
//! is needed because the handle itself serves as the future correlation key.

use std::sync::Arc;

use lore_base::error::AddressNotFound;
use lore_base::error::InvalidArguments;
use lore_base::error::NotFound;
use lore_base::error::PayloadNotFound;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::errors::StateErrors;
use lore_revision::event::EventError;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeLoadedEventData;
use lore_revision::interface::LoreError;
use lore_revision::state::State;

use crate::call::no_repository_call;
use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::handle;
use crate::revision_tree::handle::LoreRevisionTree;
use crate::revision_tree::handle::RevisionTreeInternal;
use crate::revision_tree::handle::synth_repository_context;
use crate::storage::handle as storage_handle;
use crate::storage::handle::LoreStore;

/// Arguments for `lore_revision_tree_load`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(load_impl)]
pub struct LoreRevisionTreeLoadArgs {
    /// Open storage handle the revision tree is loaded against
    pub store: LoreStore,
    /// Repository partition the loaded revision belongs to
    pub repository: Partition,
    /// Revision to open; `0` opens an empty tree for an initial commit
    pub revision_hash: Hash,
}

#[lore_macro::test_pub]
#[error_set]
enum LoadError {
    InvalidArguments,
    AddressNotFound,
    PayloadNotFound,
    NotFound,
}

impl EventError for LoadError {
    fn translated(&self) -> LoreError {
        match self {
            LoadError::InvalidArguments(_) => LoreError::InvalidArguments,
            LoadError::AddressNotFound(_) => LoreError::AddressNotFound,
            LoadError::PayloadNotFound(_) => LoreError::PayloadNotFound,
            LoadError::NotFound(_) => LoreError::NotFound,
            LoadError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Map `State::deserialize` errors to the load verb's error surface. The
/// not-found family is forwarded variant-for-variant so the originating
/// address or payload hash is preserved; every other error collapses to an
/// internal error.
fn map_state_error(err: StateErrors) -> LoadError {
    match err {
        StateErrors::AddressNotFound(address_not_found) => {
            LoadError::AddressNotFound(address_not_found)
        }
        StateErrors::PayloadNotFound(payload_not_found) => {
            LoadError::PayloadNotFound(payload_not_found)
        }
        StateErrors::NotFound(not_found) => LoadError::NotFound(not_found),
        other => LoadError::internal_with_context(other, "State::deserialize"),
    }
}

/// Unregister a handle whose parent storage handle went away while the load was in
/// flight, reporting whether it did.
///
/// A connection teardown between the parent lookup and the registration sweeps a registry
/// this handle is not in yet, leaving it holding a store nobody will reclaim. Checking
/// *after* registering closes that window rather than moving it: the teardown removes the
/// storage handle before it sweeps, so either the sweep sees this entry or this sees the
/// storage handle gone.
#[lore_macro::test_pub]
fn withdraw_if_parent_closed(store: LoreStore, revision_tree: LoreRevisionTree) -> bool {
    if storage_handle::lookup(store).is_some() {
        return false;
    }
    handle::unregister(revision_tree);
    true
}

/// Open a memory-based revision tree handle on the given `(store, repository, revision_hash)`.
///
/// On success the caller receives `LORE_EVENT_REVISION_TREE_LOADED` carrying
/// the new handle id before `Complete {status: 0}`. On failure, one
/// `LORE_EVENT_ERROR` fires followed by `Complete {status: 1}` and no
/// handle is registered.
pub async fn load(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeLoadArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, load_impl).await
}

fn load_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeLoadArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    no_repository_call(globals, callback, args, load, async move |args| {
        let store_internal = storage_handle::lookup(args.store).ok_or_else(|| {
            LoadError::from(InvalidArguments {
                reason: "storage handle is unknown or has been closed".into(),
            })
        })?;

        let repository_context = synth_repository_context(&store_internal, args.repository).await;

        let state = State::deserialize(repository_context.clone(), args.revision_hash)
            .await
            .map_err(map_state_error)?;

        let internal = Arc::new(RevisionTreeInternal::new(
            store_internal,
            args.store.handle_id,
            args.repository,
            repository_context,
            state,
        ));
        let revision_tree_handle = handle::register(internal);
        if withdraw_if_parent_closed(args.store, revision_tree_handle) {
            return Err(LoadError::from(InvalidArguments {
                reason: "storage handle was closed while the revision tree was loading".into(),
            }));
        }
        LoreEvent::RevisionTreeLoaded(LoreRevisionTreeLoadedEventData {
            handle_id: revision_tree_handle.handle_id,
        })
        .send();
        Ok::<(), LoadError>(())
    })
}
