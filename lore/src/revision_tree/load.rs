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
use serde::Deserialize;
use serde::Serialize;

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
#[derive(Copy, Clone, Debug, Default, PartialEq, Deserialize, Serialize, LoreArgs)]
#[handler(load_impl)]
pub struct LoreRevisionTreeLoadArgs {
    /// Open storage handle the revision tree is loaded against
    pub store: LoreStore,
    /// Repository partition the loaded revision belongs to
    pub repository: Partition,
    /// Revision to open; `0` opens an empty tree for an initial commit
    pub revision_hash: Hash,
}

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

async fn load_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeLoadArgs,
    callback: LoreEventCallback,
) -> i32 {
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
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use lore_error_set::FfiError;
    use lore_revision::event::LoreEvent;
    use lore_revision::interface::LoreGlobalArgs;

    use super::*;
    use crate::revision_tree::handle as rt_handle;
    use crate::storage::handle as storage_handle;
    use crate::storage::store::in_memory_for_tests;

    #[derive(Debug, Clone, PartialEq)]
    enum CapturedEvent {
        Error(u32),
        Complete(i32),
        RevisionTreeLoaded(u64),
        Other(u32),
    }

    impl CapturedEvent {
        fn from_event(event: &LoreEvent) -> Self {
            match event {
                LoreEvent::Error(data) => Self::Error(data.error_type),
                LoreEvent::Complete(data) => Self::Complete(data.status),
                LoreEvent::RevisionTreeLoaded(data) => Self::RevisionTreeLoaded(data.handle_id),
                other => Self::Other(other.discriminant()),
            }
        }
    }

    fn make_callback(sink: Arc<Mutex<Vec<CapturedEvent>>>) -> LoreEventCallback {
        Some(Box::new(move |event: &LoreEvent| {
            sink.lock().unwrap().push(CapturedEvent::from_event(event));
        }))
    }

    fn loaded_handle_id(events: &[CapturedEvent]) -> Option<u64> {
        events.iter().find_map(|e| match e {
            CapturedEvent::RevisionTreeLoaded(id) => Some(*id),
            _ => None,
        })
    }

    /// With the parent gone the check unregisters the tree rather than merely reporting;
    /// with the parent alive it leaves it registered. The interleaving itself is not
    /// reachable — there is no controllable yield between the lookup and the registration.
    #[tokio::test]
    async fn a_load_withdraws_its_handle_when_the_parent_storage_handle_is_gone() {
        let store_handle = storage_handle::register(in_memory_for_tests("withdraw").await);
        let live = rt_handle::register(rt_handle::test_support::new_for_testing().await);
        assert!(
            !withdraw_if_parent_closed(store_handle, live),
            "a live parent must leave the handle alone"
        );
        assert!(rt_handle::lookup(live).is_some());
        rt_handle::unregister(live);

        let orphan = rt_handle::register(rt_handle::test_support::new_for_testing().await);
        storage_handle::unregister(store_handle);
        assert!(
            withdraw_if_parent_closed(store_handle, orphan),
            "a closed parent must withdraw the handle"
        );
        assert!(
            rt_handle::lookup(orphan).is_none(),
            "the withdrawn handle must not stay in the registry",
        );
    }

    #[tokio::test]
    async fn load_from_zero_hash_returns_empty_handle() {
        let store = in_memory_for_tests("load-zero-hash").await;
        let store_handle = storage_handle::register(store);
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let args = LoreRevisionTreeLoadArgs {
            store: store_handle,
            repository: Partition::from([0x11u8; 16]),
            revision_hash: Hash::default(),
        };

        let status = load(LoreGlobalArgs::default(), args, make_callback(sink.clone())).await;

        assert_eq!(status, 0);
        let events = sink.lock().unwrap().clone();
        let id = loaded_handle_id(&events)
            .unwrap_or_else(|| panic!("missing RevisionTreeLoaded event, got {events:?}"));
        assert_ne!(id, 0);
        assert!(
            rt_handle::lookup(crate::revision_tree::handle::LoreRevisionTree { handle_id: id })
                .is_some(),
            "loaded handle must be present in the registry",
        );
        assert!(
            events.contains(&CapturedEvent::Complete(0)),
            "Complete event must report status=0, got {events:?}"
        );
        let loaded_pos = events
            .iter()
            .position(|e| matches!(e, CapturedEvent::RevisionTreeLoaded(_)))
            .expect("RevisionTreeLoaded must be present");
        let complete_pos = events
            .iter()
            .position(|e| matches!(e, CapturedEvent::Complete(_)))
            .expect("Complete must be present");
        assert!(
            loaded_pos < complete_pos,
            "RevisionTreeLoaded must fire before Complete, got {events:?}"
        );

        rt_handle::unregister(crate::revision_tree::handle::LoreRevisionTree { handle_id: id });
        storage_handle::unregister(store_handle);
    }

    #[tokio::test]
    async fn load_from_unknown_hash_fails_with_not_found() {
        let store = in_memory_for_tests("load-unknown-hash").await;
        let store_handle = storage_handle::register(store);
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let args = LoreRevisionTreeLoadArgs {
            store: store_handle,
            repository: Partition::from([0x22u8; 16]),
            revision_hash: Hash::from([0x99u8; 32]),
        };

        let status = load(LoreGlobalArgs::default(), args, make_callback(sink.clone())).await;

        let expected_code = LoadError::from(NotFound).ffi_code();
        assert_eq!(status, expected_code);
        let events = sink.lock().unwrap().clone();
        assert!(
            events.contains(&CapturedEvent::Complete(expected_code)),
            "Complete event must report the not-found code, got {events:?}"
        );
        assert!(
            loaded_handle_id(&events).is_none(),
            "failed load must not emit RevisionTreeLoaded, got {events:?}"
        );

        storage_handle::unregister(store_handle);
    }

    #[tokio::test]
    async fn load_with_unknown_store_handle_fails_with_invalid_arguments() {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let args = LoreRevisionTreeLoadArgs {
            store: LoreStore::INVALID,
            repository: Partition::default(),
            revision_hash: Hash::default(),
        };

        let status = load(LoreGlobalArgs::default(), args, make_callback(sink.clone())).await;

        let expected_code = LoadError::from(InvalidArguments {
            reason: "storage handle is unknown or has been closed".into(),
        })
        .ffi_code();
        assert_eq!(status, expected_code);
        let events = sink.lock().unwrap().clone();
        assert!(
            events.contains(&CapturedEvent::Complete(expected_code)),
            "Complete event must report the invalid-arguments code, got {events:?}"
        );
    }

    #[tokio::test]
    async fn load_captures_parent_storage_handle_id_for_close_cascade() {
        let store = in_memory_for_tests("load-parent-capture").await;
        let store_handle = storage_handle::register(store);
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let args = LoreRevisionTreeLoadArgs {
            store: store_handle,
            repository: Partition::from([0x44u8; 16]),
            revision_hash: Hash::default(),
        };

        let status = load(LoreGlobalArgs::default(), args, make_callback(sink.clone())).await;
        assert_eq!(status, 0);

        let events = sink.lock().unwrap().clone();
        let id = loaded_handle_id(&events).expect("RevisionTreeLoaded event");
        let entry = rt_handle::REGISTRY
            .get(&id)
            .expect("registered revision tree handle");
        assert_eq!(
            entry.parent_storage_handle_id, store_handle.handle_id,
            "internal must record the parent storage handle id for the cascade"
        );
        drop(entry);

        rt_handle::unregister(crate::revision_tree::handle::LoreRevisionTree { handle_id: id });
        storage_handle::unregister(store_handle);
    }

    #[tokio::test]
    async fn load_two_revision_trees_against_different_repositories_on_one_storage_handle() {
        let store = in_memory_for_tests("load-multi-repo").await;
        let store_handle = storage_handle::register(store);

        let sink_a: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status_a = load(
            LoreGlobalArgs::default(),
            LoreRevisionTreeLoadArgs {
                store: store_handle,
                repository: Partition::from([0xAAu8; 16]),
                revision_hash: Hash::default(),
            },
            make_callback(sink_a.clone()),
        )
        .await;
        assert_eq!(status_a, 0);
        let id_a =
            loaded_handle_id(&sink_a.lock().unwrap().clone()).expect("first load event missing");

        let sink_b: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status_b = load(
            LoreGlobalArgs::default(),
            LoreRevisionTreeLoadArgs {
                store: store_handle,
                repository: Partition::from([0xBBu8; 16]),
                revision_hash: Hash::default(),
            },
            make_callback(sink_b.clone()),
        )
        .await;
        assert_eq!(status_b, 0);
        let id_b =
            loaded_handle_id(&sink_b.lock().unwrap().clone()).expect("second load event missing");

        assert_ne!(id_a, id_b, "two loads must produce distinct handles");
        let entry_a = rt_handle::REGISTRY
            .get(&id_a)
            .expect("handle A must be registered");
        let entry_b = rt_handle::REGISTRY
            .get(&id_b)
            .expect("handle B must be registered");
        assert_eq!(entry_a.repository, Partition::from([0xAAu8; 16]));
        assert_eq!(entry_b.repository, Partition::from([0xBBu8; 16]));
        assert_eq!(entry_a.parent_storage_handle_id, store_handle.handle_id);
        assert_eq!(entry_b.parent_storage_handle_id, store_handle.handle_id);
        drop(entry_a);
        drop(entry_b);

        rt_handle::unregister(crate::revision_tree::handle::LoreRevisionTree { handle_id: id_a });
        rt_handle::unregister(crate::revision_tree::handle::LoreRevisionTree { handle_id: id_b });
        storage_handle::unregister(store_handle);
    }
}
