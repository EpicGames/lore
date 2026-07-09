// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Integration tests for the content-addressed storage API.
//!
//! Covers open, close, put, and get ops. the no-re-export
//! contract is pinned by the imports in the `imports` module.

#[cfg(test)]
#[allow(unused_imports)]
mod imports {
    use lore::storage::handle::LoreStore;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Fragment;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::event::LoreBytes;
    use lore_revision::event::LoreErrorCode;
    use lore_storage::StoreError;
    use lore_storage::store_types::StoreMatch;
}

#[cfg(test)]
mod open_tests {
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Mutex;

    use lore::repository;
    use lore::storage::open;
    use lore::storage::open::LoreStorageOpenArgs;
    use lore_base::lore_spawn;
    use lore_revision::event::LoreEvent;
    use lore_revision::interface::LoreEventCallback;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreString;

    /// Capture the events emitted by a call; mutex-guarded `Vec` so the
    /// callback (which must be `Fn + Send + Sync`) can push into it.
    #[derive(Debug, Clone, PartialEq)]
    enum Captured {
        Opened { handle_id: u64 },
        Error,
        Complete(i32),
        Other,
    }

    fn make_sink() -> (Arc<Mutex<Vec<Captured>>>, LoreEventCallback) {
        let sink: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_for_cb = sink.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            let rec = match event {
                LoreEvent::StorageOpened(data) => Captured::Opened {
                    handle_id: data.handle_id,
                },
                LoreEvent::Error(_) => Captured::Error,
                LoreEvent::Complete(data) => Captured::Complete(data.status),
                _ => Captured::Other,
            };
            sink_for_cb.lock().unwrap().push(rec);
        }));
        (sink, callback)
    }

    fn globals() -> LoreGlobalArgs {
        LoreGlobalArgs::default()
    }

    fn take_opened(events: &[Captured]) -> Option<u64> {
        events.iter().find_map(|e| match e {
            Captured::Opened { handle_id } => Some(*handle_id),
            _ => None,
        })
    }

    fn assert_opened_before_complete(events: &[Captured], status: i32) {
        let opened_ix = events
            .iter()
            .position(|e| matches!(e, Captured::Opened { .. }));
        let complete_ix = events
            .iter()
            .position(|e| matches!(e, Captured::Complete(_)));
        let (Some(opened_ix), Some(complete_ix)) = (opened_ix, complete_ix) else {
            panic!("expected both Opened and Complete events, got {events:?}");
        };
        assert!(
            opened_ix < complete_ix,
            "Opened must precede Complete, got {events:?}",
        );
        assert_eq!(events[complete_ix], Captured::Complete(status));
    }

    /// Create a `tempfile::TempDir` that auto-cleans on Drop. The `tag` becomes part of the
    /// directory's filename prefix so call sites retain a contextual hint visible in the
    /// working directory.
    fn tempdir(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("lore-storage-open-{tag}-"))
            .tempdir()
            .expect("create tempdir")
    }

    async fn create_repo(path: &Path) {
        let mut repo_globals = globals();
        repo_globals.repository_path = path.into();
        repo_globals.offline = 1;
        let result = repository::create(
            repo_globals,
            repository::LoreRepositoryCreateArgs {
                repository_url: "lore://localhost/test-storage-open".into(),
                description: LoreString::default(),
                id: LoreString::default(),
                use_shared_store: 0,
                shared_store_path: LoreString::default(),
            },
            None,
        )
        .await;
        assert_eq!(result, 0, "repository create failed for {path:?}");
    }

    #[tokio::test]
    async fn in_memory_open_emits_opened_event_with_nonzero_handle() {
        let (sink, callback) = make_sink();
        let status = open::open(
            globals(),
            LoreStorageOpenArgs {
                repository_path: LoreString::default(),
                in_memory: 1,
                ..Default::default()
            },
            callback,
        )
        .await;
        assert_eq!(status, 0);
        let events = sink.lock().unwrap().clone();
        let handle_id = take_opened(&events);
        assert!(
            matches!(handle_id, Some(id) if id != 0),
            "expected Opened with non-zero handle, got {events:?}",
        );
        assert_opened_before_complete(&events, 0);
    }

    #[tokio::test]
    async fn path_and_in_memory_together_errors_invalid_args() {
        // Path provided AND in_memory=1 — path and in_memory both set is rejected.
        let (sink, callback) = make_sink();
        let status = open::open(
            globals(),
            LoreStorageOpenArgs {
                repository_path: LoreString::from("/tmp/whatever"),
                in_memory: 1,
                ..Default::default()
            },
            callback,
        )
        .await;
        assert_ne!(status, 0);
        let events = sink.lock().unwrap().clone();
        assert!(
            !events.contains(&Captured::Error),
            "no mid-stream Error event on terminal failure, got {events:?}",
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Captured::Complete(s) if *s != 0)),
            "expected Complete(1), got {events:?}",
        );
        assert!(
            take_opened(&events).is_none(),
            "must not emit Opened on failure, got {events:?}",
        );
    }

    #[tokio::test]
    async fn empty_path_without_in_memory_errors_invalid_args() {
        // Empty path AND in_memory=0 — empty path with in_memory=0 is rejected.
        let (sink, callback) = make_sink();
        let status = open::open(
            globals(),
            LoreStorageOpenArgs {
                repository_path: LoreString::default(),
                in_memory: 0,
                ..Default::default()
            },
            callback,
        )
        .await;
        assert_ne!(status, 0);
        let events = sink.lock().unwrap().clone();
        assert!(
            !events.contains(&Captured::Error),
            "no mid-stream Error event on terminal failure, got {events:?}",
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Captured::Complete(s) if *s != 0))
        );
    }

    #[tokio::test]
    async fn two_in_memory_opens_produce_distinct_handles() {
        // Surface check: handle ids differ. The stronger isolation claim
        // (writes through one handle are invisible through another) is
        // checked in `two_in_memory_opens_isolate_writes`.
        let (sink_a, cb_a) = make_sink();
        let (sink_b, cb_b) = make_sink();
        let status_a = open::open(
            globals(),
            LoreStorageOpenArgs {
                repository_path: LoreString::default(),
                in_memory: 1,
                ..Default::default()
            },
            cb_a,
        )
        .await;
        let status_b = open::open(
            globals(),
            LoreStorageOpenArgs {
                repository_path: LoreString::default(),
                in_memory: 1,
                ..Default::default()
            },
            cb_b,
        )
        .await;
        assert_eq!(status_a, 0);
        assert_eq!(status_b, 0);
        let id_a = take_opened(&sink_a.lock().unwrap()).expect("first open should emit Opened");
        let id_b = take_opened(&sink_b.lock().unwrap()).expect("second open should emit Opened");
        assert_ne!(id_a, id_b);
    }

    #[tokio::test]
    async fn disk_backed_open_on_real_repo_emits_opened() {
        // Open an actual repo path: open an actual repo path and verify the
        // OPENED event precedes Complete(0).
        let repo_dir = tempdir("ok");
        let repo_path = repo_dir.path();
        create_repo(repo_path).await;

        let (sink, callback) = make_sink();
        let status = open::open(
            globals(),
            LoreStorageOpenArgs {
                repository_path: LoreString::from(repo_path.display().to_string().as_str()),
                in_memory: 0,
                ..Default::default()
            },
            callback,
        )
        .await;
        assert_eq!(status, 0);
        let events = sink.lock().unwrap().clone();
        assert!(
            matches!(take_opened(&events), Some(id) if id != 0),
            "expected Opened with non-zero handle, got {events:?}",
        );
        assert_opened_before_complete(&events, 0);
    }

    async fn open_in_memory(callback: LoreEventCallback) -> i32 {
        open::open(
            globals(),
            LoreStorageOpenArgs {
                repository_path: LoreString::default(),
                in_memory: 1,
                ..Default::default()
            },
            callback,
        )
        .await
    }

    async fn close_handle(
        handle: lore::storage::handle::LoreStore,
        callback: LoreEventCallback,
    ) -> i32 {
        lore::storage::close::close(
            globals(),
            lore::storage::close::LoreStorageCloseArgs { handle },
            callback,
        )
        .await
    }

    #[tokio::test]
    async fn close_after_open_returns_status_zero() {
        let (open_sink, open_cb) = make_sink();
        let status = open_in_memory(open_cb).await;
        assert_eq!(status, 0);
        let id = take_opened(&open_sink.lock().unwrap()).expect("open should have emitted Opened");
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let (close_sink, close_cb) = make_sink();
        let status = close_handle(handle, close_cb).await;
        assert_eq!(status, 0);
        let events = close_sink.lock().unwrap().clone();
        assert!(
            events.contains(&Captured::Complete(0)),
            "expected Complete(0) on close, got {events:?}",
        );
    }

    #[tokio::test]
    async fn double_close_returns_invalid_arguments() {
        // Second close: a second close on an already-closed handle must
        // return InvalidArguments.
        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let (_, cb1) = make_sink();
        assert_eq!(close_handle(handle, cb1).await, 0);

        let (sink2, cb2) = make_sink();
        let status = close_handle(handle, cb2).await;
        assert_ne!(status, 0, "second close should fail");
        let events = sink2.lock().unwrap().clone();
        assert!(
            !events.contains(&Captured::Error),
            "no mid-stream Error event on terminal failure, got {events:?}",
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Captured::Complete(s) if *s != 0))
        );
    }

    #[tokio::test]
    async fn close_on_invalid_handle_returns_invalid_arguments() {
        // On unknown handle: close on an unknown handle (never registered) errors.
        let (sink, cb) = make_sink();
        let status = close_handle(lore::storage::handle::LoreStore::INVALID, cb).await;
        assert_ne!(status, 0);
        let events = sink.lock().unwrap().clone();
        assert!(
            !events.contains(&Captured::Error),
            "no mid-stream Error event on terminal failure, got {events:?}",
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Captured::Complete(s) if *s != 0))
        );
    }

    #[tokio::test]
    async fn put_roundtrip_address_matches_write_content() {
        // Put of a small buffer: put of a small buffer yields an address identical
        // to what `write_content` would produce for the same inputs.
        // We don't have `get` yet, so "identical to write_content" is
        // verified by constructing the expected address from the hash
        // of the input bytes + the caller's context.
        use lore_base::types::Context;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).expect("open should have emitted Opened");
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let payload = b"hello, storage put".to_vec();
        let partition = Partition::from([0x11u8; 16]);
        let context = Context::from([0x22u8; 16]);

        let data = lore_revision::event::LoreBytes {
            ptr: payload.as_ptr().cast(),
            len: payload.len(),
        };
        let item = lore::storage::put::LoreStoragePutItem {
            id: 42,
            partition,
            context,
            data,
            remote_write: 0,
            local_cache: 0,
            fixed_size_chunk: 0,
        };

        let sink: Arc<Mutex<Vec<LoreEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_for_cb = sink.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            sink_for_cb.lock().unwrap().push(event.clone());
        }));
        let status = lore::storage::put::put(
            globals(),
            lore::storage::put::LoreStoragePutArgs {
                handle,
                items: lore_revision::interface::LoreArray::from_vec(vec![item]),
            },
            callback,
        )
        .await;
        // Keep `payload` alive until after the call returns — the
        // storage API snapshots args at entry but references into the
        // payload buffer must outlive `Complete`.
        drop(payload);
        assert_eq!(status, 0);

        let events = sink.lock().unwrap().clone();
        let complete = events.iter().find_map(|e| match e {
            LoreEvent::StoragePutItemComplete(data) => Some(*data),
            _ => None,
        });
        let complete = complete.expect("expected PUT_ITEM_COMPLETE event");
        assert_eq!(complete.id, 42);
        assert_eq!(
            complete.error_code,
            lore_revision::event::LoreErrorCode::None,
        );
        // Address hash matches the content hash; context is preserved.
        let expected_hash = lore_storage::hash::hash_slice(b"hello, storage put");
        assert_eq!(complete.address.hash, expected_hash);
        assert_eq!(complete.address.context, context);
    }

    #[tokio::test]
    async fn put_empty_data_short_circuits_to_zero_hash() {
        // Empty data: data.len == 0 → address = (Hash::default(), context).
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let partition = Partition::from([0x33u8; 16]);
        let context = Context::from([0x44u8; 16]);
        let item = lore::storage::put::LoreStoragePutItem {
            id: 7,
            partition,
            context,
            data: lore_revision::event::LoreBytes {
                ptr: std::ptr::null(),
                len: 0,
            },
            remote_write: 0,
            local_cache: 0,
            fixed_size_chunk: 0,
        };

        let sink: Arc<Mutex<Vec<LoreEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_for_cb = sink.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            sink_for_cb.lock().unwrap().push(event.clone());
        }));
        let status = lore::storage::put::put(
            globals(),
            lore::storage::put::LoreStoragePutArgs {
                handle,
                items: lore_revision::interface::LoreArray::from_vec(vec![item]),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0);

        let events = sink.lock().unwrap().clone();
        let complete = events
            .iter()
            .find_map(|e| match e {
                LoreEvent::StoragePutItemComplete(data) => Some(*data),
                _ => None,
            })
            .expect("expected PUT_ITEM_COMPLETE");
        assert_eq!(complete.id, 7);
        assert_eq!(
            complete.error_code,
            lore_revision::event::LoreErrorCode::None
        );
        assert_eq!(complete.address.hash, Hash::default());
        assert_eq!(complete.address.context, context);
    }

    #[tokio::test]
    async fn put_null_data_with_nonzero_len_rejects_item() {
        // Null pointer with non-zero length: data.ptr == null but data.len > 0 → the item errors
        // with InvalidArguments; other items in the same call run
        // independently.
        use lore_base::types::Context;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let partition = Partition::from([0x55u8; 16]);
        let context = Context::from([0x66u8; 16]);
        let good_payload = b"second item".to_vec();
        let items = vec![
            lore::storage::put::LoreStoragePutItem {
                id: 1,
                partition,
                context,
                data: lore_revision::event::LoreBytes {
                    ptr: std::ptr::null(),
                    len: 42, // non-zero length with null ptr — null ptr with non-zero len
                },
                remote_write: 0,
                local_cache: 0,
                fixed_size_chunk: 0,
            },
            lore::storage::put::LoreStoragePutItem {
                id: 2,
                partition,
                context,
                data: lore_revision::event::LoreBytes {
                    ptr: good_payload.as_ptr().cast(),
                    len: good_payload.len(),
                },
                remote_write: 0,
                local_cache: 0,
                fixed_size_chunk: 0,
            },
        ];

        let sink: Arc<Mutex<Vec<LoreEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_for_cb = sink.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            sink_for_cb.lock().unwrap().push(event.clone());
        }));
        let status = lore::storage::put::put(
            globals(),
            lore::storage::put::LoreStoragePutArgs {
                handle,
                items: lore_revision::interface::LoreArray::from_vec(items),
            },
            callback,
        )
        .await;
        drop(good_payload);
        // One item failed → call-level status is 1 .
        assert_ne!(status, 0);

        let events = sink.lock().unwrap().clone();
        let completes: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                LoreEvent::StoragePutItemComplete(data) => Some(*data),
                _ => None,
            })
            .collect();
        assert_eq!(completes.len(), 2, "both items should emit complete");
        let item_1 = completes.iter().find(|c| c.id == 1).unwrap();
        let item_2 = completes.iter().find(|c| c.id == 2).unwrap();
        assert_eq!(
            item_1.error_code,
            lore_revision::event::LoreErrorCode::InvalidArguments,
        );
        assert_eq!(item_2.error_code, lore_revision::event::LoreErrorCode::None,);
    }

    #[tokio::test]
    async fn put_empty_items_array_completes_with_status_zero() {
        // Empty items array: items_len=0 → Complete(0) and no per-item events.
        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let sink: Arc<Mutex<Vec<LoreEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_for_cb = sink.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            sink_for_cb.lock().unwrap().push(event.clone());
        }));
        let status = lore::storage::put::put(
            globals(),
            lore::storage::put::LoreStoragePutArgs {
                handle,
                items: lore_revision::interface::LoreArray::default(),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0);
        let events = sink.lock().unwrap().clone();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, LoreEvent::StoragePutItemComplete(_))),
            "no PUT_ITEM_COMPLETE events expected on empty input",
        );
    }

    #[tokio::test]
    async fn put_zero_partition_item_rejects_invalid_args() {
        // Zero partition rejected: the all-zero partition is reserved as the null-context
        // sentinel and yields InvalidArguments at the item level.
        use lore_base::types::Context;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let payload = b"some bytes".to_vec();
        let item = lore::storage::put::LoreStoragePutItem {
            id: 99,
            partition: Partition::default(),
            context: Context::from([0x77u8; 16]),
            data: lore_revision::event::LoreBytes {
                ptr: payload.as_ptr().cast(),
                len: payload.len(),
            },
            remote_write: 0,
            local_cache: 0,
            fixed_size_chunk: 0,
        };

        let sink: Arc<Mutex<Vec<LoreEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_for_cb = sink.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            sink_for_cb.lock().unwrap().push(event.clone());
        }));
        let status = lore::storage::put::put(
            globals(),
            lore::storage::put::LoreStoragePutArgs {
                handle,
                items: lore_revision::interface::LoreArray::from_vec(vec![item]),
            },
            callback,
        )
        .await;
        drop(payload);
        assert_ne!(status, 0);

        let events = sink.lock().unwrap().clone();
        let complete = events
            .iter()
            .find_map(|e| match e {
                LoreEvent::StoragePutItemComplete(data) => Some(*data),
                _ => None,
            })
            .expect("expected PUT_ITEM_COMPLETE");
        assert_eq!(complete.id, 99);
        assert_eq!(
            complete.error_code,
            lore_revision::event::LoreErrorCode::InvalidArguments,
        );
    }

    #[tokio::test]
    async fn put_every_item_failing_still_emits_per_item_events() {
        // All-items-fail variant of All items failing case: with every input invalid,
        // the call returns status=1 but each item still gets its own
        // PUT_ITEM_COMPLETE event with its own error code.
        use lore_base::types::Context;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let partition = Partition::from([0x88u8; 16]);
        let context = Context::from([0x99u8; 16]);
        let items = vec![
            lore::storage::put::LoreStoragePutItem {
                id: 10,
                partition,
                context,
                data: lore_revision::event::LoreBytes {
                    ptr: std::ptr::null(),
                    len: 16, // null ptr with non-zero len
                },
                remote_write: 0,
                local_cache: 0,
                fixed_size_chunk: 0,
            },
            lore::storage::put::LoreStoragePutItem {
                id: 11,
                partition,
                context,
                data: lore_revision::event::LoreBytes {
                    ptr: std::ptr::null(),
                    len: 32,
                },
                remote_write: 0,
                local_cache: 0,
                fixed_size_chunk: 0,
            },
        ];

        let sink: Arc<Mutex<Vec<LoreEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_for_cb = sink.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            sink_for_cb.lock().unwrap().push(event.clone());
        }));
        let status = lore::storage::put::put(
            globals(),
            lore::storage::put::LoreStoragePutArgs {
                handle,
                items: lore_revision::interface::LoreArray::from_vec(items),
            },
            callback,
        )
        .await;
        assert_ne!(status, 0);

        let events = sink.lock().unwrap().clone();
        let completes: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                LoreEvent::StoragePutItemComplete(data) => Some(*data),
                _ => None,
            })
            .collect();
        assert_eq!(completes.len(), 2, "every item must emit PUT_ITEM_COMPLETE");
        for c in &completes {
            assert_eq!(
                c.error_code,
                lore_revision::event::LoreErrorCode::InvalidArguments,
            );
        }
    }

    #[tokio::test]
    async fn disk_backed_open_on_nonexistent_path_errors() {
        // Non-existent path: non-existent / invalid path errors with Error +
        // Complete(status=1) and no OPENED.
        let (_guard, missing) = temp_file_path("open-missing");
        let (sink, callback) = make_sink();
        let status = open::open(
            globals(),
            LoreStorageOpenArgs {
                repository_path: LoreString::from(missing.display().to_string().as_str()),
                in_memory: 0,
                ..Default::default()
            },
            callback,
        )
        .await;
        assert_ne!(status, 0);
        let events = sink.lock().unwrap().clone();
        assert!(
            !events.contains(&Captured::Error),
            "no mid-stream Error event on terminal failure, got {events:?}",
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Captured::Complete(s) if *s != 0))
        );
        assert!(
            take_opened(&events).is_none(),
            "must not emit Opened on failure, got {events:?}",
        );
    }

    /// Get-test capture: converts `StorageGetData` into an owned `Vec<u8>`
    /// snapshot inside the callback so the buffer contents outlive the
    /// `LoreBytes` view (which is only valid for the callback invocation).
    #[derive(Debug, Clone, PartialEq)]
    enum GetCaptured {
        Header {
            id: u64,
            address: lore_base::types::Address,
            size_content: u64,
        },
        Data {
            id: u64,
            address: lore_base::types::Address,
            offset: u64,
            bytes: Vec<u8>,
        },
        ItemComplete {
            id: u64,
            address: lore_base::types::Address,
            error_code: lore_revision::event::LoreErrorCode,
        },
        Error,
        Complete(i32),
        Other,
    }

    fn make_get_sink() -> (Arc<Mutex<Vec<GetCaptured>>>, LoreEventCallback) {
        let sink: Arc<Mutex<Vec<GetCaptured>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_for_cb = sink.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            let rec = match event {
                LoreEvent::StorageGetHeader(d) => GetCaptured::Header {
                    id: d.id,
                    address: d.address,
                    size_content: d.size_content,
                },
                LoreEvent::StorageGetData(d) => {
                    // Bytes lifetime: LoreBytes is valid for the callback
                    // duration — copy out before the buffer is released.
                    let slice = if d.bytes.len == 0 {
                        Vec::new()
                    } else {
                        unsafe { std::slice::from_raw_parts(d.bytes.ptr.cast::<u8>(), d.bytes.len) }
                            .to_vec()
                    };
                    GetCaptured::Data {
                        id: d.id,
                        address: d.address,
                        offset: d.offset,
                        bytes: slice,
                    }
                }
                LoreEvent::StorageGetItemComplete(d) => GetCaptured::ItemComplete {
                    id: d.id,
                    address: d.address,
                    error_code: d.error_code,
                },
                LoreEvent::Error(_) => GetCaptured::Error,
                LoreEvent::Complete(d) => GetCaptured::Complete(d.status),
                _ => GetCaptured::Other,
            };
            sink_for_cb.lock().unwrap().push(rec);
        }));
        (sink, callback)
    }

    async fn put_once(
        handle: lore::storage::handle::LoreStore,
        partition: lore_base::types::Partition,
        context: lore_base::types::Context,
        payload: &[u8],
    ) -> lore_base::types::Address {
        let data = lore_revision::event::LoreBytes {
            ptr: payload.as_ptr().cast(),
            len: payload.len(),
        };
        let item = lore::storage::put::LoreStoragePutItem {
            id: 1,
            partition,
            context,
            data,
            remote_write: 0,
            local_cache: 0,
            fixed_size_chunk: 0,
        };
        let sink: Arc<Mutex<Vec<LoreEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_for_cb = sink.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            sink_for_cb.lock().unwrap().push(event.clone());
        }));
        let status = lore::storage::put::put(
            globals(),
            lore::storage::put::LoreStoragePutArgs {
                handle,
                items: lore_revision::interface::LoreArray::from_vec(vec![item]),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "put for round-trip setup failed");
        let events = sink.lock().unwrap().clone();
        events
            .iter()
            .find_map(|e| match e {
                LoreEvent::StoragePutItemComplete(d) => Some(d.address),
                _ => None,
            })
            .expect("put should emit PUT_ITEM_COMPLETE with an address")
    }

    #[tokio::test]
    async fn get_empty_items_array_completes_with_status_zero() {
        // Empty items array: items_len=0 → Complete(0), no per-item events.
        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let (sink, callback) = make_get_sink();
        let status = lore::storage::get::get(
            globals(),
            lore::storage::get::LoreStorageGetArgs {
                handle,
                items: lore_revision::interface::LoreArray::default(),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0);

        let events = sink.lock().unwrap().clone();
        assert!(
            !events.iter().any(|e| matches!(
                e,
                GetCaptured::Header { .. }
                    | GetCaptured::Data { .. }
                    | GetCaptured::ItemComplete { .. }
            )),
            "no per-item events expected on empty input, got {events:?}",
        );
        assert!(events.contains(&GetCaptured::Complete(0)));
    }

    #[tokio::test]
    async fn get_zero_partition_item_rejects_invalid_args() {
        // Zero partition rejected: all-zero partition is reserved and yields
        // InvalidArguments at the item level.
        use lore_base::types::Address;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let item = lore::storage::get::LoreStorageGetItem {
            id: 5,
            partition: Partition::default(),
            address: Address::default(),
            streaming: 0,
            local_cache: 0,
        };

        let (sink, callback) = make_get_sink();
        let status = lore::storage::get::get(
            globals(),
            lore::storage::get::LoreStorageGetArgs {
                handle,
                items: lore_revision::interface::LoreArray::from_vec(vec![item]),
            },
            callback,
        )
        .await;
        // One item failed → call status 1 .
        assert_ne!(status, 0);

        let events = sink.lock().unwrap().clone();
        let complete = events
            .iter()
            .find_map(|e| match e {
                GetCaptured::ItemComplete {
                    id,
                    error_code,
                    address,
                } => Some((*id, *error_code, *address)),
                _ => None,
            })
            .expect("expected GET_ITEM_COMPLETE");
        assert_eq!(complete.0, 5);
        assert_eq!(
            complete.1,
            lore_revision::event::LoreErrorCode::InvalidArguments,
        );
        // Errored items carry zero address.
        assert_eq!(complete.2, Address::default());
    }

    #[tokio::test]
    async fn get_zero_hash_emits_empty_buffer_and_success() {
        // Zero-hash short-circuit: address.hash == Hash::default() → empty payload with
        // error_code None. Confirms the short-circuit in get_item.
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let partition = Partition::from([0x11u8; 16]);
        let context = Context::from([0x22u8; 16]);
        let address = Address {
            hash: Hash::default(),
            context,
        };
        let item = lore::storage::get::LoreStorageGetItem {
            id: 12,
            partition,
            address,
            streaming: 0,
            local_cache: 0,
        };

        let (sink, callback) = make_get_sink();
        let status = lore::storage::get::get(
            globals(),
            lore::storage::get::LoreStorageGetArgs {
                handle,
                items: lore_revision::interface::LoreArray::from_vec(vec![item]),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0);

        let events = sink.lock().unwrap().clone();
        let header = events.iter().find_map(|e| match e {
            GetCaptured::Header {
                id,
                size_content,
                address,
            } => Some((*id, *size_content, *address)),
            _ => None,
        });
        assert_eq!(header, Some((12, 0, address)));
        let data = events.iter().find_map(|e| match e {
            GetCaptured::Data {
                id, bytes, offset, ..
            } => Some((*id, bytes.clone(), *offset)),
            _ => None,
        });
        assert_eq!(data, Some((12, Vec::<u8>::new(), 0)));
        let complete = events.iter().find_map(|e| match e {
            GetCaptured::ItemComplete { id, error_code, .. } => Some((*id, *error_code)),
            _ => None,
        });
        assert_eq!(
            complete,
            Some((12, lore_revision::event::LoreErrorCode::None)),
        );
    }

    #[tokio::test]
    async fn get_missing_address_returns_address_not_found() {
        // Missing address: an address the store doesn't have yields
        // AddressNotFound on the terminal item event.
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let partition = Partition::from([0xAAu8; 16]);
        let context = Context::from([0xBBu8; 16]);
        // A hash nobody ever wrote — content-addressed lookup must miss.
        let address = Address {
            hash: Hash::from([0xCCu8; 32]),
            context,
        };
        let item = lore::storage::get::LoreStorageGetItem {
            id: 77,
            partition,
            address,
            streaming: 0,
            local_cache: 0,
        };

        let (sink, callback) = make_get_sink();
        let status = lore::storage::get::get(
            globals(),
            lore::storage::get::LoreStorageGetArgs {
                handle,
                items: lore_revision::interface::LoreArray::from_vec(vec![item]),
            },
            callback,
        )
        .await;
        assert_eq!(status, -1);

        let events = sink.lock().unwrap().clone();
        // No HEADER or DATA must appear for the missed read.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, GetCaptured::Header { .. })),
            "missed read must not emit HEADER, got {events:?}",
        );
        assert!(
            !events.iter().any(|e| matches!(e, GetCaptured::Data { .. })),
            "missed read must not emit DATA, got {events:?}",
        );
        let complete = events
            .iter()
            .find_map(|e| match e {
                GetCaptured::ItemComplete {
                    id,
                    error_code,
                    address,
                } => Some((*id, *error_code, *address)),
                _ => None,
            })
            .expect("expected GET_ITEM_COMPLETE");
        assert_eq!(complete.0, 77);
        assert_eq!(
            complete.1,
            lore_revision::event::LoreErrorCode::AddressNotFound,
        );
        assert_eq!(complete.2, Address::default());
    }

    #[tokio::test]
    async fn get_on_invalid_handle_returns_invalid_arguments() {
        // On unknown handle: the return value is 1 and a single enriched
        // Complete carries the handle-miss code (FFI code 1 for the dispatch
        // InvalidArguments).
        let (sink, callback) = make_get_sink();
        let status = lore::storage::get::get(
            globals(),
            lore::storage::get::LoreStorageGetArgs {
                handle: lore::storage::handle::LoreStore::INVALID,
                items: lore_revision::interface::LoreArray::from_vec(vec![
                    lore::storage::get::LoreStorageGetItem::default(),
                ]),
            },
            callback,
        )
        .await;
        assert_ne!(status, 0);
        let events = sink.lock().unwrap().clone();
        assert!(
            !events.contains(&GetCaptured::Error),
            "no Error event must fire on the migrated terminal arm, got {events:?}",
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, GetCaptured::Complete(s) if *s != 0))
        );
        assert!(
            !events.iter().any(|e| matches!(
                e,
                GetCaptured::Header { .. }
                    | GetCaptured::Data { .. }
                    | GetCaptured::ItemComplete { .. }
            )),
            "no per-item events on handle rejection, got {events:?}",
        );
    }

    #[tokio::test]
    async fn put_then_get_roundtrip_reads_back_exact_bytes() {
        // Round-trip: put a buffer, then get the returned address back — HEADER size matches
        // payload, DATA bytes equal payload, ITEM_COMPLETE carries the original address with
        // error_code None.
        use lore_base::types::Context;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let payload = b"round-trip through storage_put then storage_get".to_vec();
        let partition = Partition::from([0x01u8; 16]);
        let context = Context::from([0x02u8; 16]);
        let address = put_once(handle, partition, context, &payload).await;

        let item = lore::storage::get::LoreStorageGetItem {
            id: 100,
            partition,
            address,
            streaming: 0,
            local_cache: 0,
        };
        let (sink, callback) = make_get_sink();
        let status = lore::storage::get::get(
            globals(),
            lore::storage::get::LoreStorageGetArgs {
                handle,
                items: lore_revision::interface::LoreArray::from_vec(vec![item]),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0);

        let events = sink.lock().unwrap().clone();
        // HEADER → DATA → ITEM_COMPLETE, in order.
        let header_ix = events
            .iter()
            .position(|e| matches!(e, GetCaptured::Header { .. }))
            .expect("HEADER missing");
        let data_ix = events
            .iter()
            .position(|e| matches!(e, GetCaptured::Data { .. }))
            .expect("DATA missing");
        let complete_ix = events
            .iter()
            .position(|e| matches!(e, GetCaptured::ItemComplete { .. }))
            .expect("ITEM_COMPLETE missing");
        assert!(
            header_ix < data_ix && data_ix < complete_ix,
            "expected HEADER<DATA<ITEM_COMPLETE, got {events:?}",
        );

        if let GetCaptured::Header {
            id,
            size_content,
            address: h_addr,
        } = &events[header_ix]
        {
            assert_eq!(*id, 100);
            assert_eq!(*size_content, payload.len() as u64);
            assert_eq!(*h_addr, address);
        }
        if let GetCaptured::Data {
            id,
            bytes,
            offset,
            address: d_addr,
        } = &events[data_ix]
        {
            assert_eq!(*id, 100);
            assert_eq!(*offset, 0);
            assert_eq!(bytes, &payload);
            assert_eq!(*d_addr, address);
        }
        if let GetCaptured::ItemComplete {
            id,
            error_code,
            address: c_addr,
        } = &events[complete_ix]
        {
            assert_eq!(*id, 100);
            assert_eq!(*error_code, lore_revision::event::LoreErrorCode::None);
            assert_eq!(*c_addr, address);
        }
    }

    /// `LoreBytes` on `GET_DATA` events is valid only for the callback's invocation. This
    /// test pins two halves of that contract: (a) reading through the `ptr/len` pair inside
    /// the callback returns the expected bytes, and (b) once the callback returns, the
    /// dispatcher continues to deliver subsequent events (`ITEM_COMPLETE`, `Complete`,
    /// `End`) — releasing the byte buffer doesn't terminate the event stream.
    #[tokio::test]
    async fn get_data_bytes_are_valid_during_callback_and_more_events_follow() {
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::AtomicU64;
        use std::sync::atomic::Ordering;

        use lore_base::types::Context;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let payload = b"bytes lifetime contract probe".to_vec();
        let partition = Partition::from([0xb1u8; 16]);
        let context = Context::from([0xb2u8; 16]);
        let address = put_once(handle, partition, context, &payload).await;

        // Counters track event ordering observed inside the callback. `data_seen_at`
        // captures when GET_DATA fired; `events_after_data` counts events delivered
        // strictly after the GET_DATA callback returned.
        let saw_data = Arc::new(AtomicBool::new(false));
        let bytes_match = Arc::new(AtomicBool::new(false));
        let events_after_data = Arc::new(AtomicU64::new(0));
        let saw_data_for_cb = saw_data.clone();
        let bytes_match_for_cb = bytes_match.clone();
        let events_after_data_for_cb = events_after_data.clone();
        let payload_for_cb = payload.clone();

        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| match event {
            LoreEvent::StorageGetData(data) => {
                let observed = if data.bytes.len == 0 {
                    Vec::new()
                } else {
                    let slice = unsafe {
                        std::slice::from_raw_parts(data.bytes.ptr.cast::<u8>(), data.bytes.len)
                    };
                    slice.to_vec()
                };
                bytes_match_for_cb.store(observed == payload_for_cb, Ordering::Release);
                saw_data_for_cb.store(true, Ordering::Release);
            }
            _ => {
                if saw_data_for_cb.load(Ordering::Acquire) {
                    events_after_data_for_cb.fetch_add(1, Ordering::AcqRel);
                }
            }
        }));

        let item = lore::storage::get::LoreStorageGetItem {
            id: 1,
            partition,
            address,
            streaming: 0,
            local_cache: 0,
        };
        let status = lore::storage::get::get(
            globals(),
            lore::storage::get::LoreStorageGetArgs {
                handle,
                items: lore_revision::interface::LoreArray::from_vec(vec![item]),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0);
        assert!(saw_data.load(Ordering::Acquire), "GET_DATA must fire");
        assert!(
            bytes_match.load(Ordering::Acquire),
            "bytes read through `LoreBytes` during the callback must equal the put payload",
        );
        assert!(
            events_after_data.load(Ordering::Acquire) >= 2,
            "expected at least ITEM_COMPLETE + Complete after GET_DATA returned, got {}",
            events_after_data.load(Ordering::Acquire),
        );
    }

    /// Run a multi-item put and return `(call_status, per_item_completes)`.
    /// `put_once` is the single-item shorthand; this helper is for tests
    /// that need access to per-item outcomes (e.g. mixed success/failure).
    async fn put_items(
        handle: lore::storage::handle::LoreStore,
        items: Vec<lore::storage::put::LoreStoragePutItem>,
    ) -> (
        i32,
        Vec<lore_revision::store::event::LoreStoragePutItemCompleteEventData>,
    ) {
        let sink: Arc<Mutex<Vec<LoreEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_for_cb = sink.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            sink_for_cb.lock().unwrap().push(event.clone());
        }));
        let status = lore::storage::put::put(
            globals(),
            lore::storage::put::LoreStoragePutArgs {
                handle,
                items: lore_revision::interface::LoreArray::from_vec(items),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        let completes = events
            .iter()
            .filter_map(|e| match e {
                LoreEvent::StoragePutItemComplete(d) => Some(*d),
                _ => None,
            })
            .collect();
        (status, completes)
    }

    async fn get_items_capture(
        handle: lore::storage::handle::LoreStore,
        items: Vec<lore::storage::get::LoreStorageGetItem>,
    ) -> (i32, Vec<GetCaptured>) {
        let (sink, callback) = make_get_sink();
        let status = lore::storage::get::get(
            globals(),
            lore::storage::get::LoreStorageGetArgs {
                handle,
                items: lore_revision::interface::LoreArray::from_vec(items),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        (status, events)
    }

    #[tokio::test]
    async fn disk_backed_roundtrip_reads_back_exact_bytes() {
        // Round-trip on a disk-backed store: real repo,
        // put a payload, get it back, verify bytes match.
        use lore_base::types::Context;
        use lore_base::types::Partition;

        let repo_dir = tempdir("rt");
        let repo_path = repo_dir.path();
        create_repo(repo_path).await;

        let (open_sink, open_cb) = make_sink();
        let status = open::open(
            globals(),
            LoreStorageOpenArgs {
                repository_path: LoreString::from(repo_path.display().to_string().as_str()),
                in_memory: 0,
                ..Default::default()
            },
            open_cb,
        )
        .await;
        assert_eq!(status, 0);
        let id = take_opened(&open_sink.lock().unwrap()).expect("disk open should emit Opened");
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let payload = b"disk-backed put then get".to_vec();
        let partition = Partition::from([0xA1u8; 16]);
        let context = Context::from([0xA2u8; 16]);
        let address = put_once(handle, partition, context, &payload).await;

        let (status, events) = get_items_capture(
            handle,
            vec![lore::storage::get::LoreStorageGetItem {
                id: 1,
                partition,
                address,
                streaming: 0,
                local_cache: 0,
            }],
        )
        .await;
        assert_eq!(status, 0);
        let data = events
            .iter()
            .find_map(|e| match e {
                GetCaptured::Data { bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .expect("DATA missing");
        assert_eq!(data, payload);
    }

    #[tokio::test]
    async fn put_get_multiple_partitions_in_one_call() {
        // Multiple partitions in one call: a single handle can target multiple partitions via
        // per-item partition fields within one call.
        use lore_base::types::Context;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let part_red = Partition::from([0xCCu8; 16]);
        let part_blue = Partition::from([0xDDu8; 16]);
        let ctx = Context::from([0xEEu8; 16]);
        let payload_red = b"red partition payload".to_vec();
        let payload_blue = b"blue partition payload".to_vec();

        let put_items_vec = vec![
            lore::storage::put::LoreStoragePutItem {
                id: 1,
                partition: part_red,
                context: ctx,
                data: lore_revision::event::LoreBytes {
                    ptr: payload_red.as_ptr().cast(),
                    len: payload_red.len(),
                },
                remote_write: 0,
                local_cache: 0,
                fixed_size_chunk: 0,
            },
            lore::storage::put::LoreStoragePutItem {
                id: 2,
                partition: part_blue,
                context: ctx,
                data: lore_revision::event::LoreBytes {
                    ptr: payload_blue.as_ptr().cast(),
                    len: payload_blue.len(),
                },
                remote_write: 0,
                local_cache: 0,
                fixed_size_chunk: 0,
            },
        ];
        let (put_status, put_completes) = put_items(handle, put_items_vec).await;
        // Buffer lifetime: free buffers after put returns; verifies they
        // survived until Complete.
        drop(payload_red);
        drop(payload_blue);
        assert_eq!(put_status, 0);
        assert_eq!(put_completes.len(), 2);
        let addr_red = put_completes
            .iter()
            .find(|c| c.id == 1)
            .map(|c| c.address)
            .unwrap();
        let addr_blue = put_completes
            .iter()
            .find(|c| c.id == 2)
            .map(|c| c.address)
            .unwrap();
        assert_ne!(addr_red, addr_blue);

        let (get_status, get_events) = get_items_capture(
            handle,
            vec![
                lore::storage::get::LoreStorageGetItem {
                    id: 100,
                    partition: part_red,
                    address: addr_red,
                    streaming: 0,
                    local_cache: 0,
                },
                lore::storage::get::LoreStorageGetItem {
                    id: 200,
                    partition: part_blue,
                    address: addr_blue,
                    streaming: 0,
                    local_cache: 0,
                },
            ],
        )
        .await;
        assert_eq!(get_status, 0);

        let data_red = get_events
            .iter()
            .find_map(|e| match e {
                GetCaptured::Data { id: 100, bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .expect("red DATA missing");
        let data_blue = get_events
            .iter()
            .find_map(|e| match e {
                GetCaptured::Data { id: 200, bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .expect("blue DATA missing");
        assert_eq!(data_red, b"red partition payload");
        assert_eq!(data_blue, b"blue partition payload");
    }

    /// Serializes the tests that toggle the process-wide `LOCAL_ISOLATION` flag. Cargo runs
    /// tests in parallel; the flag's only writer of `false` is `IsolationGuard::drop`, so two
    /// overlapping guard windows let one test's drop clear the flag mid-`get` of the other.
    /// Held for the lifetime of an [`IsolationGuard`], the two windows can never overlap.
    static ISOLATION_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Save/restore guard for the process-wide `LOCAL_ISOLATION` flag so a test can toggle it
    /// without leaking the change to parallel tests. Acquiring [`ISOLATION_SERIAL`] keeps the
    /// isolation-sensitive tests from running their toggle windows concurrently. Other storage
    /// tests target matching `(partition, address)` pairs, so the flag being set does not change
    /// their outcome.
    struct IsolationGuard {
        previous: bool,
        _serial: tokio::sync::MutexGuard<'static, ()>,
    }

    impl IsolationGuard {
        async fn force_on() -> Self {
            let serial = ISOLATION_SERIAL.lock().await;
            let previous =
                lore_storage::LOCAL_ISOLATION.swap(true, std::sync::atomic::Ordering::AcqRel);
            Self {
                previous,
                _serial: serial,
            }
        }
    }

    impl Drop for IsolationGuard {
        fn drop(&mut self) {
            // Runs before `_serial` is dropped, so the flag is restored while the serial lock is
            // still held — the next guard observes a settled flag.
            lore_storage::LOCAL_ISOLATION
                .store(self.previous, std::sync::atomic::Ordering::Release);
        }
    }

    #[tokio::test]
    async fn cross_partition_read_fails_when_local_isolation_enabled() {
        // Partitions are content namespacing: with `LOCAL_ISOLATION` on,
        // reading an address stored under partition X via partition Y
        // must yield AddressNotFound. lore-server enables this flag at
        // startup (server.rs:1294); the storage API honors it through
        // `ReadOptions::default()`.
        use lore_base::types::Context;
        use lore_base::types::Partition;

        let _guard = IsolationGuard::force_on().await;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let part_x = Partition::from([0x01u8; 16]);
        let part_y = Partition::from([0x02u8; 16]);
        let ctx = Context::from([0x03u8; 16]);
        let payload = b"isolated to partition X".to_vec();
        let address = put_once(handle, part_x, ctx, &payload).await;

        let (status, events) = get_items_capture(
            handle,
            vec![lore::storage::get::LoreStorageGetItem {
                id: 9,
                partition: part_y,
                address,
                streaming: 0,
                local_cache: 0,
            }],
        )
        .await;
        assert_eq!(status, -1);
        let complete = events
            .iter()
            .find_map(|e| match e {
                GetCaptured::ItemComplete { id, error_code, .. } => Some((*id, *error_code)),
                _ => None,
            })
            .expect("expected GET_ITEM_COMPLETE");
        assert_eq!(
            complete,
            (9, lore_revision::event::LoreErrorCode::AddressNotFound),
        );
    }

    #[tokio::test]
    async fn duplicate_content_items_correlate_by_id() {
        // Two put items with the same payload share the same address —
        // their per-item events must remain distinguishable by `id`.
        // Symmetrically, three get items reading the same address each
        // get their own HEADER/DATA/ITEM_COMPLETE keyed by the original
        // id.
        use lore_base::types::Context;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let partition = Partition::from([0x10u8; 16]);
        let ctx = Context::from([0x20u8; 16]);
        let payload = b"duplicate content".to_vec();
        let mk_put_item = |id: u64| lore::storage::put::LoreStoragePutItem {
            id,
            partition,
            context: ctx,
            data: lore_revision::event::LoreBytes {
                ptr: payload.as_ptr().cast(),
                len: payload.len(),
            },
            remote_write: 0,
            local_cache: 0,
            fixed_size_chunk: 0,
        };
        let (put_status, put_completes) =
            put_items(handle, vec![mk_put_item(1), mk_put_item(2)]).await;
        drop(payload);
        assert_eq!(put_status, 0);
        assert_eq!(put_completes.len(), 2);
        let mut ids: Vec<u64> = put_completes.iter().map(|c| c.id).collect();
        ids.sort();
        assert_eq!(ids, vec![1, 2]);
        let addr = put_completes[0].address;
        assert_eq!(put_completes[1].address, addr);

        let get_ids = [10u64, 20, 30];
        let get_items_vec: Vec<_> = get_ids
            .iter()
            .map(|id| lore::storage::get::LoreStorageGetItem {
                id: *id,
                partition,
                address: addr,
                streaming: 0,
                local_cache: 0,
            })
            .collect();
        let (get_status, events) = get_items_capture(handle, get_items_vec).await;
        assert_eq!(get_status, 0);

        // One DATA per id, all carrying the same bytes.
        for expected_id in get_ids {
            let bytes = events
                .iter()
                .find_map(|e| match e {
                    GetCaptured::Data { id, bytes, .. } if *id == expected_id => {
                        Some(bytes.clone())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("DATA for id {expected_id} missing"));
            assert_eq!(bytes, b"duplicate content");
        }
        let complete_count = events
            .iter()
            .filter(|e| matches!(e, GetCaptured::ItemComplete { .. }))
            .count();
        assert_eq!(complete_count, 3);
    }

    #[tokio::test]
    async fn mixed_get_success_and_failure_items_complete_independently() {
        // Mixed item outcomes: one stored, one zero-hash short-circuit,
        // one missing — each emits its own terminal event with its own
        // error code, and the per-item HEADER/DATA/ITEM_COMPLETE order is
        // preserved per id.
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let partition = Partition::from([0x42u8; 16]);
        let ctx = Context::from([0x55u8; 16]);
        let payload = b"the only payload that exists".to_vec();
        let real_addr = put_once(handle, partition, ctx, &payload).await;

        let zero_hash_addr = Address {
            hash: Hash::default(),
            context: ctx,
        };
        let missing_addr = Address {
            hash: Hash::from([0x77u8; 32]),
            context: ctx,
        };
        let items = vec![
            lore::storage::get::LoreStorageGetItem {
                id: 1,
                partition,
                address: real_addr,
                streaming: 0,
                local_cache: 0,
            },
            lore::storage::get::LoreStorageGetItem {
                id: 2,
                partition,
                address: zero_hash_addr,
                streaming: 0,
                local_cache: 0,
            },
            lore::storage::get::LoreStorageGetItem {
                id: 3,
                partition,
                address: missing_addr,
                streaming: 0,
                local_cache: 0,
            },
        ];
        let (status, events) = get_items_capture(handle, items).await;
        // One item failed → call-level status is the internal code -1.
        assert_eq!(status, -1);

        // Per-item terminal events.
        let collect_complete = |target_id: u64| -> Option<lore_revision::event::LoreErrorCode> {
            events.iter().find_map(|e| match e {
                GetCaptured::ItemComplete { id, error_code, .. } if *id == target_id => {
                    Some(*error_code)
                }
                _ => None,
            })
        };
        assert_eq!(
            collect_complete(1),
            Some(lore_revision::event::LoreErrorCode::None),
        );
        assert_eq!(
            collect_complete(2),
            Some(lore_revision::event::LoreErrorCode::None),
        );
        assert_eq!(
            collect_complete(3),
            Some(lore_revision::event::LoreErrorCode::AddressNotFound),
        );

        // Per-id ordering: per-id ordering HEADER → DATA → ITEM_COMPLETE
        // is preserved even with parallel items interleaving.
        for target_id in [1u64, 2] {
            let positions: Vec<(usize, &'static str)> = events
                .iter()
                .enumerate()
                .filter_map(|(ix, e)| match e {
                    GetCaptured::Header { id, .. } if *id == target_id => Some((ix, "header")),
                    GetCaptured::Data { id, .. } if *id == target_id => Some((ix, "data")),
                    GetCaptured::ItemComplete { id, .. } if *id == target_id => {
                        Some((ix, "complete"))
                    }
                    _ => None,
                })
                .collect();
            let labels: Vec<&'static str> = positions.iter().map(|(_, l)| *l).collect();
            assert_eq!(
                labels,
                vec!["header", "data", "complete"],
                "id {target_id}: expected HEADER<DATA<COMPLETE, got {labels:?}",
            );
        }
        // The missing-address item must skip HEADER and DATA.
        let missed: Vec<&'static str> = events
            .iter()
            .filter_map(|e| match e {
                GetCaptured::Header { id, .. } if *id == 3 => Some("header"),
                GetCaptured::Data { id, .. } if *id == 3 => Some("data"),
                GetCaptured::ItemComplete { id, .. } if *id == 3 => Some("complete"),
                _ => None,
            })
            .collect();
        assert_eq!(
            missed,
            vec!["complete"],
            "missing-address item should only emit COMPLETE, got {missed:?}",
        );
    }

    #[tokio::test]
    async fn parallel_items_emit_one_terminal_event_each() {
        // Concurrency: items run concurrently. We don't measure wall time
        // here (flaky on CI); instead we verify that with N items, the
        // get call emits exactly N HEADER, N DATA, and N ITEM_COMPLETE
        // events — i.e. the parallel join did not lose any items.
        use lore_base::types::Context;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let partition = Partition::from([0xAAu8; 16]);
        let ctx = Context::from([0xBBu8; 16]);
        // Distinct payloads so the resulting addresses differ.
        let payloads: Vec<Vec<u8>> = (0..16)
            .map(|i| format!("parallel item {i}").into_bytes())
            .collect();
        let mut addresses = Vec::with_capacity(payloads.len());
        for p in &payloads {
            addresses.push(put_once(handle, partition, ctx, p).await);
        }

        let items: Vec<_> = addresses
            .iter()
            .enumerate()
            .map(|(ix, addr)| lore::storage::get::LoreStorageGetItem {
                id: ix as u64,
                partition,
                address: *addr,
                streaming: 0,
                local_cache: 0,
            })
            .collect();
        let (status, events) = get_items_capture(handle, items).await;
        assert_eq!(status, 0);

        let header_count = events
            .iter()
            .filter(|e| matches!(e, GetCaptured::Header { .. }))
            .count();
        let data_count = events
            .iter()
            .filter(|e| matches!(e, GetCaptured::Data { .. }))
            .count();
        let complete_count = events
            .iter()
            .filter(|e| matches!(e, GetCaptured::ItemComplete { .. }))
            .count();
        let n = payloads.len();
        assert_eq!(header_count, n, "expected {n} HEADER events");
        assert_eq!(data_count, n, "expected {n} DATA events");
        assert_eq!(complete_count, n, "expected {n} ITEM_COMPLETE events");

        // Each get id matches its expected payload.
        for (ix, payload) in payloads.iter().enumerate() {
            let bytes = events
                .iter()
                .find_map(|e| match e {
                    GetCaptured::Data {
                        id: got_id, bytes, ..
                    } if *got_id == ix as u64 => Some(bytes.clone()),
                    _ => None,
                })
                .unwrap();
            assert_eq!(&bytes, payload);
        }
    }

    #[tokio::test]
    async fn aggregate_call_error_uses_invalid_arguments_when_any_item_invalid() {
        // Severity ordering: a single InvalidArguments per-item code wins over
        // AddressNotFound at the call level. The enriched Complete carries the
        // aggregated error's FFI code (1 for InvalidArguments).
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let bad_partition_item = lore::storage::get::LoreStorageGetItem {
            id: 1,
            partition: Partition::default(),
            address: Address {
                hash: Hash::from([0x11u8; 32]),
                context: Context::default(),
            },
            streaming: 0,
            local_cache: 0,
        };
        let missing_item = lore::storage::get::LoreStorageGetItem {
            id: 2,
            partition: Partition::from([0x42u8; 16]),
            address: Address {
                hash: Hash::from([0x77u8; 32]),
                context: Context::default(),
            },
            streaming: 0,
            local_cache: 0,
        };

        let sink: Arc<Mutex<Vec<LoreEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_for_cb = sink.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            sink_for_cb.lock().unwrap().push(event.clone());
        }));
        let status = lore::storage::get::get(
            globals(),
            lore::storage::get::LoreStorageGetArgs {
                handle,
                items: lore_revision::interface::LoreArray::from_vec(vec![
                    bad_partition_item,
                    missing_item,
                ]),
            },
            callback,
        )
        .await;
        assert_ne!(status, 0);

        let events = sink.lock().unwrap().clone();
        assert!(
            !events.iter().any(|e| matches!(e, LoreEvent::Error(_))),
            "no Error event must fire on the migrated terminal arm",
        );
        let complete = events
            .iter()
            .find_map(|e| match e {
                LoreEvent::Complete(d) => Some(d.clone()),
                _ => None,
            })
            .expect("expected Complete event");
        // InvalidArguments carries FFI code 1; it wins the aggregate.
        assert_ne!(complete.status, 0);
        assert_ne!(complete.error.error_code, 0);
    }

    #[tokio::test]
    async fn aggregate_call_error_uses_internal_when_only_address_not_found() {
        // Without an InvalidArguments item, the call-level summary maps the
        // dominant AddressNotFound aggregate to the internal error — the FFI
        // surface has no AddressNotFound variant for batch ops. The enriched
        // Complete carries the internal FFI code (-1).
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let part = Partition::from([0x42u8; 16]);
        let missing = |id: u64, byte: u8| lore::storage::get::LoreStorageGetItem {
            id,
            partition: part,
            address: Address {
                hash: Hash::from([byte; 32]),
                context: Context::default(),
            },
            streaming: 0,
            local_cache: 0,
        };

        let sink: Arc<Mutex<Vec<LoreEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_for_cb = sink.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            sink_for_cb.lock().unwrap().push(event.clone());
        }));
        let status = lore::storage::get::get(
            globals(),
            lore::storage::get::LoreStorageGetArgs {
                handle,
                items: lore_revision::interface::LoreArray::from_vec(vec![
                    missing(1, 0x77),
                    missing(2, 0x88),
                ]),
            },
            callback,
        )
        .await;
        assert_eq!(status, -1);

        let events = sink.lock().unwrap().clone();
        assert!(
            !events.iter().any(|e| matches!(e, LoreEvent::Error(_))),
            "no Error event must fire on the migrated terminal arm",
        );
        let complete = events
            .iter()
            .find_map(|e| match e {
                LoreEvent::Complete(d) => Some(d.clone()),
                _ => None,
            })
            .expect("expected Complete event");
        // The aggregated AddressNotFound maps to the internal error, FFI code -1.
        assert_eq!(complete.status, -1);
        assert_eq!(complete.error.error_code, -1);
    }

    #[tokio::test]
    async fn two_in_memory_opens_isolate_writes() {
        // Independent in-memory backends: two in-memory opens get independent backends — a
        // write through handle A is invisible through handle B.
        use lore_base::types::Context;
        use lore_base::types::Partition;

        let (sink_a, cb_a) = make_sink();
        assert_eq!(open_in_memory(cb_a).await, 0);
        let id_a = take_opened(&sink_a.lock().unwrap()).unwrap();
        let handle_a = lore::storage::handle::LoreStore { handle_id: id_a };

        let (sink_b, cb_b) = make_sink();
        assert_eq!(open_in_memory(cb_b).await, 0);
        let id_b = take_opened(&sink_b.lock().unwrap()).unwrap();
        let handle_b = lore::storage::handle::LoreStore { handle_id: id_b };

        let payload = b"only in handle A".to_vec();
        let partition = Partition::from([0xF1u8; 16]);
        let ctx = Context::from([0xF2u8; 16]);
        let address = put_once(handle_a, partition, ctx, &payload).await;

        let (status, events) = get_items_capture(
            handle_b,
            vec![lore::storage::get::LoreStorageGetItem {
                id: 1,
                partition,
                address,
                streaming: 0,
                local_cache: 0,
            }],
        )
        .await;
        // Handle B never saw the write — read must miss.
        assert_eq!(status, -1);
        let complete = events
            .iter()
            .find_map(|e| match e {
                GetCaptured::ItemComplete { id, error_code, .. } => Some((*id, *error_code)),
                _ => None,
            })
            .expect("expected GET_ITEM_COMPLETE");
        assert_eq!(
            complete,
            (1, lore_revision::event::LoreErrorCode::AddressNotFound),
        );
    }

    #[tokio::test]
    async fn put_caller_buffer_can_be_freed_after_complete() {
        // Buffer lifetime: caller frees the source buffer after the
        // call returns. The address must still be readable from the
        // store (which means put copied the bytes during dispatch).
        use lore_base::types::Context;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let partition = Partition::from([0x66u8; 16]);
        let ctx = Context::from([0x77u8; 16]);
        let payload = b"freed-after-complete".to_vec();
        let address = put_once(handle, partition, ctx, &payload).await;
        // Drop the original buffer; then prove get still sees the bytes.
        drop(payload);

        let (status, events) = get_items_capture(
            handle,
            vec![lore::storage::get::LoreStorageGetItem {
                id: 1,
                partition,
                address,
                streaming: 0,
                local_cache: 0,
            }],
        )
        .await;
        assert_eq!(status, 0);
        let bytes = events
            .iter()
            .find_map(|e| match e {
                GetCaptured::Data { bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(bytes, b"freed-after-complete");
    }

    async fn flush_handle(handle: lore::storage::handle::LoreStore) -> (i32, Vec<Captured>) {
        let (sink, callback) = make_sink();
        let status = lore::storage::flush::flush(
            globals(),
            lore::storage::flush::LoreStorageFlushArgs { handle },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        (status, events)
    }

    #[tokio::test]
    async fn in_memory_flush_completes_with_status_zero() {
        // In-memory flush: in-memory flush is a no-op that still produces a
        // clean Complete(0) event.
        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let (status, events) = flush_handle(handle).await;
        assert_eq!(status, 0);
        assert!(events.contains(&Captured::Complete(0)));
        assert!(!events.contains(&Captured::Error));
    }

    #[tokio::test]
    async fn disk_backed_flush_completes_with_status_zero() {
        // Disk-backed flush: disk-backed flush returns successfully on a real repo.
        // We don't directly observe fsync but a successful flush + later
        // ops on the same handle prove the path is wired.
        use lore_base::types::Context;
        use lore_base::types::Partition;

        let repo_dir = tempdir("flush");
        let repo_path = repo_dir.path();
        create_repo(repo_path).await;

        let (open_sink, open_cb) = make_sink();
        let status = open::open(
            globals(),
            LoreStorageOpenArgs {
                repository_path: LoreString::from(repo_path.display().to_string().as_str()),
                in_memory: 0,
                ..Default::default()
            },
            open_cb,
        )
        .await;
        assert_eq!(status, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        // Put then flush then get — proves flush doesn't break the
        // handle and content survives.
        let payload = b"flush survives".to_vec();
        let partition = Partition::from([0xB1u8; 16]);
        let context = Context::from([0xB2u8; 16]);
        let address = put_once(handle, partition, context, &payload).await;

        let (status, events) = flush_handle(handle).await;
        assert_eq!(status, 0);
        assert!(events.contains(&Captured::Complete(0)));

        let (status, events) = get_items_capture(
            handle,
            vec![lore::storage::get::LoreStorageGetItem {
                id: 1,
                partition,
                address,
                streaming: 0,
                local_cache: 0,
            }],
        )
        .await;
        assert_eq!(status, 0);
        let data = events
            .iter()
            .find_map(|e| match e {
                GetCaptured::Data { bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .expect("DATA missing");
        assert_eq!(data, payload);
    }

    #[tokio::test]
    async fn flush_on_invalid_handle_returns_invalid_arguments() {
        // On unknown handle: the return value is 1 and a single enriched
        // Complete carries the handle-miss code (FFI code 1 for the dispatch
        // InvalidArguments).
        let (sink, callback) = make_sink();
        let status = lore::storage::flush::flush(
            globals(),
            lore::storage::flush::LoreStorageFlushArgs {
                handle: lore::storage::handle::LoreStore::INVALID,
            },
            callback,
        )
        .await;
        assert_ne!(status, 0);
        let events = sink.lock().unwrap().clone();
        assert!(
            !events.contains(&Captured::Error),
            "no Error event must fire on the migrated terminal arm, got {events:?}",
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Captured::Complete(s) if *s != 0))
        );
    }

    /// Capture for `get_metadata` items (the terminal
    /// `GET_METADATA_ITEM_COMPLETE` carries `(id, address, fragment, error_code)`).
    #[derive(Debug, Clone, PartialEq)]
    enum GetMetadataCaptured {
        Complete {
            id: u64,
            address: lore_base::types::Address,
            fragment: lore_base::types::Fragment,
            error_code: lore_revision::event::LoreErrorCode,
        },
        Error,
        CallComplete(i32),
        Other,
    }

    fn make_get_metadata_sink() -> (Arc<Mutex<Vec<GetMetadataCaptured>>>, LoreEventCallback) {
        let sink: Arc<Mutex<Vec<GetMetadataCaptured>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_for_cb = sink.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            let rec = match event {
                LoreEvent::StorageGetMetadataItemComplete(d) => GetMetadataCaptured::Complete {
                    id: d.id,
                    address: d.address,
                    fragment: d.fragment,
                    error_code: d.error_code,
                },
                LoreEvent::Error(_) => GetMetadataCaptured::Error,
                LoreEvent::Complete(d) => GetMetadataCaptured::CallComplete(d.status),
                _ => GetMetadataCaptured::Other,
            };
            sink_for_cb.lock().unwrap().push(rec);
        }));
        (sink, callback)
    }

    async fn get_metadata_items(
        handle: lore::storage::handle::LoreStore,
        items: Vec<lore::storage::get_metadata::LoreStorageGetMetadataItem>,
    ) -> (i32, Vec<GetMetadataCaptured>) {
        let (sink, callback) = make_get_metadata_sink();
        let status = lore::storage::get_metadata::get_metadata(
            globals(),
            lore::storage::get_metadata::LoreStorageGetMetadataArgs {
                handle,
                items: lore_revision::interface::LoreArray::from_vec(items),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        (status, events)
    }

    #[tokio::test]
    async fn get_metadata_returns_fragment_for_stored_address() {
        // get_metadata success path.
        use lore_base::types::Context;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let payload = b"queryable content".to_vec();
        let partition = Partition::from([0xC1u8; 16]);
        let context = Context::from([0xC2u8; 16]);
        let address = put_once(handle, partition, context, &payload).await;

        let (status, events) = get_metadata_items(
            handle,
            vec![lore::storage::get_metadata::LoreStorageGetMetadataItem {
                id: 1,
                partition,
                address,
            }],
        )
        .await;
        assert_eq!(status, 0);
        let complete = events
            .iter()
            .find_map(|e| match e {
                GetMetadataCaptured::Complete {
                    id,
                    error_code,
                    fragment,
                    address,
                } => Some((*id, *error_code, *fragment, *address)),
                _ => None,
            })
            .expect("GET_METADATA_ITEM_COMPLETE missing");
        assert_eq!(complete.0, 1);
        assert_eq!(complete.1, lore_revision::event::LoreErrorCode::None);
        assert_eq!(complete.2.size_content, payload.len() as u64);
        assert_eq!(complete.3, address);
    }

    #[tokio::test]
    async fn get_metadata_missing_address_returns_address_not_found() {
        // Miss path: caller uses error_code as the "exists?" check.
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let partition = Partition::from([0xD1u8; 16]);
        let address = Address {
            hash: Hash::from([0xEEu8; 32]),
            context: Context::from([0xD2u8; 16]),
        };

        let (status, events) = get_metadata_items(
            handle,
            vec![lore::storage::get_metadata::LoreStorageGetMetadataItem {
                id: 7,
                partition,
                address,
            }],
        )
        .await;
        assert_eq!(status, -1);
        let complete = events
            .iter()
            .find_map(|e| match e {
                GetMetadataCaptured::Complete { id, error_code, .. } => Some((*id, *error_code)),
                _ => None,
            })
            .expect("GET_METADATA_ITEM_COMPLETE missing");
        assert_eq!(
            complete,
            (7, lore_revision::event::LoreErrorCode::AddressNotFound),
        );
    }

    #[tokio::test]
    async fn get_metadata_zero_partition_rejects_invalid_args() {
        // Zero partition is rejected.
        use lore_base::types::Address;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let (status, events) = get_metadata_items(
            handle,
            vec![lore::storage::get_metadata::LoreStorageGetMetadataItem {
                id: 5,
                partition: Partition::default(),
                address: Address::default(),
            }],
        )
        .await;
        assert_ne!(status, 0);
        let complete = events
            .iter()
            .find_map(|e| match e {
                GetMetadataCaptured::Complete { id, error_code, .. } => Some((*id, *error_code)),
                _ => None,
            })
            .expect("GET_METADATA_ITEM_COMPLETE missing");
        assert_eq!(
            complete,
            (5, lore_revision::event::LoreErrorCode::InvalidArguments),
        );
    }

    #[tokio::test]
    async fn get_metadata_empty_items_completes_with_status_zero() {
        // Empty items array.
        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let (status, events) = get_metadata_items(handle, vec![]).await;
        assert_eq!(status, 0);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, GetMetadataCaptured::Complete { .. })),
            "no per-item events on empty input, got {events:?}",
        );
        assert!(events.contains(&GetMetadataCaptured::CallComplete(0)));
    }

    /// Capture for obliterate items.
    #[derive(Debug, Clone, PartialEq)]
    enum ObliterateCaptured {
        Complete {
            id: u64,
            address: lore_base::types::Address,
            local_success: u8,
            remote_success: u8,
            local_skipped: u8,
            remote_skipped: u8,
            error_code: lore_revision::event::LoreErrorCode,
        },
        Error,
        CallComplete(i32),
        Other,
    }

    fn make_obliterate_sink() -> (Arc<Mutex<Vec<ObliterateCaptured>>>, LoreEventCallback) {
        let sink: Arc<Mutex<Vec<ObliterateCaptured>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_for_cb = sink.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            let rec = match event {
                LoreEvent::StorageObliterateItemComplete(d) => ObliterateCaptured::Complete {
                    id: d.id,
                    address: d.address,
                    local_success: d.local_success,
                    remote_success: d.remote_success,
                    local_skipped: d.local_skipped,
                    remote_skipped: d.remote_skipped,
                    error_code: d.error_code,
                },
                LoreEvent::Error(_) => ObliterateCaptured::Error,
                LoreEvent::Complete(d) => ObliterateCaptured::CallComplete(d.status),
                _ => ObliterateCaptured::Other,
            };
            sink_for_cb.lock().unwrap().push(rec);
        }));
        (sink, callback)
    }

    async fn obliterate_items(
        handle: lore::storage::handle::LoreStore,
        items: Vec<lore::storage::obliterate::LoreStorageObliterateItem>,
    ) -> (i32, Vec<ObliterateCaptured>) {
        let (sink, callback) = make_obliterate_sink();
        let status = lore::storage::obliterate::obliterate(
            globals(),
            lore::storage::obliterate::LoreStorageObliterateArgs {
                handle,
                items: lore_revision::interface::LoreArray::from_vec(items),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        (status, events)
    }

    #[tokio::test]
    async fn obliterate_present_item_succeeds_and_marks_payload_gone() {
        // No remote configured: with no remote, remote_success=1 (no-op success).
        // The store keeps the entry as a tombstone — query still hits
        // but the fragment carries the `PayloadObliterated` flag and no
        // longer reports `PayloadStoredLocal`. A subsequent `get` would
        // miss because the payload is gone.
        use lore_base::types::Context;
        use lore_base::types::FragmentFlags;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let payload = b"to be erased".to_vec();
        let partition = Partition::from([0xE1u8; 16]);
        let context = Context::from([0xE2u8; 16]);
        let address = put_once(handle, partition, context, &payload).await;

        let (status, events) = obliterate_items(
            handle,
            vec![lore::storage::obliterate::LoreStorageObliterateItem {
                id: 1,
                partition,
                address,
            }],
        )
        .await;
        assert_eq!(status, 0);
        let complete = events
            .iter()
            .find_map(|e| match e {
                ObliterateCaptured::Complete {
                    id,
                    local_success,
                    remote_success,
                    local_skipped,
                    remote_skipped,
                    error_code,
                    ..
                } => Some((
                    *id,
                    *local_success,
                    *remote_success,
                    *local_skipped,
                    *remote_skipped,
                    *error_code,
                )),
                _ => None,
            })
            .expect("OBLITERATE_ITEM_COMPLETE missing");
        // No remote_config: local leg ran (success=1), remote leg was skipped (skipped=1).
        assert_eq!(
            complete,
            (1, 1, 0, 0, 1, lore_revision::event::LoreErrorCode::None),
        );

        // Tombstone observable via get_metadata: entry still matches, but the
        // fragment flag set says the payload is gone.
        let (q_status, q_events) = get_metadata_items(
            handle,
            vec![lore::storage::get_metadata::LoreStorageGetMetadataItem {
                id: 99,
                partition,
                address,
            }],
        )
        .await;
        assert_eq!(q_status, 0);
        let fragment = q_events
            .iter()
            .find_map(|e| match e {
                GetMetadataCaptured::Complete { fragment, .. } => Some(*fragment),
                _ => None,
            })
            .expect("post-obliterate get_metadata event missing");
        let flags = FragmentFlags::from_bits_truncate(fragment.flags);
        assert!(
            flags.contains(FragmentFlags::PayloadObliterated),
            "obliterated entry must carry PayloadObliterated, got {flags:?}",
        );
        assert!(
            !flags.contains(FragmentFlags::PayloadStoredLocal),
            "obliterated entry must drop PayloadStoredLocal, got {flags:?}",
        );
    }

    #[tokio::test]
    async fn obliterate_absent_item_is_idempotent_success() {
        // Absent address: address not present locally still reports
        // local_success=1; the underlying store treats absence as
        // nothing-to-do.
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let absent = Address {
            hash: Hash::from([0xABu8; 32]),
            context: Context::from([0xCDu8; 16]),
        };
        let (status, events) = obliterate_items(
            handle,
            vec![lore::storage::obliterate::LoreStorageObliterateItem {
                id: 7,
                partition: Partition::from([0xEFu8; 16]),
                address: absent,
            }],
        )
        .await;
        assert_eq!(status, 0);
        let complete = events
            .iter()
            .find_map(|e| match e {
                ObliterateCaptured::Complete {
                    id,
                    local_success,
                    remote_success,
                    local_skipped,
                    remote_skipped,
                    error_code,
                    ..
                } => Some((
                    *id,
                    *local_success,
                    *remote_success,
                    *local_skipped,
                    *remote_skipped,
                    *error_code,
                )),
                _ => None,
            })
            .expect("OBLITERATE_ITEM_COMPLETE missing");
        // No remote_config: local-side absent-address is idempotent success; remote leg skipped.
        assert_eq!(
            complete,
            (7, 1, 0, 0, 1, lore_revision::event::LoreErrorCode::None),
        );
    }

    /// Capture for copy items.
    #[derive(Debug, Clone, PartialEq)]
    enum CopyCaptured {
        Complete {
            id: u64,
            source_partition: lore_base::types::Partition,
            target_partition: lore_base::types::Partition,
            source_address: lore_base::types::Address,
            target_context: lore_base::types::Context,
            error_code: lore_revision::event::LoreErrorCode,
        },
        Error,
        CallComplete(i32),
        Other,
    }

    fn make_copy_sink() -> (Arc<Mutex<Vec<CopyCaptured>>>, LoreEventCallback) {
        let sink: Arc<Mutex<Vec<CopyCaptured>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_for_cb = sink.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            let rec = match event {
                LoreEvent::StorageCopyItemComplete(d) => CopyCaptured::Complete {
                    id: d.id,
                    source_partition: d.source_partition,
                    target_partition: d.target_partition,
                    source_address: d.source_address,
                    target_context: d.target_context,
                    error_code: d.error_code,
                },
                LoreEvent::Error(_) => CopyCaptured::Error,
                LoreEvent::Complete(d) => CopyCaptured::CallComplete(d.status),
                _ => CopyCaptured::Other,
            };
            sink_for_cb.lock().unwrap().push(rec);
        }));
        (sink, callback)
    }

    async fn copy_items(
        handle: lore::storage::handle::LoreStore,
        items: Vec<lore::storage::copy::LoreStorageCopyItem>,
    ) -> (i32, Vec<CopyCaptured>) {
        let (sink, callback) = make_copy_sink();
        let status = lore::storage::copy::copy(
            globals(),
            lore::storage::copy::LoreStorageCopyArgs {
                handle,
                items: lore_revision::interface::LoreArray::from_vec(items),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        (status, events)
    }

    #[tokio::test]
    async fn copy_across_partitions_relocates_payload() {
        // Cross-partition copy: cross-partition local copy succeeds; the
        // target partition's entry retains PayloadStoredLocal but not
        // PayloadStoredDurable since no remote was involved.
        // Round-trip is verified via get from the target partition.
        use lore_base::types::Context;
        use lore_base::types::FragmentFlags;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let payload = b"replicate me".to_vec();
        let source_partition = Partition::from([0xAA; 16]);
        let target_partition = Partition::from([0xBB; 16]);
        let context = Context::from([0xCC; 16]);
        let address = put_once(handle, source_partition, context, &payload).await;

        let (status, events) = copy_items(
            handle,
            vec![lore::storage::copy::LoreStorageCopyItem {
                id: 1,
                source_partition,
                target_partition,
                source_address: address,
                target_context: address.context,
            }],
        )
        .await;
        assert_eq!(status, 0);
        let complete = events
            .iter()
            .find_map(|e| match e {
                CopyCaptured::Complete {
                    id,
                    error_code,
                    source_partition,
                    target_partition,
                    source_address,
                    target_context,
                } => Some((
                    *id,
                    *error_code,
                    *source_partition,
                    *target_partition,
                    *source_address,
                    *target_context,
                )),
                _ => None,
            })
            .expect("COPY_ITEM_COMPLETE missing");
        assert_eq!(complete.0, 1);
        assert_eq!(complete.1, lore_revision::event::LoreErrorCode::None);
        assert_eq!(complete.2, source_partition);
        assert_eq!(complete.3, target_partition);
        assert_eq!(complete.4, address);
        assert_eq!(complete.5, address.context);

        // Verify the target carries the payload locally without Durable.
        let _guard = IsolationGuard::force_on().await;
        let (q_status, q_events) = get_metadata_items(
            handle,
            vec![lore::storage::get_metadata::LoreStorageGetMetadataItem {
                id: 99,
                partition: target_partition,
                address,
            }],
        )
        .await;
        assert_eq!(q_status, 0);
        let fragment = q_events
            .iter()
            .find_map(|e| match e {
                GetMetadataCaptured::Complete { fragment, .. } => Some(*fragment),
                _ => None,
            })
            .expect("post-copy get_metadata event missing");
        let flags = FragmentFlags::from_bits_truncate(fragment.flags);
        assert!(
            flags.contains(FragmentFlags::PayloadStoredLocal),
            "target must have PayloadStoredLocal, got {flags:?}",
        );
        assert!(
            !flags.contains(FragmentFlags::PayloadStoredDurable),
            "target must NOT have PayloadStoredDurable on local-only path, got {flags:?}",
        );
    }

    #[tokio::test]
    async fn copy_missing_source_returns_address_not_found() {
        // Missing source: remote not usable + no local source payload → fail
        // with ADDRESS_NOT_FOUND.
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let absent_addr = Address {
            hash: Hash::from([0x77u8; 32]),
            context: Context::from([0x88u8; 16]),
        };
        let (status, events) = copy_items(
            handle,
            vec![lore::storage::copy::LoreStorageCopyItem {
                id: 7,
                source_partition: Partition::from([0xAA; 16]),
                target_partition: Partition::from([0xBB; 16]),
                source_address: absent_addr,
                target_context: absent_addr.context,
            }],
        )
        .await;
        assert_eq!(status, -1);
        let complete = events
            .iter()
            .find_map(|e| match e {
                CopyCaptured::Complete { id, error_code, .. } => Some((*id, *error_code)),
                _ => None,
            })
            .expect("COPY_ITEM_COMPLETE missing");
        assert_eq!(
            complete,
            (7, lore_revision::event::LoreErrorCode::AddressNotFound),
        );
    }

    #[tokio::test]
    async fn copy_idempotent_via_flag_merge() {
        // Idempotency: copy onto an existing target flag-merges via the
        // store path. A second copy of the same `(source, target)`
        // succeeds without reporting an error.
        use lore_base::types::Context;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let payload = b"twice".to_vec();
        let source_partition = Partition::from([0x33; 16]);
        let target_partition = Partition::from([0x44; 16]);
        let context = Context::from([0x55; 16]);
        let address = put_once(handle, source_partition, context, &payload).await;

        let item = lore::storage::copy::LoreStorageCopyItem {
            id: 1,
            source_partition,
            target_partition,
            source_address: address,
            target_context: address.context,
        };
        let (status_first, _) = copy_items(handle, vec![item]).await;
        assert_eq!(status_first, 0);
        let (status_second, events) = copy_items(handle, vec![item]).await;
        assert_eq!(status_second, 0, "second copy must succeed");
        let complete = events
            .iter()
            .find_map(|e| match e {
                CopyCaptured::Complete { id, error_code, .. } => Some((*id, *error_code)),
                _ => None,
            })
            .expect("COPY_ITEM_COMPLETE missing");
        assert_eq!(complete, (1, lore_revision::event::LoreErrorCode::None),);
    }

    #[tokio::test]
    async fn copy_same_partition_new_context_round_trips_via_get() {
        // In-partition payload duplication: copying a fragment from `(P, H, C1)` to
        // `(P, H, C2)` creates a second entry pointing at the same payload — get against
        // the new (P, H, C2) tuple must return the same bytes as the source.
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let payload = b"in-partition retag payload".to_vec();
        let partition = Partition::from([0x33; 16]);
        let source_context = Context::from([0xC1; 16]);
        let target_context = Context::from([0xC2; 16]);
        let source_address = put_once(handle, partition, source_context, &payload).await;
        let destination_address = Address {
            hash: source_address.hash,
            context: target_context,
        };

        // Issue the copy with same source/target partition but different target_context.
        let (status, events) = copy_items(
            handle,
            vec![lore::storage::copy::LoreStorageCopyItem {
                id: 11,
                source_partition: partition,
                target_partition: partition,
                source_address,
                target_context,
            }],
        )
        .await;
        assert_eq!(status, 0);

        // The complete event must echo the destination context the caller asked for.
        let echoed_target = events
            .iter()
            .find_map(|e| match e {
                CopyCaptured::Complete {
                    id,
                    error_code,
                    target_context,
                    ..
                } if *id == 11 => Some((*error_code, *target_context)),
                _ => None,
            })
            .expect("COPY_ITEM_COMPLETE for id=11 missing");
        assert_eq!(echoed_target.0, lore_revision::event::LoreErrorCode::None);
        assert_eq!(echoed_target.1, target_context);

        // Read against the destination tuple — must return the source's payload byte-for-byte.
        let (get_status, dst_events) = get_items_capture(
            handle,
            vec![lore::storage::get::LoreStorageGetItem {
                id: 1,
                partition,
                address: destination_address,
                streaming: 0,
                local_cache: 0,
            }],
        )
        .await;
        assert_eq!(get_status, 0);
        let bytes_at_target = dst_events
            .iter()
            .find_map(|e| match e {
                GetCaptured::Data { bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .expect("DATA missing for destination tuple");
        assert_eq!(bytes_at_target, payload);

        // Source tuple must still be readable independently — copy created a new entry, not a
        // move.
        let (src_status, src_events) = get_items_capture(
            handle,
            vec![lore::storage::get::LoreStorageGetItem {
                id: 2,
                partition,
                address: source_address,
                streaming: 0,
                local_cache: 0,
            }],
        )
        .await;
        assert_eq!(src_status, 0);
        let bytes_at_source = src_events
            .iter()
            .find_map(|e| match e {
                GetCaptured::Data { bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .expect("DATA missing for source tuple");
        assert_eq!(bytes_at_source, payload);
    }

    #[tokio::test]
    async fn copy_batch_with_shared_source_partition_all_items_succeed() {
        // Batch authz aggregation: N items targeting the same source partition must succeed
        // — exercising the call-level code path that authorizes each unique source partition
        // exactly once. The strict "1 wire session_start per unique source" assertion needs
        // server-side instrumentation and is deferred; here we verify correctness of the
        // batched code path end-to-end.
        use lore_base::types::Context;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        const N: u64 = 6;
        let source_partition = Partition::from([0xE1; 16]);
        let context = Context::from([0xE2; 16]);

        let mut items = Vec::with_capacity(N as usize);
        for i in 0..N {
            let payload = format!("batch-shared-source #{i}").into_bytes();
            let address = put_once(handle, source_partition, context, &payload).await;
            // Each item retags the destination with a fresh `target_context` so the destination
            // tuples are all distinct (the API rejects identical destination tuples).
            let target_context = Context::from([0xF0 | (i as u8); 16]);
            items.push(lore::storage::copy::LoreStorageCopyItem {
                id: 200 + i,
                source_partition,
                target_partition: source_partition,
                source_address: address,
                target_context,
            });
        }

        let (status, events) = copy_items(handle, items).await;
        assert_eq!(
            status, 0,
            "batch of N copies sharing a source partition must succeed"
        );

        // Every per-item event must report success.
        let mut succeeded: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for event in &events {
            if let CopyCaptured::Complete { id, error_code, .. } = event {
                assert_eq!(
                    *error_code,
                    lore_revision::event::LoreErrorCode::None,
                    "item {id} must succeed",
                );
                succeeded.insert(*id);
            }
        }
        for i in 0..N {
            assert!(
                succeeded.contains(&(200 + i)),
                "missing COPY_ITEM_COMPLETE for item {i}",
            );
        }
    }

    #[tokio::test]
    async fn copy_same_partition_same_context_rejects_invalid_args() {
        // The destination tuple is identical to the source — there is nothing to copy. The API
        // rejects this up front rather than silently no-op'ing, matching the documented contract.
        use lore_base::types::Context;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let payload = b"identical-tuple copy".to_vec();
        let partition = Partition::from([0x44; 16]);
        let context = Context::from([0xD1; 16]);
        let address = put_once(handle, partition, context, &payload).await;

        let (status, events) = copy_items(
            handle,
            vec![lore::storage::copy::LoreStorageCopyItem {
                id: 12,
                source_partition: partition,
                target_partition: partition,
                source_address: address,
                target_context: address.context,
            }],
        )
        .await;
        assert_ne!(status, 0);
        let complete = events
            .iter()
            .find_map(|e| match e {
                CopyCaptured::Complete { id, error_code, .. } => Some((*id, *error_code)),
                _ => None,
            })
            .expect("COPY_ITEM_COMPLETE missing");
        assert_eq!(
            complete,
            (12, lore_revision::event::LoreErrorCode::InvalidArguments),
        );
    }

    #[tokio::test]
    async fn obliterate_zero_partition_rejects_invalid_args() {
        use lore_base::types::Address;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let (status, events) = obliterate_items(
            handle,
            vec![lore::storage::obliterate::LoreStorageObliterateItem {
                id: 5,
                partition: Partition::default(),
                address: Address::default(),
            }],
        )
        .await;
        assert_ne!(status, 0);
        let complete = events
            .iter()
            .find_map(|e| match e {
                ObliterateCaptured::Complete { id, error_code, .. } => Some((*id, *error_code)),
                _ => None,
            })
            .expect("OBLITERATE_ITEM_COMPLETE missing");
        assert_eq!(
            complete,
            (5, lore_revision::event::LoreErrorCode::InvalidArguments),
        );
    }

    #[tokio::test]
    async fn put_oversized_payload_round_trips_via_multi_fragment() {
        // Multi-fragment payload: payload exceeding FRAGMENT_SIZE_THRESHOLD yields a
        // single top-level address; get returns byte-identical content.
        use lore_base::types::Context;
        use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let len = 4 * FRAGMENT_SIZE_THRESHOLD;
        let payload: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_mul(31)).collect();
        let partition = Partition::from([0x10u8; 16]);
        let context = Context::from([0x20u8; 16]);
        let address = put_once(handle, partition, context, &payload).await;

        let (status, events) = get_items_capture(
            handle,
            vec![lore::storage::get::LoreStorageGetItem {
                id: 1,
                partition,
                address,
                streaming: 0,
                local_cache: 0,
            }],
        )
        .await;
        assert_eq!(status, 0);
        let bytes = events
            .iter()
            .find_map(|e| match e {
                GetCaptured::Data { bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .expect("DATA missing");
        assert_eq!(bytes.len(), payload.len());
        assert_eq!(bytes, payload);
    }

    #[tokio::test]
    async fn get_streaming_emits_one_data_per_leaf_with_offsets() {
        // Streaming mode: streaming=1 emits one GET_DATA per leaf
        // fragment carrying an offset; offsets do not overlap, sum of
        // data.len equals size_content, and every byte in
        // [0, size_content) is covered exactly once.
        use lore_base::types::Context;
        use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        // Force multi-fragment storage with small leaves so the
        // streaming path emits multiple GET_DATA events.
        let len = 4 * FRAGMENT_SIZE_THRESHOLD;
        let payload: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_mul(13)).collect();
        let partition = Partition::from([0x50u8; 16]);
        let context = Context::from([0x60u8; 16]);

        let item = lore::storage::put::LoreStoragePutItem {
            id: 1,
            partition,
            context,
            data: lore_revision::event::LoreBytes {
                ptr: payload.as_ptr().cast(),
                len: payload.len(),
            },
            remote_write: 0,
            local_cache: 0,
            fixed_size_chunk: 64 * 1024,
        };
        let (put_status, put_completes) = put_items(handle, vec![item]).await;
        drop(payload);
        assert_eq!(put_status, 0);
        let address = put_completes.iter().find(|c| c.id == 1).unwrap().address;

        let payload: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_mul(13)).collect();

        let (status, events) = get_items_capture(
            handle,
            vec![lore::storage::get::LoreStorageGetItem {
                id: 9,
                partition,
                address,
                streaming: 1,
                local_cache: 0,
            }],
        )
        .await;
        assert_eq!(status, 0);

        // HEADER carries the authoritative size_content.
        let header_size = events
            .iter()
            .find_map(|e| match e {
                GetCaptured::Header { size_content, .. } => Some(*size_content),
                _ => None,
            })
            .expect("HEADER missing");
        assert_eq!(header_size, payload.len() as u64);

        // Collect (offset, bytes) pairs and verify fragment invariants.
        let mut chunks: Vec<(u64, Vec<u8>)> = events
            .iter()
            .filter_map(|e| match e {
                GetCaptured::Data { offset, bytes, .. } => Some((*offset, bytes.clone())),
                _ => None,
            })
            .collect();
        assert!(
            chunks.len() >= 2,
            "streaming should emit multiple DATA events for a multi-fragment payload, got {}",
            chunks.len(),
        );

        // Reassemble using offsets — order-independent.
        chunks.sort_by_key(|(offset, _)| *offset);
        let mut reassembled = Vec::with_capacity(payload.len());
        let mut expected_offset: u64 = 0;
        for (offset, bytes) in &chunks {
            assert_eq!(
                *offset, expected_offset,
                "fragment offsets must cover [0, size_content) exactly once",
            );
            reassembled.extend_from_slice(bytes);
            expected_offset += bytes.len() as u64;
        }
        assert_eq!(
            expected_offset,
            payload.len() as u64,
            "sum of data.len must equal size_content",
        );
        assert_eq!(reassembled, payload, "reassembled bytes must match input");
    }

    /// Create a `tempfile::NamedTempFile` populated with `contents`. The returned guard
    /// auto-cleans the file on Drop (success or panic). Callers hold the guard for the test
    /// scope and pass `guard.path()` to the API.
    fn write_temp_file(contents: &[u8], tag: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::Builder::new()
            .prefix(&format!("lore-put-file-{tag}-"))
            .tempfile()
            .expect("create tempfile");
        std::io::Write::write_all(&mut file, contents).expect("write tempfile");
        file
    }

    /// Create a path inside a fresh `TempDir` for tests that need a destination file location
    /// (e.g. `get_file` target). The directory cleans up its entire tree on Drop, so the
    /// resulting file — whether the test creates it, the API creates it, or it never exists —
    /// is removed on both success and panic paths.
    fn temp_file_path(tag: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("lore-storage-{tag}-"))
            .tempdir()
            .expect("create tempdir");
        let path = dir.path().join("target");
        (dir, path)
    }

    async fn put_file_items(
        handle: lore::storage::handle::LoreStore,
        items: Vec<lore::storage::put_file::LoreStoragePutFileItem>,
    ) -> (
        i32,
        Vec<lore_revision::store::event::LoreStoragePutItemCompleteEventData>,
    ) {
        let sink: Arc<Mutex<Vec<LoreEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_for_cb = sink.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            sink_for_cb.lock().unwrap().push(event.clone());
        }));
        let status = lore::storage::put_file::put_file(
            globals(),
            lore::storage::put_file::LoreStoragePutFileArgs {
                handle,
                items: lore_revision::interface::LoreArray::from_vec(items),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        let completes = events
            .iter()
            .filter_map(|e| match e {
                LoreEvent::StoragePutItemComplete(d) => Some(*d),
                _ => None,
            })
            .collect();
        (status, completes)
    }

    #[tokio::test]
    async fn put_file_round_trips_via_get() {
        // put_file round-trip: file content lands in the store at the same
        // address `write_from_file` would produce; get returns the
        // original bytes.
        use lore_base::types::Context;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let payload = b"file contents for put_file".to_vec();
        let file = write_temp_file(&payload, "round-trip");
        let partition = Partition::from([0x70u8; 16]);
        let context = Context::from([0x80u8; 16]);

        let item = lore::storage::put_file::LoreStoragePutFileItem {
            id: 1,
            partition,
            context,
            path: LoreString::from(file.path().display().to_string().as_str()),
            remote_write: 0,
            local_cache: 0,
            fixed_size_chunk: 0,
        };
        let (status, completes) = put_file_items(handle, vec![item]).await;
        assert_eq!(status, 0);
        let address = completes.iter().find(|c| c.id == 1).unwrap().address;
        assert_eq!(
            completes[0].error_code,
            lore_revision::event::LoreErrorCode::None
        );

        let (g_status, events) = get_items_capture(
            handle,
            vec![lore::storage::get::LoreStorageGetItem {
                id: 2,
                partition,
                address,
                streaming: 0,
                local_cache: 0,
            }],
        )
        .await;
        assert_eq!(g_status, 0);
        let bytes = events
            .iter()
            .find_map(|e| match e {
                GetCaptured::Data { bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .expect("DATA missing");
        assert_eq!(bytes, payload);
    }

    #[tokio::test]
    async fn put_file_empty_file_short_circuits_to_zero_hash() {
        // Zero-byte file: zero-byte file → (Hash::default(), context).
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let file = write_temp_file(&[], "empty");
        let partition = Partition::from([0x91u8; 16]);
        let context = Context::from([0x92u8; 16]);

        let (status, completes) = put_file_items(
            handle,
            vec![lore::storage::put_file::LoreStoragePutFileItem {
                id: 9,
                partition,
                context,
                path: LoreString::from(file.path().display().to_string().as_str()),
                remote_write: 0,
                local_cache: 0,
                fixed_size_chunk: 0,
            }],
        )
        .await;
        assert_eq!(status, 0);
        let complete = completes.iter().find(|c| c.id == 9).unwrap();
        assert_eq!(
            complete.error_code,
            lore_revision::event::LoreErrorCode::None
        );
        assert_eq!(complete.address.hash, Hash::default());
        assert_eq!(complete.address.context, context);
    }

    #[tokio::test]
    async fn put_file_missing_file_rejects_invalid_args() {
        // A path that doesn't resolve to a regular file is caller-fixable input — surfaces
        // as `InvalidArguments`, not `Internal`.
        use lore_base::types::Context;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let (_guard, missing) = temp_file_path("put-file-missing");
        let (status, completes) = put_file_items(
            handle,
            vec![lore::storage::put_file::LoreStoragePutFileItem {
                id: 1,
                partition: Partition::from([0xA1u8; 16]),
                context: Context::from([0xA2u8; 16]),
                path: LoreString::from(missing.display().to_string().as_str()),
                remote_write: 0,
                local_cache: 0,
                fixed_size_chunk: 0,
            }],
        )
        .await;
        assert_ne!(status, 0);
        let complete = completes.iter().find(|c| c.id == 1).unwrap();
        assert_eq!(
            complete.error_code,
            lore_revision::event::LoreErrorCode::InvalidArguments
        );
    }

    #[tokio::test]
    async fn put_file_zero_partition_rejects_invalid_args() {
        use lore_base::types::Context;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let file = write_temp_file(b"data", "zero-partition");
        let (status, completes) = put_file_items(
            handle,
            vec![lore::storage::put_file::LoreStoragePutFileItem {
                id: 5,
                partition: Partition::default(),
                context: Context::default(),
                path: LoreString::from(file.path().display().to_string().as_str()),
                remote_write: 0,
                local_cache: 0,
                fixed_size_chunk: 0,
            }],
        )
        .await;
        assert_ne!(status, 0);
        let complete = completes.iter().find(|c| c.id == 5).unwrap();
        assert_eq!(
            complete.error_code,
            lore_revision::event::LoreErrorCode::InvalidArguments
        );
    }

    #[tokio::test]
    async fn put_file_oversized_round_trips() {
        // Oversized file: a file larger than the fragment threshold yields a
        // single top-level address; round-trip via get returns the
        // original bytes.
        use lore_base::types::Context;
        use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let len = 4 * FRAGMENT_SIZE_THRESHOLD;
        let payload: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_add(11)).collect();
        let file = write_temp_file(&payload, "oversized");
        let partition = Partition::from([0xB1u8; 16]);
        let context = Context::from([0xB2u8; 16]);

        let (status, completes) = put_file_items(
            handle,
            vec![lore::storage::put_file::LoreStoragePutFileItem {
                id: 1,
                partition,
                context,
                path: LoreString::from(file.path().display().to_string().as_str()),
                remote_write: 0,
                local_cache: 0,
                fixed_size_chunk: 64 * 1024,
            }],
        )
        .await;
        assert_eq!(status, 0);
        let address = completes.iter().find(|c| c.id == 1).unwrap().address;

        let (g_status, events) = get_items_capture(
            handle,
            vec![lore::storage::get::LoreStorageGetItem {
                id: 2,
                partition,
                address,
                streaming: 0,
                local_cache: 0,
            }],
        )
        .await;
        assert_eq!(g_status, 0);
        let bytes = events
            .iter()
            .find_map(|e| match e {
                GetCaptured::Data { bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .expect("DATA missing");
        assert_eq!(bytes, payload);
    }

    async fn get_file_items(
        handle: lore::storage::handle::LoreStore,
        items: Vec<lore::storage::get_file::LoreStorageGetFileItem>,
    ) -> (i32, Vec<GetCaptured>) {
        let (sink, callback) = make_get_sink();
        let status = lore::storage::get_file::get_file(
            globals(),
            lore::storage::get_file::LoreStorageGetFileArgs {
                handle,
                items: lore_revision::interface::LoreArray::from_vec(items),
            },
            callback,
        )
        .await;
        let events = sink.lock().unwrap().clone();
        (status, events)
    }

    #[tokio::test]
    async fn get_file_writes_payload_to_disk() {
        // get_file behavior: get_file writes the reassembled bytes and
        // emits only GET_ITEM_COMPLETE — no HEADER, no DATA.
        use lore_base::types::Context;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let payload = b"file output bytes".to_vec();
        let partition = Partition::from([0xC1u8; 16]);
        let context = Context::from([0xC2u8; 16]);
        let address = put_once(handle, partition, context, &payload).await;

        let (_target_guard, target) = temp_file_path("get-file-ok");
        let (status, events) = get_file_items(
            handle,
            vec![lore::storage::get_file::LoreStorageGetFileItem {
                id: 1,
                partition,
                address,
                path: LoreString::from(target.display().to_string().as_str()),
                local_cache: 0,
            }],
        )
        .await;
        assert_eq!(status, 0);

        let on_disk = std::fs::read(&target).unwrap();
        assert_eq!(on_disk, payload);

        // No HEADER / DATA — only the terminal event.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, GetCaptured::Header { .. } | GetCaptured::Data { .. })),
            "get_file must not emit HEADER or DATA, got {events:?}",
        );
        let complete = events.iter().find_map(|e| match e {
            GetCaptured::ItemComplete { id, error_code, .. } => Some((*id, *error_code)),
            _ => None,
        });
        assert_eq!(
            complete,
            Some((1, lore_revision::event::LoreErrorCode::None)),
        );
    }

    #[tokio::test]
    async fn get_file_zero_hash_creates_empty_target() {
        // Zero-hash address: zero-hash address creates/truncates target to zero
        // bytes; complete event reports None.
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let (_target_guard, target) = temp_file_path("get-file-zero");
        // Pre-fill the file so we can verify truncation.
        std::fs::write(&target, b"existing junk").unwrap();

        let zero_addr = Address {
            hash: Hash::default(),
            context: Context::from([0xCDu8; 16]),
        };
        let (status, _events) = get_file_items(
            handle,
            vec![lore::storage::get_file::LoreStorageGetFileItem {
                id: 1,
                partition: Partition::from([0xEFu8; 16]),
                address: zero_addr,
                path: LoreString::from(target.display().to_string().as_str()),
                local_cache: 0,
            }],
        )
        .await;
        assert_eq!(status, 0);
        let on_disk = std::fs::read(&target).unwrap();
        assert!(on_disk.is_empty(), "target file must be truncated to zero");
    }

    #[tokio::test]
    async fn get_file_missing_address_returns_address_not_found() {
        // Content not reachable: content not reachable → ADDRESS_NOT_FOUND.
        use lore_base::types::Address;
        use lore_base::types::Context;
        use lore_base::types::Hash;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let (_target_guard, target) = temp_file_path("get-file-miss");
        let absent = Address {
            hash: Hash::from([0xAFu8; 32]),
            context: Context::from([0xBEu8; 16]),
        };
        let (status, events) = get_file_items(
            handle,
            vec![lore::storage::get_file::LoreStorageGetFileItem {
                id: 7,
                partition: Partition::from([0xC1u8; 16]),
                address: absent,
                path: LoreString::from(target.display().to_string().as_str()),
                local_cache: 0,
            }],
        )
        .await;
        assert_eq!(status, -1);
        let complete = events.iter().find_map(|e| match e {
            GetCaptured::ItemComplete { id, error_code, .. } => Some((*id, *error_code)),
            _ => None,
        });
        assert_eq!(
            complete,
            Some((7, lore_revision::event::LoreErrorCode::AddressNotFound)),
        );
    }

    #[tokio::test]
    async fn get_file_zero_partition_rejects_invalid_args() {
        use lore_base::types::Address;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let (_target_guard, target) = temp_file_path("get-file-zerop");
        let (status, events) = get_file_items(
            handle,
            vec![lore::storage::get_file::LoreStorageGetFileItem {
                id: 5,
                partition: Partition::default(),
                address: Address::default(),
                path: LoreString::from(target.display().to_string().as_str()),
                local_cache: 0,
            }],
        )
        .await;
        assert_ne!(status, 0);
        let complete = events.iter().find_map(|e| match e {
            GetCaptured::ItemComplete { id, error_code, .. } => Some((*id, *error_code)),
            _ => None,
        });
        assert_eq!(
            complete,
            Some((5, lore_revision::event::LoreErrorCode::InvalidArguments)),
        );
    }

    #[tokio::test]
    async fn put_with_fixed_size_chunk_round_trips_intact() {
        // `fixed_size_chunk` controls leaf fragment sizing for
        // multi-fragment writes; the round-trip must still return the
        // original payload byte-for-byte.
        use lore_base::types::Context;
        use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
        use lore_base::types::Partition;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let len = 4 * FRAGMENT_SIZE_THRESHOLD;
        let payload: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_add(7)).collect();
        let partition = Partition::from([0x30u8; 16]);
        let context = Context::from([0x40u8; 16]);

        let item = lore::storage::put::LoreStoragePutItem {
            id: 1,
            partition,
            context,
            data: lore_revision::event::LoreBytes {
                ptr: payload.as_ptr().cast(),
                len: payload.len(),
            },
            remote_write: 0,
            local_cache: 0,
            fixed_size_chunk: 64 * 1024,
        };
        let (put_status, put_completes) = put_items(handle, vec![item]).await;
        drop(payload);
        assert_eq!(put_status, 0);
        let address = put_completes
            .iter()
            .find(|c| c.id == 1)
            .map(|c| c.address)
            .unwrap();

        let payload: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_add(7)).collect();
        let (status, events) = get_items_capture(
            handle,
            vec![lore::storage::get::LoreStorageGetItem {
                id: 2,
                partition,
                address,
                streaming: 0,
                local_cache: 0,
            }],
        )
        .await;
        assert_eq!(status, 0);
        let bytes = events
            .iter()
            .find_map(|e| match e {
                GetCaptured::Data { bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .expect("DATA missing");
        assert_eq!(bytes, payload);
    }

    // ---------------------------------------------------------------------------------------
    // Concurrent calls against the same handle.
    //
    // The storage API contract says concurrent calls from multiple threads against the same
    // handle produce correct results without external synchronization. These tests exercise
    // that contract by spawning N tokio tasks that each issue a full API call; per-op tests
    // verify every call lands with a correct outcome, the mixed-op test verifies different
    // ops can run interleaved, and the timing test proves the dispatch is actually parallel
    // rather than silently serialized somewhere along the path.
    // ---------------------------------------------------------------------------------------

    /// N parallel `put` calls against the same handle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn n_concurrent_puts_against_same_handle_all_succeed() {
        use lore_base::types::Context;
        use lore_base::types::Partition;
        use tokio::task::JoinSet;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        const N: usize = 16;
        let mut tasks: JoinSet<(usize, i32, lore_base::types::Address)> = JoinSet::new();
        for i in 0..N {
            lore_spawn!(tasks, async move {
                let payload = format!("concurrent put #{i} payload bytes").into_bytes();
                let item = lore::storage::put::LoreStoragePutItem {
                    id: i as u64,
                    partition: Partition::from([0x10 + i as u8; 16]),
                    context: Context::from([0x20 + i as u8; 16]),
                    data: lore_revision::event::LoreBytes {
                        ptr: payload.as_ptr().cast(),
                        len: payload.len(),
                    },
                    remote_write: 0,
                    local_cache: 0,
                    fixed_size_chunk: 0,
                };
                let (status, completes) = put_items(handle, vec![item]).await;
                let address = completes
                    .iter()
                    .find(|c| c.id == i as u64)
                    .map(|c| c.address)
                    .expect("PUT_ITEM_COMPLETE for this task");
                drop(payload); // payload lifetime is the task; drop explicit so capture is obvious
                (i, status, address)
            });
        }

        let mut results = Vec::with_capacity(N);
        while let Some(result) = tasks.join_next().await {
            results.push(result.expect("task panicked"));
        }
        assert_eq!(results.len(), N);
        for (i, status, address) in &results {
            assert_eq!(*status, 0, "put #{i} should succeed");
            assert_ne!(
                address.hash,
                lore_base::types::Hash::default(),
                "put #{i} must produce a non-zero hash",
            );
        }
        // Each task wrote a distinct payload; addresses must all differ.
        let mut hashes: Vec<_> = results.iter().map(|(_, _, a)| a.hash).collect();
        hashes.sort();
        hashes.dedup();
        assert_eq!(hashes.len(), N, "every put address should be unique");
    }

    /// N parallel `get` calls against the same handle, fetching N pre-populated payloads.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn n_concurrent_gets_against_same_handle_all_succeed() {
        use lore_base::types::Context;
        use lore_base::types::Partition;
        use tokio::task::JoinSet;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        const N: usize = 16;
        let mut seeded = Vec::with_capacity(N);
        for i in 0..N {
            let payload = format!("seeded payload #{i}").into_bytes();
            let partition = Partition::from([0x40 + i as u8; 16]);
            let context = Context::from([0x50 + i as u8; 16]);
            let address = put_once(handle, partition, context, &payload).await;
            seeded.push((i, partition, address, payload));
        }

        let mut tasks: JoinSet<(usize, i32, Vec<u8>)> = JoinSet::new();
        for (i, partition, address, expected) in seeded {
            lore_spawn!(tasks, async move {
                let (status, events) = get_items_capture(
                    handle,
                    vec![lore::storage::get::LoreStorageGetItem {
                        id: i as u64,
                        partition,
                        address,
                        streaming: 0,
                        local_cache: 0,
                    }],
                )
                .await;
                let bytes = events
                    .iter()
                    .find_map(|e| match e {
                        GetCaptured::Data { bytes, .. } => Some(bytes.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let _ = expected; // returned in the result tuple instead
                (i, status, bytes)
            });
        }

        let mut by_id: std::collections::HashMap<usize, (i32, Vec<u8>)> = Default::default();
        while let Some(result) = tasks.join_next().await {
            let (i, status, bytes) = result.expect("task panicked");
            by_id.insert(i, (status, bytes));
        }
        assert_eq!(by_id.len(), N);
        for i in 0..N {
            let (status, bytes) = by_id.get(&i).expect("missing result");
            assert_eq!(*status, 0, "get #{i} should succeed");
            let expected = format!("seeded payload #{i}").into_bytes();
            assert_eq!(*bytes, expected, "get #{i} bytes mismatch");
        }
    }

    /// N parallel `get_metadata` calls against the same handle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn n_concurrent_get_metadata_calls_against_same_handle_all_succeed() {
        use lore_base::types::Context;
        use lore_base::types::Partition;
        use tokio::task::JoinSet;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        const N: usize = 16;
        let mut seeded = Vec::with_capacity(N);
        for i in 0..N {
            let payload = format!("metadata-target payload #{i}").into_bytes();
            let partition = Partition::from([0x60 + i as u8; 16]);
            let context = Context::from([0x70 + i as u8; 16]);
            let address = put_once(handle, partition, context, &payload).await;
            seeded.push((i, partition, address, payload.len()));
        }

        let mut tasks: JoinSet<(usize, i32, u64)> = JoinSet::new();
        for (i, partition, address, expected_size) in seeded {
            lore_spawn!(tasks, async move {
                let (status, events) = get_metadata_items(
                    handle,
                    vec![lore::storage::get_metadata::LoreStorageGetMetadataItem {
                        id: i as u64,
                        partition,
                        address,
                    }],
                )
                .await;
                let observed_size = events
                    .iter()
                    .find_map(|e| match e {
                        GetMetadataCaptured::Complete { fragment, .. } => {
                            Some(fragment.size_content)
                        }
                        _ => None,
                    })
                    .unwrap_or(0);
                let _ = expected_size;
                (i, status, observed_size)
            });
        }

        let mut by_id: std::collections::HashMap<usize, (i32, u64)> = Default::default();
        while let Some(result) = tasks.join_next().await {
            let (i, status, size) = result.expect("task panicked");
            by_id.insert(i, (status, size));
        }
        assert_eq!(by_id.len(), N);
        for i in 0..N {
            let (status, size) = by_id.get(&i).expect("missing result");
            assert_eq!(*status, 0, "get_metadata #{i} should succeed");
            let expected = format!("metadata-target payload #{i}").len() as u64;
            assert_eq!(*size, expected, "get_metadata #{i} size mismatch");
        }
    }

    /// N parallel `obliterate` calls against the same handle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn n_concurrent_obliterates_against_same_handle_all_succeed() {
        use lore_base::types::Context;
        use lore_base::types::Partition;
        use tokio::task::JoinSet;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        const N: usize = 16;
        let mut seeded = Vec::with_capacity(N);
        for i in 0..N {
            let payload = format!("obliterate-target #{i}").into_bytes();
            let partition = Partition::from([0x80 + i as u8; 16]);
            let context = Context::from([0x90 + i as u8; 16]);
            let address = put_once(handle, partition, context, &payload).await;
            seeded.push((i, partition, address));
        }

        let mut tasks: JoinSet<(usize, i32, u8, u8, u8)> = JoinSet::new();
        for (i, partition, address) in seeded {
            lore_spawn!(tasks, async move {
                let (status, events) = obliterate_items(
                    handle,
                    vec![lore::storage::obliterate::LoreStorageObliterateItem {
                        id: i as u64,
                        partition,
                        address,
                    }],
                )
                .await;
                let (local, remote, remote_skipped) = events
                    .iter()
                    .find_map(|e| match e {
                        ObliterateCaptured::Complete {
                            local_success,
                            remote_success,
                            remote_skipped,
                            ..
                        } => Some((*local_success, *remote_success, *remote_skipped)),
                        _ => None,
                    })
                    .unwrap_or((0, 0, 0));
                (i, status, local, remote, remote_skipped)
            });
        }

        let mut by_id: std::collections::HashMap<usize, (i32, u8, u8, u8)> = Default::default();
        while let Some(result) = tasks.join_next().await {
            let (i, status, local, remote, remote_skipped) = result.expect("task panicked");
            by_id.insert(i, (status, local, remote, remote_skipped));
        }
        assert_eq!(by_id.len(), N);
        for i in 0..N {
            let (status, local, remote, remote_skipped) = by_id.get(&i).expect("missing result");
            assert_eq!(*status, 0, "obliterate #{i} should succeed");
            assert_eq!(*local, 1, "obliterate #{i} local_success");
            assert_eq!(
                *remote, 0,
                "obliterate #{i} remote_success (no-remote skipped, not success)"
            );
            assert_eq!(
                *remote_skipped, 1,
                "obliterate #{i} remote_skipped (no remote configured)"
            );
        }
    }

    /// N parallel `copy` calls against the same handle, each across a unique partition pair.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn n_concurrent_copies_against_same_handle_all_succeed() {
        use lore_base::types::Context;
        use lore_base::types::Partition;
        use tokio::task::JoinSet;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        const N: usize = 8;
        let mut seeded = Vec::with_capacity(N);
        for i in 0..N {
            let payload = format!("copy-source #{i} bytes").into_bytes();
            let source_partition = Partition::from([0xA0 + i as u8; 16]);
            let target_partition = Partition::from([0xB0 + i as u8; 16]);
            let context = Context::from([0xC0 + i as u8; 16]);
            let address = put_once(handle, source_partition, context, &payload).await;
            seeded.push((i, source_partition, target_partition, address));
        }

        let mut tasks: JoinSet<(usize, i32)> = JoinSet::new();
        for (i, source_partition, target_partition, source_address) in seeded {
            lore_spawn!(tasks, async move {
                let (status, _events) = copy_items(
                    handle,
                    vec![lore::storage::copy::LoreStorageCopyItem {
                        id: i as u64,
                        source_partition,
                        source_address,
                        target_partition,
                        target_context: source_address.context,
                    }],
                )
                .await;
                (i, status)
            });
        }

        let mut by_id: std::collections::HashMap<usize, i32> = Default::default();
        while let Some(result) = tasks.join_next().await {
            let (i, status) = result.expect("task panicked");
            by_id.insert(i, status);
        }
        assert_eq!(by_id.len(), N);
        for i in 0..N {
            assert_eq!(*by_id.get(&i).unwrap(), 0, "copy #{i} should succeed",);
        }
    }

    /// Mixed-op concurrency: put / get / `get_metadata` / copy / obliterate calls all in flight
    /// at the same time on one handle. Each task targets independent partitions so the ops
    /// don't race for the same key — the goal is to verify the dispatch handles multiple
    /// op-kinds simultaneously without deadlock or cross-talk.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mixed_concurrent_ops_against_same_handle_complete_independently() {
        use lore_base::types::Context;
        use lore_base::types::Partition;
        use tokio::task::JoinSet;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        // Pre-populate three partitions with one address each so get / get_metadata /
        // obliterate / copy each have independent state to operate on.
        let payload_get = b"mixed-test get target".to_vec();
        let payload_meta = b"mixed-test meta target".to_vec();
        let payload_obl = b"mixed-test obliterate target".to_vec();
        let payload_cp = b"mixed-test copy source".to_vec();

        let part_get = Partition::from([0xD1; 16]);
        let part_meta = Partition::from([0xD2; 16]);
        let part_obl = Partition::from([0xD3; 16]);
        let part_cp_src = Partition::from([0xD4; 16]);
        let part_cp_tgt = Partition::from([0xD5; 16]);
        let part_put = Partition::from([0xD6; 16]);
        let ctx = Context::from([0xE0; 16]);

        let addr_get = put_once(handle, part_get, ctx, &payload_get).await;
        let addr_meta = put_once(handle, part_meta, ctx, &payload_meta).await;
        let addr_obl = put_once(handle, part_obl, ctx, &payload_obl).await;
        let addr_cp = put_once(handle, part_cp_src, ctx, &payload_cp).await;

        let mut tasks: JoinSet<(&'static str, bool)> = JoinSet::new();

        // put — adds a new address.
        lore_spawn!(tasks, async move {
            let payload = b"mixed-test new put".to_vec();
            let item = lore::storage::put::LoreStoragePutItem {
                id: 1,
                partition: part_put,
                context: ctx,
                data: lore_revision::event::LoreBytes {
                    ptr: payload.as_ptr().cast(),
                    len: payload.len(),
                },
                remote_write: 0,
                local_cache: 0,
                fixed_size_chunk: 0,
            };
            let (status, completes) = put_items(handle, vec![item]).await;
            drop(payload);
            (
                "put",
                status == 0
                    && completes
                        .iter()
                        .all(|c| c.error_code == lore_revision::event::LoreErrorCode::None),
            )
        });

        // get — fetches the pre-seeded address.
        lore_spawn!(tasks, async move {
            let (status, _events) = get_items_capture(
                handle,
                vec![lore::storage::get::LoreStorageGetItem {
                    id: 2,
                    partition: part_get,
                    address: addr_get,
                    streaming: 0,
                    local_cache: 0,
                }],
            )
            .await;
            ("get", status == 0)
        });

        // get_metadata — fetches metadata for the pre-seeded address.
        lore_spawn!(tasks, async move {
            let (status, _events) = get_metadata_items(
                handle,
                vec![lore::storage::get_metadata::LoreStorageGetMetadataItem {
                    id: 3,
                    partition: part_meta,
                    address: addr_meta,
                }],
            )
            .await;
            ("get_metadata", status == 0)
        });

        // copy — moves the pre-seeded source to a new partition.
        lore_spawn!(tasks, async move {
            let (status, _events) = copy_items(
                handle,
                vec![lore::storage::copy::LoreStorageCopyItem {
                    id: 4,
                    source_partition: part_cp_src,
                    source_address: addr_cp,
                    target_partition: part_cp_tgt,
                    target_context: addr_cp.context,
                }],
            )
            .await;
            ("copy", status == 0)
        });

        // obliterate — removes the pre-seeded address.
        lore_spawn!(tasks, async move {
            let (status, _events) = obliterate_items(
                handle,
                vec![lore::storage::obliterate::LoreStorageObliterateItem {
                    id: 5,
                    partition: part_obl,
                    address: addr_obl,
                }],
            )
            .await;
            ("obliterate", status == 0)
        });

        let mut outcomes: std::collections::HashMap<&'static str, bool> = Default::default();
        while let Some(result) = tasks.join_next().await {
            let (op, ok) = result.expect("task panicked");
            outcomes.insert(op, ok);
        }
        assert_eq!(outcomes.len(), 5);
        for op in ["put", "get", "get_metadata", "copy", "obliterate"] {
            assert_eq!(outcomes.get(op), Some(&true), "{op} should succeed");
        }
    }

    /// Parallelism proof via timing: N concurrent multi-fragment puts must finish
    /// substantially faster than the same N puts run sequentially. The wall-clock ratio is
    /// measured on a multi-thread runtime (4 worker threads) using payloads large enough
    /// that scheduler noise and per-op fixed costs are dwarfed by the work.
    ///
    /// Robustness: the measurement is repeated `SAMPLES` times and the BEST observed ratio
    /// is checked against the threshold. A fully-serialized dispatcher fails every sample
    /// (no run can produce speedup); transient CI load that starves a single run gets
    /// absorbed. Each sample alternates parallel/sequential to keep system load symmetric.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_calls_observably_run_in_parallel_via_timing() {
        use std::time::Duration;
        use std::time::Instant;

        use lore_base::types::Context;
        use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
        use lore_base::types::Partition;
        use tokio::task::JoinSet;

        let (open_sink, open_cb) = make_sink();
        assert_eq!(open_in_memory(open_cb).await, 0);
        let id = take_opened(&open_sink.lock().unwrap()).unwrap();
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        // Each put is large enough that hashing + chunking dominates scheduler noise. With
        // 4 MiB and FRAGMENT_SIZE_THRESHOLD = ~256 KiB, each put fragments into ~16 leaves.
        let len = 4 * FRAGMENT_SIZE_THRESHOLD;
        const N: usize = 4;
        const SAMPLES: usize = 3;

        async fn sequential_run(
            handle: lore::storage::handle::LoreStore,
            sample: usize,
            len: usize,
        ) -> Duration {
            let payloads: Vec<Vec<u8>> = (0..N)
                .map(|i| {
                    let mix = (sample * 71 + i * 31) as u8;
                    (0..len)
                        .map(|j| (j as u8).wrapping_mul(mix.wrapping_add(7)))
                        .collect()
                })
                .collect();
            let start = Instant::now();
            for (i, payload) in payloads.iter().enumerate() {
                let item = lore::storage::put::LoreStoragePutItem {
                    id: (sample * 100 + i) as u64,
                    partition: Partition::from([0xF0 + sample as u8; 16]),
                    context: Context::from([0xE0 + (sample * N + i) as u8; 16]),
                    data: lore_revision::event::LoreBytes {
                        ptr: payload.as_ptr().cast(),
                        len: payload.len(),
                    },
                    remote_write: 0,
                    local_cache: 0,
                    fixed_size_chunk: 0,
                };
                let (status, _) = put_items(handle, vec![item]).await;
                assert_eq!(status, 0, "sequential put sample={sample} #{i}");
            }
            start.elapsed()
        }

        async fn parallel_run(
            handle: lore::storage::handle::LoreStore,
            sample: usize,
            len: usize,
        ) -> Duration {
            let payloads: Vec<Vec<u8>> = (0..N)
                .map(|i| {
                    let mix = (sample * 53 + i * 17) as u8;
                    (0..len)
                        .map(|j| (j as u8).wrapping_mul(mix.wrapping_add(11)))
                        .collect()
                })
                .collect();
            let start = Instant::now();
            let mut tasks: JoinSet<i32> = JoinSet::new();
            for (i, payload) in payloads.into_iter().enumerate() {
                lore_spawn!(tasks, async move {
                    let item = lore::storage::put::LoreStoragePutItem {
                        id: (sample * 1000 + 5000 + i) as u64,
                        partition: Partition::from([0xC0 + sample as u8; 16]),
                        context: Context::from([0xA0 + (sample * N + i) as u8; 16]),
                        data: lore_revision::event::LoreBytes {
                            ptr: payload.as_ptr().cast(),
                            len: payload.len(),
                        },
                        remote_write: 0,
                        local_cache: 0,
                        fixed_size_chunk: 0,
                    };
                    let (status, _completes) = put_items(handle, vec![item]).await;
                    drop(payload);
                    status
                });
            }
            while let Some(result) = tasks.join_next().await {
                assert_eq!(
                    result.expect("task panic"),
                    0,
                    "parallel put sample={sample}"
                );
            }
            start.elapsed()
        }

        // Take SAMPLES paired measurements; record the smallest parallel/sequential ratio
        // across all samples. The scheme alternates which mode runs first per sample so any
        // warm-cache or cool-down bias evens out.
        let mut best_ratio: Option<f64> = None;
        let mut samples_log: Vec<(Duration, Duration, f64)> = Vec::with_capacity(SAMPLES);
        for sample in 0..SAMPLES {
            let (seq_elapsed, par_elapsed) = if sample % 2 == 0 {
                let s = sequential_run(handle, sample, len).await;
                let p = parallel_run(handle, sample, len).await;
                (s, p)
            } else {
                let p = parallel_run(handle, sample, len).await;
                let s = sequential_run(handle, sample, len).await;
                (s, p)
            };
            let ratio = par_elapsed.as_secs_f64() / seq_elapsed.as_secs_f64();
            samples_log.push((seq_elapsed, par_elapsed, ratio));
            best_ratio = Some(best_ratio.map_or(ratio, |b: f64| b.min(ratio)));
        }
        let best_ratio = best_ratio.expect("at least one sample");

        // Threshold: best observed parallel/sequential ratio must be < 0.7 (≥1.43× speedup).
        // With 4 worker threads doing CPU-bound hashing on an N=4 batch, the actual speedup
        // is typically 2.5–3.5×. A fully-serialized dispatcher would produce ratios near
        // 1.0× across every sample, so the regression-catching property is preserved while
        // transient noise on a single run is absorbed.
        assert!(
            best_ratio < 0.7,
            "best parallel/sequential ratio across {SAMPLES} samples was {best_ratio:.3}, \
             expected < 0.7; per-sample (seq, par, ratio): {samples_log:?} — concurrent \
             calls appear to be serialized",
        );
    }

    /// An N-item `lore_storage_put` call must achieve throughput within a reasonable margin
    /// of N concurrent `write_content` calls — the API layer cannot impose significant
    /// per-call overhead beyond what the underlying storage primitives already pay for the
    /// same work.
    ///
    /// Comparison: a single `lore_storage_put` carrying N items (one internal `JoinSet`) vs
    /// N concurrent `write_content` calls dispatched by an outer `JoinSet`. Both sides
    /// execute against the same in-memory `ImmutableStore`. Each sample uses distinct
    /// payloads so dedup never short-circuits; partition + context bytes are derived from
    /// `(sample, item_index)` to keep work per item identical between sides.
    ///
    /// Memory bound: each sample opens a fresh in-memory store, runs both phases, closes
    /// the store. Stored bytes from one sample are released before the next begins. Peak
    /// per-sample memory ≈ `N * len * 4` (api payloads + direct payloads + their stored
    /// copies in the store).
    ///
    /// Robustness: SAMPLES paired runs with alternating ordering, take the ratio closest to
    /// 1.0 (`min(api_elapsed / direct_elapsed)`). Threshold is generous enough to absorb CI
    /// scheduler noise but tight enough to catch a real regression.
    ///
    /// Marked `#[ignore]` because the workload runs ~30 s in debug and only collapses to
    /// ~2 s in release. Run on demand:
    ///   `cargo test -p lore-integration-tests --release -- --ignored put_batch_api_within_overhead`
    #[ignore = "benchmark — run on demand with --release"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn put_batch_api_within_overhead_budget_of_direct_write_content() {
        use std::sync::Arc;
        use std::time::Duration;
        use std::time::Instant;

        use bytes::Bytes;
        use lore_base::types::Context;
        use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
        use lore_base::types::Partition;
        use lore_storage::options::WriteOptions;
        use lore_storage::write::write_content;
        use tokio::task::JoinSet;

        // Per-item payload large enough that hashing + chunking dominates per-call fixed
        // costs. With 2 MiB and FRAGMENT_SIZE_THRESHOLD = 256 KiB each item splits into
        // 8 leaf fragments — same on both sides. SAMPLES × N × len chosen so total wall
        // time lands near 2 s in release while peak per-sample memory stays ≤ ~256 MiB.
        const N: usize = 32;
        const SAMPLES: usize = 14;
        let len = 8 * FRAGMENT_SIZE_THRESHOLD;

        fn payload_for(sample: usize, item: usize, len: usize, salt: u8) -> Vec<u8> {
            let mix = (sample as u8)
                .wrapping_mul(31)
                .wrapping_add((item as u8).wrapping_mul(17))
                .wrapping_add(salt);
            (0..len)
                .map(|j| (j as u8).wrapping_mul(mix.wrapping_add(7)))
                .collect()
        }

        async fn run_api_path(
            handle: lore::storage::handle::LoreStore,
            sample: usize,
            payloads: &[Vec<u8>],
        ) -> Duration {
            let items: Vec<lore::storage::put::LoreStoragePutItem> = payloads
                .iter()
                .enumerate()
                .map(|(i, p)| lore::storage::put::LoreStoragePutItem {
                    id: (sample * 10_000 + i) as u64,
                    partition: Partition::from([0xB0u8.wrapping_add(sample as u8); 16]),
                    context: Context::from([(sample * N + i) as u8; 16]),
                    data: lore_revision::event::LoreBytes {
                        ptr: p.as_ptr().cast(),
                        len: p.len(),
                    },
                    remote_write: 0,
                    local_cache: 0,
                    fixed_size_chunk: 0,
                })
                .collect();
            let start = Instant::now();
            let (status, _completes) = put_items(handle, items).await;
            let elapsed = start.elapsed();
            assert_eq!(status, 0, "api put sample={sample}");
            elapsed
        }

        async fn run_direct_path(
            store: Arc<dyn lore_storage::ImmutableStore>,
            sample: usize,
            payloads: &[Bytes],
        ) -> Duration {
            let start = Instant::now();
            let mut tasks: JoinSet<()> = JoinSet::new();
            for (i, payload) in payloads.iter().enumerate() {
                let store = store.clone();
                let partition = Partition::from([0xE0u8.wrapping_add(sample as u8); 16]);
                let context = Context::from([0x80u8.wrapping_add((sample * N + i) as u8); 16]);
                let payload = payload.clone();
                lore_base::lore_spawn!(tasks, async move {
                    write_content(
                        store,
                        partition,
                        context,
                        payload,
                        WriteOptions::default(),
                        None,
                        None,
                    )
                    .await
                    .expect("direct write_content");
                });
            }
            while let Some(result) = tasks.join_next().await {
                result.expect("direct task panic");
            }
            start.elapsed()
        }

        let mut best_ratio: Option<f64> = None;
        let mut samples_log: Vec<(Duration, Duration, f64)> = Vec::with_capacity(SAMPLES);
        for sample in 0..SAMPLES {
            // Fresh store per sample bounds total resident bytes — the in-memory store
            // owns the payload bytes for the duration the handle is open, so closing
            // between samples returns memory to baseline.
            let (open_sink, open_cb) = make_sink();
            assert_eq!(open_in_memory(open_cb).await, 0);
            let id = take_opened(&open_sink.lock().unwrap()).unwrap();
            let handle = lore::storage::handle::LoreStore { handle_id: id };
            let immutable = lore::storage::handle::immutable_for_test(handle)
                .expect("handle should resolve to an immutable store");

            // Pin payloads for the whole sample. The API path's `Bytes::from_static`
            // hack inside `put.rs` requires the source buffer to outlive every event,
            // including the in-store reference until `close_handle` drops the store.
            let api_payloads: Vec<Vec<u8>> =
                (0..N).map(|i| payload_for(sample, i, len, 0xA1)).collect();
            let direct_payloads: Vec<Bytes> = (0..N)
                .map(|i| Bytes::from(payload_for(sample, i, len, 0xD2)))
                .collect();

            let (api_elapsed, direct_elapsed) = if sample % 2 == 0 {
                let api = run_api_path(handle, sample, &api_payloads).await;
                let direct = run_direct_path(immutable.clone(), sample, &direct_payloads).await;
                (api, direct)
            } else {
                let direct = run_direct_path(immutable.clone(), sample, &direct_payloads).await;
                let api = run_api_path(handle, sample, &api_payloads).await;
                (api, direct)
            };
            let ratio = api_elapsed.as_secs_f64() / direct_elapsed.as_secs_f64();
            samples_log.push((api_elapsed, direct_elapsed, ratio));
            best_ratio = Some(best_ratio.map_or(ratio, |b: f64| b.min(ratio)));

            // Close the handle and drop the immutable Arc so the store is destroyed and
            // the stored payload bytes are freed before the next sample starts. Payloads
            // outlive the close so the API path's raw-pointer view stays valid until
            // every reference is gone.
            drop(immutable);
            let (_, close_cb) = make_sink();
            let _ = close_handle(handle, close_cb).await;
            drop(api_payloads);
            drop(direct_payloads);
        }
        let best_ratio = best_ratio.expect("at least one sample");

        // Threshold: best api/direct ratio across SAMPLES paired runs must be ≤ 1.20 (i.e.
        // the API path costs at most 20% more wall time than direct concurrent
        // `write_content` for the same work). A tighter threshold is too flaky under CI
        // scheduler noise — 20% catches a real per-call overhead regression (e.g. an
        // accidental synchronization point or extra alloc per item) without flagging
        // incidental jitter.
        assert!(
            best_ratio <= 1.20,
            "best api/direct ratio across {SAMPLES} samples was {best_ratio:.3}, expected \
             ≤ 1.20; per-sample (api, direct, ratio): {samples_log:?} — `lore_storage_put` \
             appears to add per-call overhead over direct `write_content`",
        );
    }

    /// Bound global-flags validation surface for `lore_storage_open`.
    ///
    /// Open with `globals.local=1 && globals.remote=1` rejects with `InvalidArguments`. The
    /// other three single-bit modes (`offline`, `local`, `remote`) are valid bound states;
    /// per-op behavior is exercised by the dedicated bound-flag behavior tests.
    #[tokio::test]
    async fn open_with_local_and_remote_set_returns_invalid_arguments() {
        let mut bad = globals();
        bad.local = 1;
        bad.remote = 1;

        let (sink, callback) = make_sink();
        let status = open::open(
            bad,
            LoreStorageOpenArgs {
                repository_path: LoreString::default(),
                in_memory: 1,
                ..Default::default()
            },
            callback,
        )
        .await;
        assert_ne!(status, 0, "open with local=1 && remote=1 must fail");
        let events = sink.lock().unwrap().clone();
        assert!(
            !events.iter().any(|e| matches!(e, Captured::Error)),
            "no mid-stream Error event on terminal failure, got {events:?}",
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Captured::Complete(s) if *s != 0)),
            "expected Complete(1), got {events:?}",
        );
        // No Opened event must fire on a rejected open.
        assert!(
            take_opened(&events).is_none(),
            "rejected open must not emit Opened, got {events:?}",
        );
    }

    #[tokio::test]
    async fn open_with_offline_set_succeeds() {
        // `globals.offline=1` is a valid bound state. Behavior assertions live in the
        // bound-flag behavior tests; here we only verify that open accepts the flag.
        let mut g = globals();
        g.offline = 1;
        let (sink, callback) = make_sink();
        let status = open::open(
            g,
            LoreStorageOpenArgs {
                repository_path: LoreString::default(),
                in_memory: 1,
                ..Default::default()
            },
            callback,
        )
        .await;
        assert_eq!(status, 0);
        let events = sink.lock().unwrap().clone();
        let id = take_opened(&events).expect("offline open should emit Opened");
        let handle = lore::storage::handle::LoreStore { handle_id: id };
        let (_, close_cb) = make_sink();
        assert_eq!(close_handle(handle, close_cb).await, 0);
    }

    #[tokio::test]
    async fn open_with_local_only_succeeds() {
        // `globals.local=1` (alone) is a valid bound state.
        let mut g = globals();
        g.local = 1;
        let (sink, callback) = make_sink();
        let status = open::open(
            g,
            LoreStorageOpenArgs {
                repository_path: LoreString::default(),
                in_memory: 1,
                ..Default::default()
            },
            callback,
        )
        .await;
        assert_eq!(status, 0);
        let events = sink.lock().unwrap().clone();
        let id = take_opened(&events).expect("local open should emit Opened");
        let handle = lore::storage::handle::LoreStore { handle_id: id };
        let (_, close_cb) = make_sink();
        assert_eq!(close_handle(handle, close_cb).await, 0);
    }

    #[tokio::test]
    async fn open_with_remote_without_remote_config_returns_invalid_arguments() {
        // `globals.remote=1` requires `has_remote_config != 0` — a remote-bound handle
        // without a remote endpoint is unusable, so the open is rejected up front rather
        // than producing a silently-broken handle.
        let mut g = globals();
        g.remote = 1;
        let (sink, callback) = make_sink();
        let status = open::open(
            g,
            LoreStorageOpenArgs {
                repository_path: LoreString::default(),
                in_memory: 1,
                ..Default::default()
            },
            callback,
        )
        .await;
        assert_ne!(status, 0, "open with remote=1 + no remote_config must fail");
        let events = sink.lock().unwrap().clone();
        assert!(
            !events.iter().any(|e| matches!(e, Captured::Error)),
            "no mid-stream Error event on terminal failure, got {events:?}",
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Captured::Complete(s) if *s != 0))
        );
        assert!(take_opened(&events).is_none());
    }

    /// Open a disk-backed handle with explicit `cache_target_*` values, which enable the
    /// handle's incremental GC. The underlying evictor's internal floor prevents the targets
    /// from being arbitrarily small, so this test is structural — verify that the handle
    /// accepts the fields, the spawn does not panic, the handle survives an op cycle, and the
    /// close path tears the spawned tasks down cleanly (proves the spawn happened — without
    /// spawn, `compact_stop` would have no counterpart to stop)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn open_with_gc_and_cache_target_round_trips_an_op_cycle() {
        use lore_base::types::Context;
        use lore_base::types::Partition;

        let repo_dir = tempdir("gc-on");
        let repo_path = repo_dir.path();
        create_repo(repo_path).await;

        let (open_sink, open_cb) = make_sink();
        let g = globals();
        let status = open::open(
            g,
            LoreStorageOpenArgs {
                repository_path: LoreString::from(repo_path.display().to_string().as_str()),
                in_memory: 0,
                cache_target_bytes: 1024 * 1024 * 64,
                cache_target_fragments: 1024,
                ..Default::default()
            },
            open_cb,
        )
        .await;
        assert_eq!(status, 0, "open with cache_target_* must succeed");
        let id = take_opened(&open_sink.lock().unwrap()).expect("open should have emitted Opened");
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let payload = b"gc-roundtrip".to_vec();
        let item = lore::storage::put::LoreStoragePutItem {
            id: 1,
            partition: Partition::from([0xeeu8; 16]),
            context: Context::from([0x11u8; 16]),
            data: lore_revision::event::LoreBytes {
                ptr: payload.as_ptr().cast(),
                len: payload.len(),
            },
            remote_write: 0,
            local_cache: 0,
            fixed_size_chunk: 0,
        };
        let (_sink, cb) = make_sink();
        let status = lore::storage::put::put(
            globals(),
            lore::storage::put::LoreStoragePutArgs {
                handle,
                items: lore_revision::interface::LoreArray::from_vec(vec![item]),
            },
            cb,
        )
        .await;
        assert_eq!(status, 0);
        drop(payload);

        let (_, close_cb) = make_sink();
        assert_eq!(close_handle(handle, close_cb).await, 0);
    }

    /// Stress test: N tasks race to open + close handles on the same disk-backed path. The
    /// test grabs a `Weak<dyn ImmutableStore>` from the underlying backend Arc on first
    /// open, then drives the open/close cycles, and finally asserts:
    ///
    /// 1. Every open + close completes without panic under contention.
    /// 2. While at least one handle holds the path's backend Arc, the `Weak` upgrades.
    /// 3. After every handle closes AND every spawned flush task drops its Arc clone, the
    ///    `Weak` no longer upgrades — proving the cache holds only a `Weak` and the backend
    ///    really tears down on last-strong-ref-drop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_open_and_close_against_same_path_converges_cleanly() {
        use std::sync::Arc;
        use std::time::Duration;
        use std::time::Instant;

        use tokio::task::JoinSet;

        const TASKS: usize = 8;
        const CYCLES_PER_TASK: usize = 4;

        let repo_dir = tempdir("open-close-race");
        let repo_path = Arc::new(repo_dir.path().to_path_buf());
        create_repo(&repo_path).await;

        // Open one anchor handle outside the race so we can grab the path-shared backend Arc
        // and downgrade it to a `Weak`. The backend cache holds only a `Weak`, so when this
        // local Arc, every per-handle clone, and every close-spawned flush task's clone all
        // drop, the `Weak` fails to upgrade — that's the last-strong-ref-drop signal.
        let (anchor_sink, anchor_cb) = make_sink();
        assert_eq!(
            open::open(
                globals(),
                LoreStorageOpenArgs {
                    repository_path: LoreString::from(repo_path.display().to_string().as_str()),
                    in_memory: 0,
                    ..Default::default()
                },
                anchor_cb,
            )
            .await,
            0,
        );
        let anchor_id = take_opened(&anchor_sink.lock().unwrap()).expect("anchor Opened");
        let anchor_handle = lore::storage::handle::LoreStore {
            handle_id: anchor_id,
        };
        let backend_arc = lore::storage::handle::immutable_for_test(anchor_handle)
            .expect("anchor backend lookup");
        let backend_weak = Arc::downgrade(&backend_arc);
        // Drop the test's local Arc; the `StoreInternal` (via the registered handle) and
        // the cache (Weak only) hold the only references at this point.
        drop(backend_arc);
        assert!(
            backend_weak.upgrade().is_some(),
            "Weak must upgrade while a handle holds the backend",
        );

        let mut tasks: JoinSet<i32> = JoinSet::new();
        for _ in 0..TASKS {
            let p = repo_path.clone();
            #[allow(clippy::disallowed_methods)]
            tasks.spawn(async move {
                let mut last_status = 0;
                for _ in 0..CYCLES_PER_TASK {
                    let (sink, cb) = make_sink();
                    let s = open::open(
                        globals(),
                        LoreStorageOpenArgs {
                            repository_path: LoreString::from(p.display().to_string().as_str()),
                            in_memory: 0,
                            ..Default::default()
                        },
                        cb,
                    )
                    .await;
                    if s != 0 {
                        last_status = s;
                        break;
                    }
                    let id = take_opened(&sink.lock().unwrap()).expect("Opened");
                    let h = lore::storage::handle::LoreStore { handle_id: id };
                    let (_, ccb) = make_sink();
                    let cs = close_handle(h, ccb).await;
                    if cs != 0 {
                        last_status = cs;
                        break;
                    }
                }
                last_status
            });
        }
        while let Some(result) = tasks.join_next().await {
            assert_eq!(result.expect("task panic"), 0, "open/close race");
        }

        // Close the anchor. After this, no `StoreInternal` references the backend; only the
        // close-spawned flush task's Arc clones can keep it alive. Poll until they drop and
        // the Weak fails to upgrade — bounded so a regression doesn't hang.
        let (_, anchor_close_cb) = make_sink();
        assert_eq!(close_handle(anchor_handle, anchor_close_cb).await, 0);

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut released = false;
        while Instant::now() < deadline {
            if backend_weak.upgrade().is_none() {
                released = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            released,
            "backend Arc must be released after all handles + flush tasks drop their refs",
        );
    }

    /// Open with `no_gc=1`: no evictor or compactor is spawned even though `cache_target_*`
    /// are set. Putting many fragments must not cause the count to drop. We verify the
    /// negative — fragment count climbs and stays — to prove the evictor is genuinely off
    /// (rather than just slow).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn open_without_gc_does_not_spawn_evictor() {
        use lore_base::types::Context;
        use lore_base::types::Partition;

        let repo_dir = tempdir("no-gc");
        let repo_path = repo_dir.path();
        create_repo(repo_path).await;

        let (open_sink, open_cb) = make_sink();
        let mut g = globals();
        g.no_gc = 1;
        let status = open::open(
            g,
            LoreStorageOpenArgs {
                repository_path: LoreString::from(repo_path.display().to_string().as_str()),
                in_memory: 0,
                cache_target_bytes: 1024,
                cache_target_fragments: 4,
                ..Default::default()
            },
            open_cb,
        )
        .await;
        assert_eq!(status, 0);
        let id = take_opened(&open_sink.lock().unwrap()).expect("open should have emitted Opened");
        let handle = lore::storage::handle::LoreStore { handle_id: id };

        let partition = Partition::from([0xefu8; 16]);
        for i in 0..16u64 {
            let payload = format!("no-gc-test-payload-{i}").into_bytes();
            let item = lore::storage::put::LoreStoragePutItem {
                id: i,
                partition,
                context: Context::from([(i as u8) | 0x40; 16]),
                data: lore_revision::event::LoreBytes {
                    ptr: payload.as_ptr().cast(),
                    len: payload.len(),
                },
                remote_write: 0,
                local_cache: 0,
                fixed_size_chunk: 0,
            };
            let (_sink, cb) = make_sink();
            let status = lore::storage::put::put(
                globals(),
                lore::storage::put::LoreStoragePutArgs {
                    handle,
                    items: lore_revision::interface::LoreArray::from_vec(vec![item]),
                },
                cb,
            )
            .await;
            assert_eq!(status, 0);
            drop(payload);
        }

        // Sleep long enough that any evictor with a default delay would have run at least once;
        // assert the count is unchanged (no eviction happened).
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let immutable = lore::storage::handle::immutable_for_test(handle)
            .expect("handle should still be registered");
        let count = immutable.fragment_count().await.unwrap_or(0);
        assert!(
            count >= 16,
            "with no_gc=1 no evictor should run; expected >= 16 fragments, got {count}",
        );

        let (_, close_cb) = make_sink();
        assert_eq!(close_handle(handle, close_cb).await, 0);
    }
}
