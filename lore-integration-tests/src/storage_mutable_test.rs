// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Integration tests for the mutable storage API's local-backed paths.
//!
//! Exercise `mutable_load`, `mutable_store`, `mutable_compare_and_swap`, and `mutable_list`
//! against an in-memory handle (no remote configured), so every op resolves on the handle's
//! local mutable store. The no-re-export contract is pinned by the imports in the `imports`
//! module.

#[cfg(test)]
#[allow(unused_imports)]
mod imports {
    use lore::storage::handle::LoreStore;
    use lore_base::types::Hash;
    use lore_base::types::KeyType;
    use lore_base::types::Partition;
    use lore_revision::event::LoreErrorCode;
}

#[cfg(test)]
mod mutable_local_tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use lore::storage::mutable_compare_and_swap;
    use lore::storage::mutable_compare_and_swap::LoreStorageMutableCompareAndSwapArgs;
    use lore::storage::mutable_compare_and_swap::LoreStorageMutableCompareAndSwapItem;
    use lore::storage::mutable_list;
    use lore::storage::mutable_list::LoreStorageMutableListArgs;
    use lore::storage::mutable_list::LoreStorageMutableListItem;
    use lore::storage::mutable_load;
    use lore::storage::mutable_load::LoreStorageMutableLoadArgs;
    use lore::storage::mutable_load::LoreStorageMutableLoadItem;
    use lore::storage::mutable_store;
    use lore::storage::mutable_store::LoreStorageMutableStoreArgs;
    use lore::storage::mutable_store::LoreStorageMutableStoreItem;
    use lore::storage::open;
    use lore::storage::open::LoreStorageOpenArgs;
    use lore_base::types::Hash;
    use lore_base::types::KeyType;
    use lore_base::types::Partition;
    use lore_revision::event::LoreErrorCode;
    use lore_revision::event::LoreEvent;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreEventCallback;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreString;

    const KEY_TYPE: KeyType = KeyType::BranchLatestPointer;

    fn globals() -> LoreGlobalArgs {
        LoreGlobalArgs::default()
    }

    fn handle(handle_id: u64) -> lore::storage::handle::LoreStore {
        lore::storage::handle::LoreStore { handle_id }
    }

    /// Per-call `globals.remote = 1` selects the remote mutable store.
    fn remote_globals() -> LoreGlobalArgs {
        LoreGlobalArgs {
            remote: 1,
            ..Default::default()
        }
    }

    /// Capture for the no-remote rejection tests: the `Complete.status`, and whether ANY per-item
    /// mutable event leaked (none should — the call is rejected up front).
    #[derive(Default)]
    struct RejectCapture {
        complete_status: Option<i32>,
        saw_item_event: bool,
    }

    fn make_reject_sink() -> (Arc<Mutex<RejectCapture>>, LoreEventCallback) {
        let capture: Arc<Mutex<RejectCapture>> = Arc::new(Mutex::new(RejectCapture::default()));
        let capture_for_cb = capture.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            let mut capture = capture_for_cb.lock().unwrap();
            match event {
                LoreEvent::Complete(data) => capture.complete_status = Some(data.status),
                LoreEvent::StorageMutableLoadItemComplete(_)
                | LoreEvent::StorageMutableStoreItemComplete(_)
                | LoreEvent::StorageMutableCompareAndSwapItemComplete(_)
                | LoreEvent::StorageMutableListEntry(_)
                | LoreEvent::StorageMutableListItemComplete(_) => capture.saw_item_event = true,
                _ => {}
            }
        }));
        (capture, callback)
    }

    fn assert_rejected(status: i32, capture: &Arc<Mutex<RejectCapture>>, op: &str) {
        assert_eq!(
            status, 1,
            "{op}: remote op on a local-only handle must fail the call"
        );
        let capture = capture.lock().unwrap();
        assert_eq!(
            capture.complete_status,
            Some(1),
            "{op}: expected Complete(1)"
        );
        assert!(
            !capture.saw_item_event,
            "{op}: no per-item events expected on a pre-dispatch rejection",
        );
    }

    /// Open an in-memory handle and return its id. A bare in-memory handle has no remote, so
    /// every mutable op resolves locally.
    async fn open_in_memory() -> u64 {
        let sink: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_for_cb = sink.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StorageOpened(data) = event {
                sink_for_cb.lock().unwrap().push(data.handle_id);
            }
        }));
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
        assert_eq!(status, 0, "in-memory open must succeed");
        let id = *sink.lock().unwrap().first().expect("Opened event with id");
        assert_ne!(id, 0);
        id
    }

    /// Run a store call and return `(status, per-item (id, error_code))`.
    async fn store_items(
        handle_id: u64,
        items: Vec<LoreStorageMutableStoreItem>,
    ) -> (i32, Vec<(u64, LoreErrorCode)>) {
        let captured: Arc<Mutex<Vec<(u64, LoreErrorCode)>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_for_cb = captured.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StorageMutableStoreItemComplete(data) = event {
                captured_for_cb
                    .lock()
                    .unwrap()
                    .push((data.id, data.error_code));
            }
        }));
        let status = mutable_store::mutable_store(
            globals(),
            LoreStorageMutableStoreArgs {
                handle: handle(handle_id),
                items: LoreArray::from_vec(items),
            },
            callback,
        )
        .await;
        let events = captured.lock().unwrap().clone();
        (status, events)
    }

    /// Store a single key-value pair on the local mutable store, asserting success.
    async fn store_one(handle_id: u64, partition: Partition, key: Hash, value: Hash) {
        let (status, completes) = store_items(
            handle_id,
            vec![LoreStorageMutableStoreItem {
                id: 1,
                partition,
                key,
                value,
                key_type: KEY_TYPE,
            }],
        )
        .await;
        assert_eq!(status, 0, "store must succeed");
        assert_eq!(completes, vec![(1, LoreErrorCode::None)]);
    }

    /// Run a load call and return `(status, per-item (id, value, error_code))`.
    async fn load_items(
        handle_id: u64,
        items: Vec<LoreStorageMutableLoadItem>,
    ) -> (i32, Vec<(u64, Hash, LoreErrorCode)>) {
        let captured: Arc<Mutex<Vec<(u64, Hash, LoreErrorCode)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let captured_for_cb = captured.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StorageMutableLoadItemComplete(data) = event {
                captured_for_cb
                    .lock()
                    .unwrap()
                    .push((data.id, data.value, data.error_code));
            }
        }));
        let status = mutable_load::mutable_load(
            globals(),
            LoreStorageMutableLoadArgs {
                handle: handle(handle_id),
                items: LoreArray::from_vec(items),
            },
            callback,
        )
        .await;
        let events = captured.lock().unwrap().clone();
        (status, events)
    }

    /// Load a single key and return its `(value, error_code)`.
    async fn load_one(handle_id: u64, partition: Partition, key: Hash) -> (Hash, LoreErrorCode) {
        let (_status, events) = load_items(
            handle_id,
            vec![LoreStorageMutableLoadItem {
                id: 9,
                partition,
                key,
                key_type: KEY_TYPE,
            }],
        )
        .await;
        assert_eq!(events.len(), 1, "exactly one load complete event");
        (events[0].1, events[0].2)
    }

    /// Run a compare-and-swap call and return `(status, per-item (id, previous, error_code))`.
    async fn cas_items(
        handle_id: u64,
        items: Vec<LoreStorageMutableCompareAndSwapItem>,
    ) -> (i32, Vec<(u64, Hash, LoreErrorCode)>) {
        let captured: Arc<Mutex<Vec<(u64, Hash, LoreErrorCode)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let captured_for_cb = captured.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StorageMutableCompareAndSwapItemComplete(data) = event {
                captured_for_cb
                    .lock()
                    .unwrap()
                    .push((data.id, data.previous, data.error_code));
            }
        }));
        let status = mutable_compare_and_swap::mutable_compare_and_swap(
            globals(),
            LoreStorageMutableCompareAndSwapArgs {
                handle: handle(handle_id),
                items: LoreArray::from_vec(items),
            },
            callback,
        )
        .await;
        let events = captured.lock().unwrap().clone();
        (status, events)
    }

    /// Run a list call and return `(status, entries (key, value), complete error_code)`.
    async fn list_one(
        handle_id: u64,
        partition: Partition,
        key_type: KeyType,
    ) -> (i32, Vec<(Hash, Hash)>, Option<LoreErrorCode>) {
        let entries: Arc<Mutex<Vec<(Hash, Hash)>>> = Arc::new(Mutex::new(Vec::new()));
        let complete: Arc<Mutex<Option<LoreErrorCode>>> = Arc::new(Mutex::new(None));
        let entries_for_cb = entries.clone();
        let complete_for_cb = complete.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| match event {
            LoreEvent::StorageMutableListEntry(data) => {
                entries_for_cb.lock().unwrap().push((data.key, data.value));
            }
            LoreEvent::StorageMutableListItemComplete(data) => {
                *complete_for_cb.lock().unwrap() = Some(data.error_code);
            }
            _ => {}
        }));
        let status = mutable_list::mutable_list(
            globals(),
            LoreStorageMutableListArgs {
                handle: handle(handle_id),
                items: LoreArray::from_vec(vec![LoreStorageMutableListItem {
                    id: 5,
                    partition,
                    key_type,
                }]),
            },
            callback,
        )
        .await;
        let entries = entries.lock().unwrap().clone();
        let complete = *complete.lock().unwrap();
        (status, entries, complete)
    }

    #[tokio::test]
    async fn store_then_load_round_trips_value() {
        let handle_id = open_in_memory().await;
        let partition = Partition::from([0x11u8; 16]);
        let key = Hash::from([0x22u8; 32]);
        let value = Hash::from([0x33u8; 32]);

        store_one(handle_id, partition, key, value).await;
        let (loaded, code) = load_one(handle_id, partition, key).await;
        assert_eq!(code, LoreErrorCode::None);
        assert_eq!(loaded, value, "load must return the stored value");
    }

    #[tokio::test]
    async fn load_missing_key_returns_address_not_found() {
        let handle_id = open_in_memory().await;
        let partition = Partition::from([0x44u8; 16]);
        let key = Hash::from([0x55u8; 32]);

        let (value, code) = load_one(handle_id, partition, key).await;
        assert_eq!(code, LoreErrorCode::AddressNotFound);
        // Errored loads carry a zero value.
        assert_eq!(value, Hash::default());
    }

    #[tokio::test]
    async fn store_null_value_removes_key() {
        let handle_id = open_in_memory().await;
        let partition = Partition::from([0x66u8; 16]);
        let key = Hash::from([0x77u8; 32]);
        let value = Hash::from([0x88u8; 32]);

        store_one(handle_id, partition, key, value).await;
        assert_eq!(
            load_one(handle_id, partition, key).await.1,
            LoreErrorCode::None
        );

        // Storing the null value removes the key.
        store_one(handle_id, partition, key, Hash::default()).await;
        assert_eq!(
            load_one(handle_id, partition, key).await.1,
            LoreErrorCode::AddressNotFound,
            "key must be gone after storing the null value",
        );
    }

    #[tokio::test]
    async fn compare_and_swap_absent_key_sets_value() {
        let handle_id = open_in_memory().await;
        let partition = Partition::from([0x99u8; 16]);
        let key = Hash::from([0xaau8; 32]);
        let value = Hash::from([0xbbu8; 32]);

        // expected == null matches an absent key, so the swap takes effect.
        let (status, completes) = cas_items(
            handle_id,
            vec![LoreStorageMutableCompareAndSwapItem {
                id: 1,
                partition,
                key,
                expected: Hash::default(),
                value,
                key_type: KEY_TYPE,
            }],
        )
        .await;
        assert_eq!(status, 0);
        assert_eq!(completes.len(), 1);
        let (_id, previous, code) = completes[0];
        assert_eq!(code, LoreErrorCode::None);
        // The swap took effect: previous (zero) equals the supplied expected (zero).
        assert_eq!(previous, Hash::default());
        assert_eq!(load_one(handle_id, partition, key).await.0, value);
    }

    #[tokio::test]
    async fn compare_and_swap_match_swaps_and_returns_previous() {
        let handle_id = open_in_memory().await;
        let partition = Partition::from([0xccu8; 16]);
        let key = Hash::from([0xddu8; 32]);
        let current = Hash::from([0x01u8; 32]);
        let next = Hash::from([0x02u8; 32]);

        store_one(handle_id, partition, key, current).await;

        let (status, completes) = cas_items(
            handle_id,
            vec![LoreStorageMutableCompareAndSwapItem {
                id: 1,
                partition,
                key,
                expected: current,
                value: next,
                key_type: KEY_TYPE,
            }],
        )
        .await;
        assert_eq!(status, 0);
        let (_id, previous, code) = completes[0];
        assert_eq!(code, LoreErrorCode::None);
        // previous == expected → the swap took effect.
        assert_eq!(previous, current);
        assert_eq!(load_one(handle_id, partition, key).await.0, next);
    }

    #[tokio::test]
    async fn compare_and_swap_mismatch_returns_current_and_does_not_swap() {
        let handle_id = open_in_memory().await;
        let partition = Partition::from([0xeeu8; 16]);
        let key = Hash::from([0xffu8; 32]);
        let current = Hash::from([0x03u8; 32]);
        let wrong_expected = Hash::from([0x04u8; 32]);
        let attempted = Hash::from([0x05u8; 32]);

        store_one(handle_id, partition, key, current).await;

        let (status, completes) = cas_items(
            handle_id,
            vec![LoreStorageMutableCompareAndSwapItem {
                id: 1,
                partition,
                key,
                expected: wrong_expected,
                value: attempted,
                key_type: KEY_TYPE,
            }],
        )
        .await;
        assert_eq!(status, 0, "a no-op CAS is still a successful call");
        let (_id, previous, code) = completes[0];
        assert_eq!(code, LoreErrorCode::None);
        // previous != expected → no swap; previous reflects the actual current value.
        assert_eq!(previous, current);
        assert_eq!(
            load_one(handle_id, partition, key).await.0,
            current,
            "value must be unchanged after a mismatched CAS",
        );
    }

    #[tokio::test]
    async fn list_returns_all_stored_pairs() {
        let handle_id = open_in_memory().await;
        let partition = Partition::from([0x10u8; 16]);
        // The mutable store encodes the key_type into key byte 2, and `list` returns those typed
        // keys. Build keys that already carry `KEY_TYPE` in byte 2 so they are fixed points of
        // that encoding and the listed keys round-trip exactly to what was stored.
        let pairs: Vec<(Hash, Hash)> = (0u8..5)
            .map(|i| {
                let mut key = [i; 32];
                key[2] = KEY_TYPE as u8;
                (Hash::from(key), Hash::from([i.wrapping_add(0x80); 32]))
            })
            .collect();
        for (key, value) in &pairs {
            store_one(handle_id, partition, *key, *value).await;
        }

        let (status, mut entries, complete) = list_one(handle_id, partition, KEY_TYPE).await;
        assert_eq!(status, 0);
        assert_eq!(complete, Some(LoreErrorCode::None));
        entries.sort();
        let mut expected = pairs.clone();
        expected.sort();
        assert_eq!(entries, expected, "list must return every stored pair");
    }

    #[tokio::test]
    async fn list_zero_partition_returns_entries_across_all_partitions() {
        let handle_id = open_in_memory().await;
        let part_a = Partition::from([0x71u8; 16]);
        let part_b = Partition::from([0x72u8; 16]);

        // `list` returns typed keys (key_type written into byte 2), so use fixed-point keys that
        // already carry KEY_TYPE in byte 2 and therefore round-trip exactly. Keys are distinct
        // across the two partitions so the union is unambiguous.
        let typed_key = |byte: u8| {
            let mut key = [byte; 32];
            key[2] = KEY_TYPE as u8;
            Hash::from(key)
        };
        let a_pairs: Vec<(Hash, Hash)> = (0u8..3)
            .map(|i| {
                (
                    typed_key(0x10u8.wrapping_add(i)),
                    Hash::from([0x90u8.wrapping_add(i); 32]),
                )
            })
            .collect();
        let b_pairs: Vec<(Hash, Hash)> = (0u8..2)
            .map(|i| {
                (
                    typed_key(0x20u8.wrapping_add(i)),
                    Hash::from([0xa0u8.wrapping_add(i); 32]),
                )
            })
            .collect();
        for (key, value) in &a_pairs {
            store_one(handle_id, part_a, *key, *value).await;
        }
        for (key, value) in &b_pairs {
            store_one(handle_id, part_b, *key, *value).await;
        }

        // The zero/default partition matches every partition the caller can access.
        let (status, mut entries, complete) =
            list_one(handle_id, Partition::default(), KEY_TYPE).await;
        assert_eq!(status, 0);
        assert_eq!(complete, Some(LoreErrorCode::None));
        entries.sort();
        let mut expected: Vec<(Hash, Hash)> =
            a_pairs.iter().chain(b_pairs.iter()).copied().collect();
        expected.sort();
        assert_eq!(
            entries, expected,
            "zero partition must list entries from every partition",
        );

        // Flip side: a concrete partition lists only its own entries, not the other partition's.
        let (single_status, mut only_a, single_complete) =
            list_one(handle_id, part_a, KEY_TYPE).await;
        assert_eq!(single_status, 0);
        assert_eq!(single_complete, Some(LoreErrorCode::None));
        only_a.sort();
        let mut a_sorted = a_pairs.clone();
        a_sorted.sort();
        assert_eq!(
            only_a, a_sorted,
            "a concrete partition must not leak entries from other partitions",
        );
    }

    #[tokio::test]
    async fn list_empty_partition_completes_with_no_entries() {
        let handle_id = open_in_memory().await;
        let partition = Partition::from([0x20u8; 16]);

        let (status, entries, complete) = list_one(handle_id, partition, KEY_TYPE).await;
        assert_eq!(status, 0);
        assert!(
            entries.is_empty(),
            "no entries expected for an empty partition"
        );
        assert_eq!(complete, Some(LoreErrorCode::None));
    }

    #[tokio::test]
    async fn list_filters_by_key_type() {
        let handle_id = open_in_memory().await;
        let partition = Partition::from([0x30u8; 16]);
        let key = Hash::from([0x31u8; 32]);
        let value = Hash::from([0x32u8; 32]);

        // Store under KEY_TYPE, then list a different type — the entry must not appear.
        store_one(handle_id, partition, key, value).await;
        let other_type = KeyType::BranchId;
        assert_ne!(other_type, KEY_TYPE);
        let (status, entries, complete) = list_one(handle_id, partition, other_type).await;
        assert_eq!(status, 0);
        assert!(
            entries.is_empty(),
            "listing a different key_type must not surface the stored entry",
        );
        assert_eq!(complete, Some(LoreErrorCode::None));
    }

    #[tokio::test]
    async fn load_zero_partition_rejects_invalid_args() {
        let handle_id = open_in_memory().await;
        let (value, code) =
            load_one(handle_id, Partition::default(), Hash::from([0x41u8; 32])).await;
        assert_eq!(code, LoreErrorCode::InvalidArguments);
        assert_eq!(value, Hash::default());
    }

    #[tokio::test]
    async fn store_zero_partition_rejects_invalid_args() {
        let handle_id = open_in_memory().await;
        let (status, completes) = store_items(
            handle_id,
            vec![LoreStorageMutableStoreItem {
                id: 1,
                partition: Partition::default(),
                key: Hash::from([0x42u8; 32]),
                value: Hash::from([0x43u8; 32]),
                key_type: KEY_TYPE,
            }],
        )
        .await;
        assert_eq!(status, 1);
        assert_eq!(completes, vec![(1, LoreErrorCode::InvalidArguments)]);
    }

    #[tokio::test]
    async fn store_empty_items_completes_with_status_zero() {
        let handle_id = open_in_memory().await;
        let (status, completes) = store_items(handle_id, vec![]).await;
        assert_eq!(status, 0);
        assert!(
            completes.is_empty(),
            "no per-item events expected on empty input"
        );
    }

    #[tokio::test]
    async fn load_unknown_handle_returns_invalid_arguments() {
        // Unknown handle → call-level error, no per-item events.
        let saw_complete: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
        let saw_item: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        let saw_complete_cb = saw_complete.clone();
        let saw_item_cb = saw_item.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| match event {
            LoreEvent::Complete(data) => *saw_complete_cb.lock().unwrap() = Some(data.status),
            LoreEvent::StorageMutableLoadItemComplete(_) => *saw_item_cb.lock().unwrap() = true,
            _ => {}
        }));
        let status = mutable_load::mutable_load(
            globals(),
            LoreStorageMutableLoadArgs {
                handle: lore::storage::handle::LoreStore::INVALID,
                items: LoreArray::from_vec(vec![LoreStorageMutableLoadItem::default()]),
            },
            callback,
        )
        .await;
        assert_eq!(status, 1);
        assert_eq!(*saw_complete.lock().unwrap(), Some(1));
        assert!(
            !*saw_item.lock().unwrap(),
            "no per-item events on handle rejection"
        );
    }

    #[tokio::test]
    async fn independent_items_report_per_item_outcomes() {
        // One valid store and one zero-partition store in the same call: the call fails, but
        // each item reports its own outcome.
        let handle_id = open_in_memory().await;
        let good_partition = Partition::from([0x61u8; 16]);
        let (status, mut completes) = store_items(
            handle_id,
            vec![
                LoreStorageMutableStoreItem {
                    id: 1,
                    partition: good_partition,
                    key: Hash::from([0x62u8; 32]),
                    value: Hash::from([0x63u8; 32]),
                    key_type: KEY_TYPE,
                },
                LoreStorageMutableStoreItem {
                    id: 2,
                    partition: Partition::default(),
                    key: Hash::from([0x64u8; 32]),
                    value: Hash::from([0x65u8; 32]),
                    key_type: KEY_TYPE,
                },
            ],
        )
        .await;
        assert_eq!(status, 1, "one failed item fails the call");
        completes.sort_by_key(|(id, _)| *id);
        assert_eq!(
            completes,
            vec![
                (1, LoreErrorCode::None),
                (2, LoreErrorCode::InvalidArguments),
            ],
        );
    }

    #[tokio::test]
    async fn remote_op_on_local_only_handle_rejects_invalid_arguments() {
        // A handle with no remote configured, asked to act remotely (`globals.remote=1`), rejects
        // the whole call up front (status 1 + Error + Complete(1)) and emits no per-item events —
        // mirroring `lore_storage_upload` on a remote-less handle.
        let handle_id = open_in_memory().await;
        let partition = Partition::from([0x80u8; 16]);
        let key = Hash::from([0x81u8; 32]);

        let (capture, callback) = make_reject_sink();
        let status = mutable_store::mutable_store(
            remote_globals(),
            LoreStorageMutableStoreArgs {
                handle: handle(handle_id),
                items: LoreArray::from_vec(vec![LoreStorageMutableStoreItem {
                    id: 1,
                    partition,
                    key,
                    value: Hash::from([0x82u8; 32]),
                    key_type: KEY_TYPE,
                }]),
            },
            callback,
        )
        .await;
        assert_rejected(status, &capture, "mutable_store");

        let (capture, callback) = make_reject_sink();
        let status = mutable_load::mutable_load(
            remote_globals(),
            LoreStorageMutableLoadArgs {
                handle: handle(handle_id),
                items: LoreArray::from_vec(vec![LoreStorageMutableLoadItem {
                    id: 1,
                    partition,
                    key,
                    key_type: KEY_TYPE,
                }]),
            },
            callback,
        )
        .await;
        assert_rejected(status, &capture, "mutable_load");

        let (capture, callback) = make_reject_sink();
        let status = mutable_compare_and_swap::mutable_compare_and_swap(
            remote_globals(),
            LoreStorageMutableCompareAndSwapArgs {
                handle: handle(handle_id),
                items: LoreArray::from_vec(vec![LoreStorageMutableCompareAndSwapItem {
                    id: 1,
                    partition,
                    key,
                    expected: Hash::default(),
                    value: Hash::from([0x83u8; 32]),
                    key_type: KEY_TYPE,
                }]),
            },
            callback,
        )
        .await;
        assert_rejected(status, &capture, "mutable_compare_and_swap");

        let (capture, callback) = make_reject_sink();
        let status = mutable_list::mutable_list(
            remote_globals(),
            LoreStorageMutableListArgs {
                handle: handle(handle_id),
                items: LoreArray::from_vec(vec![LoreStorageMutableListItem {
                    id: 1,
                    partition,
                    key_type: KEY_TYPE,
                }]),
            },
            callback,
        )
        .await;
        assert_rejected(status, &capture, "mutable_list");
    }
}
