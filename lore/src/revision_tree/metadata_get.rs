// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_metadata_get` — read a metadata value by key. The
//! verb consults the handle's pending edits first, then falls back to the
//! loaded revision's frozen Metadata fragment. A missing key emits no value
//! event; `Complete` fires with status 0, matching the convention used by
//! `lore_revision_metadata_get_async`.

use lore_base::error::InvalidArguments;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::LoreMetadataEventData;
use lore_revision::event::revision_tree::LoreRevisionTreeMetadataGetCompleteEventData;
use lore_revision::interface::LoreError;
use lore_revision::interface::LoreMetadata;
use lore_revision::interface::LoreString;
use lore_revision::metadata::Metadata;
use serde::Deserialize;
use serde::Serialize;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;

/// Arguments for `lore_revision_tree_metadata_get`.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, LoreArgs)]
#[handler(metadata_get_impl)]
pub struct LoreRevisionTreeMetadataGetArgs {
    /// Per-call correlation id echoed back in events
    pub id: u64,
    /// Loaded revision-tree handle to read from
    pub handle: LoreRevisionTree,
    /// Metadata key to read; pending edits take precedence over the revision
    pub key: LoreString,
}

#[error_set]
enum MetadataGetError {
    InvalidArguments,
}

impl EventError for MetadataGetError {
    fn translated(&self) -> LoreError {
        match self {
            MetadataGetError::InvalidArguments(_) => LoreError::InvalidArguments,
            MetadataGetError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

fn invalid(reason: &str) -> MetadataGetError {
    MetadataGetError::from(InvalidArguments {
        reason: reason.into(),
    })
}

/// Emit the id-carrying terminal for a failed `metadata_get`. The value slot
/// carries an empty string — `LoreMetadata` has no zero variant and the
/// caller must ignore the value whenever `error_code != NONE`.
fn emit_metadata_get_error(id: u64, error_code: LoreErrorCode) {
    LoreEvent::RevisionTreeMetadataGetComplete(LoreRevisionTreeMetadataGetCompleteEventData {
        id,
        key: LoreString::default(),
        value: LoreMetadata::String(LoreString::default()),
        error_code,
    })
    .send();
}

/// Build the per-call value event from a raw metadata entry. Returns `None`
/// when the entry cannot be represented (which `LoreMetadataEventData::new`
/// treats as a conversion failure).
fn value_event(
    id: u64,
    metadata: &Metadata,
    key: &str,
) -> Option<LoreRevisionTreeMetadataGetCompleteEventData> {
    let (value, value_type) = metadata.get_typed(key).ok()?;
    let entry = LoreMetadataEventData::new(key, value, value_type).ok()?;
    Some(LoreRevisionTreeMetadataGetCompleteEventData {
        id,
        key: entry.key,
        value: entry.value,
        error_code: LoreErrorCode::None,
    })
}

/// Read a metadata value by key from the handle.
///
/// On a hit the caller receives
/// `LORE_EVENT_REVISION_TREE_METADATA_GET_COMPLETE` carrying the key, the
/// typed value, and `error_code = NONE`, before `Complete {status: 0}`.
/// Pending `metadata_set` edits take precedence over the loaded revision's
/// frozen Metadata fragment. A missing key emits no value event and
/// completes with status 0, matching `lore_revision_metadata_get_async`.
pub async fn metadata_get(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeMetadataGetArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, metadata_get_impl).await
}

async fn metadata_get_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeMetadataGetArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let miss_id = args.id;
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        metadata_get,
        move |_| {
            emit_metadata_get_error(miss_id, LoreErrorCode::InvalidArguments);
        },
        async move |internal, args: LoreRevisionTreeMetadataGetArgs| {
            let id = args.id;

            let Ok(key) = std::str::from_utf8(args.key.as_bytes()) else {
                emit_metadata_get_error(id, LoreErrorCode::InvalidArguments);
                return Err(invalid("key is not valid UTF-8"));
            };
            if key.is_empty() {
                emit_metadata_get_error(id, LoreErrorCode::InvalidArguments);
                return Err(invalid("key is empty"));
            }

            // Pending edits first — the copy out of the read guard keeps the
            // lock scope free of awaits.
            let pending_event = {
                let pending = internal.pending_metadata.read();
                value_event(id, &pending, key)
            };
            if let Some(event) = pending_event {
                LoreEvent::RevisionTreeMetadataGetComplete(event).send();
                return Ok(());
            }

            // Fall back to the loaded revision's frozen Metadata fragment.
            let metadata_hash = internal.state().metadata_hash();
            if metadata_hash.is_zero() {
                return Ok(());
            }
            let metadata =
                match Metadata::deserialize(internal.repository_context.clone(), metadata_hash)
                    .await
                {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        emit_metadata_get_error(id, LoreErrorCode::Internal);
                        return Err(MetadataGetError::internal_with_context(
                            error,
                            "Metadata::deserialize",
                        ));
                    }
                };
            if let Some(event) = value_event(id, &metadata, key) {
                LoreEvent::RevisionTreeMetadataGetComplete(event).send();
            }
            Ok(())
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::interface::LoreMetadataType;

    use super::*;
    use crate::revision_tree::handle as rt_handle;
    use crate::revision_tree::load::LoreRevisionTreeLoadArgs;
    use crate::revision_tree::load::load;
    use crate::revision_tree::metadata_set::LoreRevisionTreeMetadataSetArgs;
    use crate::revision_tree::metadata_set::metadata_set;
    use crate::storage::handle as storage_handle;
    use crate::storage::store::in_memory_for_tests;

    #[derive(Clone)]
    enum CapturedEvent {
        Complete(i32),
        RevisionTreeLoaded(u64),
        MetadataGetComplete(Box<LoreRevisionTreeMetadataGetCompleteEventData>),
        Other,
    }

    impl CapturedEvent {
        fn from_event(event: &LoreEvent) -> Self {
            match event {
                LoreEvent::Complete(data) => Self::Complete(data.status),
                LoreEvent::RevisionTreeLoaded(data) => Self::RevisionTreeLoaded(data.handle_id),
                LoreEvent::RevisionTreeMetadataGetComplete(data) => {
                    Self::MetadataGetComplete(Box::new(data.clone()))
                }
                _ => Self::Other,
            }
        }
    }

    fn make_callback(sink: Arc<Mutex<Vec<CapturedEvent>>>) -> LoreEventCallback {
        Some(Box::new(move |event: &LoreEvent| {
            sink.lock().unwrap().push(CapturedEvent::from_event(event));
        }))
    }

    fn get_outcome(
        events: &[CapturedEvent],
        id: u64,
    ) -> Option<LoreRevisionTreeMetadataGetCompleteEventData> {
        events.iter().find_map(|event| match event {
            CapturedEvent::MetadataGetComplete(data) if data.id == id => Some((**data).clone()),
            _ => None,
        })
    }

    fn completes_with(events: &[CapturedEvent], status: i32) -> bool {
        events
            .iter()
            .any(|event| matches!(event, CapturedEvent::Complete(value) if *value == status))
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

    async fn run_get(handle: LoreRevisionTree, id: u64, key: &str) -> (i32, Vec<CapturedEvent>) {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = metadata_get(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataGetArgs {
                id,
                handle,
                key: LoreString::from_str(key),
            },
            make_callback(sink.clone()),
        )
        .await;
        let events = sink.lock().unwrap().clone();
        (status, events)
    }

    fn release(handle: LoreRevisionTree, store_handle_id: u64) {
        rt_handle::unregister(handle);
        storage_handle::unregister(crate::storage::handle::LoreStore {
            handle_id: store_handle_id,
        });
    }

    #[tokio::test]
    async fn metadata_get_returns_a_pending_value() {
        let (handle, store_handle_id) =
            load_handle("mg-pending", Partition::from([0x11u8; 16])).await;
        let set_status = metadata_set(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataSetArgs {
                id: 1,
                handle,
                key: LoreString::from_str("message"),
                value: LoreString::from_str("import nightly assets"),
                format: LoreMetadataType::String as u32,
            },
            None,
        )
        .await;
        assert_eq!(set_status, 0, "the set fixture must succeed");

        let (status, events) = run_get(handle, 2, "message").await;

        assert_eq!(status, 0, "reading a pending key must succeed");
        let data = get_outcome(&events, 2).expect("MetadataGetComplete must fire");
        assert_eq!(data.error_code, LoreErrorCode::None);
        assert_eq!(data.key.as_str(), "message");
        assert!(
            matches!(&data.value, LoreMetadata::String(value) if value.as_str() == "import nightly assets"),
            "the pending string value must round-trip"
        );
        assert!(completes_with(&events, 0));

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn metadata_get_missing_key_emits_no_value_and_completes_ok() {
        let (handle, store_handle_id) =
            load_handle("mg-missing", Partition::from([0x22u8; 16])).await;

        let (status, events) = run_get(handle, 3, "no-such-key").await;

        assert_eq!(status, 0, "a missing key must complete with status 0");
        assert!(
            get_outcome(&events, 3).is_none(),
            "a missing key must emit no value event"
        );
        assert!(completes_with(&events, 0));

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn metadata_get_numeric_value_round_trips_typed() {
        let (handle, store_handle_id) =
            load_handle("mg-numeric", Partition::from([0x33u8; 16])).await;
        let set_status = metadata_set(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataSetArgs {
                id: 4,
                handle,
                key: LoreString::from_str("count"),
                value: LoreString::from_str("42"),
                format: LoreMetadataType::Numeric as u32,
            },
            None,
        )
        .await;
        assert_eq!(set_status, 0, "the set fixture must succeed");

        let (status, events) = run_get(handle, 5, "count").await;

        assert_eq!(status, 0);
        let data = get_outcome(&events, 5).expect("MetadataGetComplete must fire");
        assert!(
            matches!(data.value, LoreMetadata::Numeric(42)),
            "the numeric value must round-trip typed"
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn metadata_get_rejects_an_empty_key() {
        let (handle, store_handle_id) =
            load_handle("mg-empty", Partition::from([0x44u8; 16])).await;

        let (status, events) = run_get(handle, 6, "").await;

        assert_eq!(status, 1, "an empty key must fail");
        let data = get_outcome(&events, 6).expect("the terminal must carry the caller id");
        assert_eq!(data.error_code, LoreErrorCode::InvalidArguments);
        assert!(completes_with(&events, 1));

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn metadata_get_on_unknown_handle_emits_terminal_with_invalid_arguments() {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = metadata_get(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataGetArgs {
                id: 7,
                handle: LoreRevisionTree::INVALID,
                key: LoreString::from_str("key"),
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "reading against an unknown handle must fail");
        let events = sink.lock().unwrap().clone();
        let data = get_outcome(&events, 7)
            .expect("a handle miss must still emit the terminal carrying the caller id");
        assert_eq!(data.error_code, LoreErrorCode::InvalidArguments);
        assert!(completes_with(&events, 1));
    }
}
