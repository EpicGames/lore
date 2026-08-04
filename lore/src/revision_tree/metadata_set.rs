// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_metadata_set` — record a `(key, value, format)`
//! triple on the in-progress revision's metadata. A subsequent set on the
//! same key overwrites the previous value in the same uncommitted handle
//! state. `format` is a `u32` matching the existing
//! `LoreRevisionMetadataSetArgs::formats` element type.

use lore_base::error::InvalidArguments;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeMetadataSetCompleteEventData;
use lore_revision::interface::LoreError;
use lore_revision::interface::LoreMetadataType;
use lore_revision::interface::LoreString;
use lore_revision::metadata::Metadata;
use lore_revision::metadata::MetadataType;
use serde::Deserialize;
use serde::Serialize;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;

/// Arguments for `lore_revision_tree_metadata_set`.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, LoreArgs)]
#[handler(metadata_set_impl)]
pub struct LoreRevisionTreeMetadataSetArgs {
    /// Per-call correlation id echoed back in events
    pub id: u64,
    /// Loaded revision-tree handle to mutate
    pub handle: LoreRevisionTree,
    /// Metadata key; re-setting it overwrites the pending value
    pub key: LoreString,
    /// Value stored under the key
    pub value: LoreString,
    /// Value encoding, matching `LoreRevisionMetadataSetArgs::formats`
    pub format: u32,
}

#[error_set]
enum MetadataSetError {
    InvalidArguments,
}

impl EventError for MetadataSetError {
    fn translated(&self) -> LoreError {
        match self {
            MetadataSetError::InvalidArguments(_) => LoreError::InvalidArguments,
            MetadataSetError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

fn invalid(reason: &str) -> MetadataSetError {
    MetadataSetError::from(InvalidArguments {
        reason: reason.into(),
    })
}

fn emit_metadata_set_complete(id: u64, error_code: LoreErrorCode) {
    LoreEvent::RevisionTreeMetadataSetComplete(LoreRevisionTreeMetadataSetCompleteEventData {
        id,
        error_code,
    })
    .send();
}

/// Record a `(key, value, format)` triple on the in-progress revision's
/// metadata.
///
/// On success the caller receives
/// `LORE_EVENT_REVISION_TREE_METADATA_SET_COMPLETE` with `error_code = NONE`,
/// before `Complete {status: 0}`. The value lives on the handle until
/// `commit` serializes it into the new revision's metadata fragment;
/// re-setting a key overwrites the pending value. Mirrors
/// `lore_revision_metadata_set` for the file-system-based API but writes
/// into the open handle instead of the staged revision.
pub async fn metadata_set(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeMetadataSetArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, metadata_set_impl).await
}

async fn metadata_set_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeMetadataSetArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let miss_id = args.id;
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        metadata_set,
        move |_| {
            emit_metadata_set_complete(miss_id, LoreErrorCode::InvalidArguments);
        },
        async move |internal, args: LoreRevisionTreeMetadataSetArgs| {
            let id = args.id;
            let fail = |reason: &str| {
                emit_metadata_set_complete(id, LoreErrorCode::InvalidArguments);
                Err(invalid(reason))
            };

            let Ok(key) = std::str::from_utf8(args.key.as_bytes()) else {
                return fail("key is not valid UTF-8");
            };
            if key.is_empty() {
                return fail("key is empty");
            }

            let format = if args.format == LoreMetadataType::Binary as u32 {
                MetadataType::Binary
            } else if args.format == LoreMetadataType::Numeric as u32 {
                MetadataType::Numeric
            } else if args.format == LoreMetadataType::String as u32 {
                MetadataType::String
            } else {
                return fail("format is not a valid metadata type");
            };

            let value = match Metadata::decode_to_value(args.value.as_str(), &format) {
                Ok(value) => value,
                Err(_error) => {
                    return fail("value does not decode under the requested format");
                }
            };

            let result = {
                let mut pending = internal.pending_metadata.write();
                match format {
                    MetadataType::Binary => pending.set_binary(key, &value),
                    MetadataType::Numeric => {
                        // `decode_to_value` produced the little-endian bytes;
                        // re-read them so the typed setter records the type.
                        match Metadata::to_u64(&value) {
                            Ok(number) => pending.set_u64(key, number),
                            Err(error) => Err(error),
                        }
                    }
                    _ => pending.set_string(key, args.value.as_str()),
                }
            };
            if let Err(error) = result {
                emit_metadata_set_complete(id, LoreErrorCode::Internal);
                return Err(MetadataSetError::internal_with_context(
                    error,
                    "Metadata::set",
                ));
            }

            emit_metadata_set_complete(id, LoreErrorCode::None);
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
        MetadataSetComplete(u64, LoreErrorCode),
        Other(u32),
    }

    impl CapturedEvent {
        fn from_event(event: &LoreEvent) -> Self {
            match event {
                LoreEvent::Error(data) => Self::Error(data.error_type),
                LoreEvent::Complete(data) => Self::Complete(data.status),
                LoreEvent::RevisionTreeLoaded(data) => Self::RevisionTreeLoaded(data.handle_id),
                LoreEvent::RevisionTreeMetadataSetComplete(data) => {
                    Self::MetadataSetComplete(data.id, data.error_code)
                }
                other => Self::Other(other.discriminant()),
            }
        }
    }

    fn make_callback(sink: Arc<Mutex<Vec<CapturedEvent>>>) -> LoreEventCallback {
        Some(Box::new(move |event: &LoreEvent| {
            sink.lock().unwrap().push(CapturedEvent::from_event(event));
        }))
    }

    fn set_outcome(events: &[CapturedEvent], id: u64) -> Option<LoreErrorCode> {
        events.iter().find_map(|event| match event {
            CapturedEvent::MetadataSetComplete(event_id, error_code) if *event_id == id => {
                Some(*error_code)
            }
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

    async fn run_set(
        handle: LoreRevisionTree,
        id: u64,
        key: &str,
        value: &str,
        format: u32,
    ) -> (i32, Vec<CapturedEvent>) {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = metadata_set(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataSetArgs {
                id,
                handle,
                key: LoreString::from_str(key),
                value: LoreString::from_str(value),
                format,
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
    async fn metadata_set_records_the_pending_value() {
        let (handle, store_handle_id) = load_handle("ms-set", Partition::from([0x11u8; 16])).await;

        let (status, events) = run_set(
            handle,
            1,
            "message",
            "import nightly assets",
            LoreMetadataType::String as u32,
        )
        .await;

        assert_eq!(
            status, 0,
            "setting a string value must succeed, got {events:?}"
        );
        assert_eq!(set_outcome(&events, 1), Some(LoreErrorCode::None));
        assert!(events.contains(&CapturedEvent::Complete(0)));

        let entry = rt_handle::REGISTRY
            .get(&handle.handle_id)
            .expect("handle registered");
        let pending = entry.pending_metadata.read();
        assert_eq!(
            pending.get_string("message").expect("key must be present"),
            "import nightly assets"
        );
        drop(pending);
        drop(entry);

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn metadata_set_overwrites_the_previous_pending_value() {
        let (handle, store_handle_id) =
            load_handle("ms-overwrite", Partition::from([0x22u8; 16])).await;

        let (status, _) = run_set(handle, 2, "count", "7", LoreMetadataType::Numeric as u32).await;
        assert_eq!(status, 0);
        let (status, _) = run_set(handle, 3, "count", "9", LoreMetadataType::Numeric as u32).await;
        assert_eq!(status, 0);

        let entry = rt_handle::REGISTRY
            .get(&handle.handle_id)
            .expect("handle registered");
        let pending = entry.pending_metadata.read();
        assert_eq!(pending.get_u64("count").expect("key must be present"), 9);
        drop(pending);
        drop(entry);

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn metadata_set_rejects_bad_key_format_and_value() {
        let (handle, store_handle_id) =
            load_handle("ms-invalid", Partition::from([0x33u8; 16])).await;

        let (status, events) = run_set(handle, 4, "", "v", LoreMetadataType::String as u32).await;
        assert_eq!(status, 1, "an empty key must fail, got {events:?}");
        assert_eq!(
            set_outcome(&events, 4),
            Some(LoreErrorCode::InvalidArguments)
        );

        let (status, events) = run_set(handle, 5, "key", "v", 42).await;
        assert_eq!(status, 1, "an unknown format must fail, got {events:?}");

        let (status, events) = run_set(
            handle,
            6,
            "key",
            "not-a-number",
            LoreMetadataType::Numeric as u32,
        )
        .await;
        assert_eq!(status, 1, "a non-numeric value must fail, got {events:?}");

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn metadata_set_on_unknown_handle_emits_terminal_with_invalid_arguments() {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = metadata_set(
            LoreGlobalArgs::default(),
            LoreRevisionTreeMetadataSetArgs {
                id: 7,
                handle: LoreRevisionTree::INVALID,
                key: LoreString::from_str("key"),
                value: LoreString::from_str("value"),
                format: LoreMetadataType::String as u32,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "setting against an unknown handle must fail");
        let events = sink.lock().unwrap().clone();
        assert_eq!(
            set_outcome(&events, 7),
            Some(LoreErrorCode::InvalidArguments),
            "a handle miss must still emit the terminal carrying the caller id, got {events:?}"
        );
        assert!(events.contains(&CapturedEvent::Complete(1)));
    }
}
