// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_info` — fetch the loaded revision's record-level
//! metadata: parent revision signatures plus the creation timestamp, author
//! identity, and metadata key count from the revision's Metadata fragment.
//! Revision-scoped; it takes no node id.

use lore_base::error::InvalidArguments;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeInfoEventData;
use lore_revision::interface::LoreError;
use lore_revision::interface::LoreString;
use lore_revision::metadata::CREATED_BY;
use lore_revision::metadata::Metadata;
use serde::Deserialize;
use serde::Serialize;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;

/// Arguments for `lore_revision_tree_info`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Deserialize, Serialize, LoreArgs)]
#[handler(info_impl)]
pub struct LoreRevisionTreeInfoArgs {
    /// Per-call correlation id echoed back in events
    pub id: u64,
    /// Loaded revision-tree handle whose revision metadata is fetched
    pub handle: LoreRevisionTree,
}

#[error_set]
enum InfoError {
    InvalidArguments,
}

impl EventError for InfoError {
    fn translated(&self) -> LoreError {
        match self {
            InfoError::InvalidArguments(_) => LoreError::InvalidArguments,
            InfoError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Emit the id-carrying terminal for a failed `info`: zeroed fields plus the
/// populated `error_code`.
fn emit_info_error(id: u64, error_code: LoreErrorCode) {
    LoreEvent::RevisionTreeInfo(LoreRevisionTreeInfoEventData {
        id,
        error_code,
        ..Default::default()
    })
    .send();
}

/// Fetch the loaded revision's record-level metadata.
///
/// On success the caller receives `LORE_EVENT_REVISION_TREE_INFO` carrying the
/// `(repository, revision)` the handle represents, the parent revision
/// signatures, and — from the revision's Metadata fragment — the creation
/// timestamp, author identity, and metadata key count, with
/// `error_code = NONE`, before `Complete {status: 0}`. A revision with no
/// Metadata fragment reports zeroed metadata fields (not an error); a
/// present-but-unreadable fragment completes with `error_code = INTERNAL`. The
/// verb materializes no bytes to disk.
pub async fn info(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeInfoArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, info_impl).await
}

async fn info_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeInfoArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let miss_id = args.id;
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        info,
        move || {
            emit_info_error(miss_id, LoreErrorCode::InvalidArguments);
        },
        async move |internal, args: LoreRevisionTreeInfoArgs| {
            let id = args.id;

            let metadata_hash = internal.state.metadata_hash();
            let metadata = if metadata_hash.is_zero() {
                Metadata::default()
            } else {
                match Metadata::deserialize(internal.repository_context.clone(), metadata_hash)
                    .await
                {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        emit_info_error(id, LoreErrorCode::Internal);
                        return Err(InfoError::internal_with_context(
                            error,
                            "Metadata::deserialize",
                        ));
                    }
                }
            };

            let creation_timestamp = metadata.get_timestamp().unwrap_or_default() as i64;
            let author_identity = metadata
                .get_string(CREATED_BY)
                .map(LoreString::from)
                .unwrap_or_default();
            let mut metadata_key_count = 0u32;
            let _ = metadata.walk(|_, _, _| metadata_key_count += 1);

            LoreEvent::RevisionTreeInfo(LoreRevisionTreeInfoEventData {
                id,
                repository: internal.repository,
                revision: internal.state.revision(),
                parent: internal.state.parents(),
                creation_timestamp,
                author_identity,
                metadata_key_count,
                error_code: LoreErrorCode::None,
            })
            .send();
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
    use lore_revision::metadata::MESSAGE;
    use lore_revision::repository::RepositoryContext;
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
        Info(Box<LoreRevisionTreeInfoEventData>),
        Other(u32),
    }

    impl CapturedEvent {
        fn from_event(event: &LoreEvent) -> Self {
            match event {
                LoreEvent::Error(data) => Self::Error(data.error_type),
                LoreEvent::Complete(data) => Self::Complete(data.status),
                LoreEvent::RevisionTreeLoaded(data) => Self::RevisionTreeLoaded(data.handle_id),
                LoreEvent::RevisionTreeInfo(data) => Self::Info(Box::new(data.clone())),
                other => Self::Other(other.discriminant()),
            }
        }
    }

    fn make_callback(sink: Arc<Mutex<Vec<CapturedEvent>>>) -> LoreEventCallback {
        Some(Box::new(move |event: &LoreEvent| {
            sink.lock().unwrap().push(CapturedEvent::from_event(event));
        }))
    }

    fn info_event(events: &[CapturedEvent]) -> Option<LoreRevisionTreeInfoEventData> {
        events.iter().find_map(|event| match event {
            CapturedEvent::Info(data) => Some((**data).clone()),
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

    fn handle_state(handle: LoreRevisionTree) -> (Arc<State>, Arc<RepositoryContext>) {
        let entry = rt_handle::REGISTRY
            .get(&handle.handle_id)
            .expect("handle registered");
        (entry.state.clone(), entry.repository_context.clone())
    }

    fn release(handle: LoreRevisionTree, store_handle_id: u64) {
        rt_handle::unregister(handle);
        storage_handle::unregister(crate::storage::handle::LoreStore {
            handle_id: store_handle_id,
        });
    }

    #[tokio::test]
    async fn info_returns_parents_timestamp_author_and_key_count() {
        let partition = Partition::from([0x22u8; 16]);
        let (handle, store_handle_id) = load_handle("info-full", partition).await;
        let (state, repository_context) = handle_state(handle);

        let mut metadata = Metadata::new();
        metadata
            .set_timestamp(1_700_000_000)
            .expect("set timestamp");
        metadata
            .set_string(CREATED_BY, "alice")
            .expect("set author");
        metadata
            .set_string(MESSAGE, "initial")
            .expect("set message");
        let metadata_hash = metadata
            .serialize(repository_context.clone())
            .await
            .expect("serialize metadata");
        state.set_metadata_hash(metadata_hash);
        let parent_self = Hash::from([0x01u8; 32]);
        let parent_other = Hash::from([0x02u8; 32]);
        state.set_parent_self(parent_self);
        state.set_parent_other(parent_other);

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = info(
            LoreGlobalArgs::default(),
            LoreRevisionTreeInfoArgs { id: 1, handle },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 0);
        let events = sink.lock().unwrap().clone();
        let data = info_event(&events).expect("info event must fire");
        assert_eq!(data.id, 1);
        assert_eq!(data.error_code, LoreErrorCode::None);
        assert_eq!(data.repository, partition, "got {events:?}");
        assert_eq!(
            data.revision,
            Hash::default(),
            "info reports the handle's loaded revision, got {events:?}"
        );
        assert_eq!(
            data.parent,
            [parent_self, parent_other],
            "info must carry the parent revision signatures, got {events:?}"
        );
        assert_eq!(data.creation_timestamp, 1_700_000_000, "got {events:?}");
        assert_eq!(data.author_identity.as_str(), "alice", "got {events:?}");
        assert_eq!(
            data.metadata_key_count, 3,
            "timestamp + created-by + message = 3 keys, got {events:?}"
        );
        assert!(events.contains(&CapturedEvent::Complete(0)));

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn info_without_metadata_reports_zeroed_fields() {
        let partition = Partition::from([0x33u8; 16]);
        let (handle, store_handle_id) = load_handle("info-empty", partition).await;

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = info(
            LoreGlobalArgs::default(),
            LoreRevisionTreeInfoArgs { id: 2, handle },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 0);
        let events = sink.lock().unwrap().clone();
        let data = info_event(&events).expect("info event must fire");
        assert_eq!(data.error_code, LoreErrorCode::None);
        assert_eq!(data.repository, partition, "got {events:?}");
        assert_eq!(data.metadata_key_count, 0, "got {events:?}");
        assert_eq!(data.creation_timestamp, 0, "got {events:?}");
        assert_eq!(data.author_identity.as_str(), "", "got {events:?}");
        assert_eq!(
            data.parent,
            [Hash::default(), Hash::default()],
            "an uncommitted handle has no parent signatures, got {events:?}"
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn info_with_unreadable_metadata_returns_internal() {
        let (handle, store_handle_id) =
            load_handle("info-corrupt", Partition::from([0x44u8; 16])).await;
        let (state, _repository_context) = handle_state(handle);
        // Point the revision at a metadata fragment that was never written to the store.
        state.set_metadata_hash(Hash::from([0x7Eu8; 32]));

        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let status = info(
            LoreGlobalArgs::default(),
            LoreRevisionTreeInfoArgs { id: 3, handle },
            make_callback(sink.clone()),
        )
        .await;

        assert_ne!(status, 0, "an unreadable metadata fragment must fail");
        let events = sink.lock().unwrap().clone();
        let data = info_event(&events)
            .expect("a failure must still emit the info terminal carrying the id");
        assert_eq!(data.id, 3);
        assert_eq!(
            data.error_code,
            LoreErrorCode::Internal,
            "a present-but-unreadable metadata fragment must report Internal, got {events:?}"
        );
        assert!(
            events.contains(&CapturedEvent::Complete(status)),
            "Complete must carry the failure status, got {events:?}"
        );

        release(handle, store_handle_id);
    }

    #[tokio::test]
    async fn info_on_unknown_handle_emits_terminal_with_invalid_arguments() {
        let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));

        let status = info(
            LoreGlobalArgs::default(),
            LoreRevisionTreeInfoArgs {
                id: 4,
                handle: LoreRevisionTree::INVALID,
            },
            make_callback(sink.clone()),
        )
        .await;

        assert_eq!(status, 1, "an unknown handle must fail");
        let events = sink.lock().unwrap().clone();
        let data = info_event(&events)
            .expect("a handle miss must still emit the info terminal carrying the id");
        assert_eq!(data.id, 4);
        assert_eq!(
            data.error_code,
            LoreErrorCode::InvalidArguments,
            "got {events:?}"
        );
        assert!(events.contains(&CapturedEvent::Complete(1)));
    }
}
