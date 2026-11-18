// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `LORE_EVENT_STORAGE_PUT_STATS`, which the put and upload calls send once their items have
//! finished, at statistics level one and above.

use std::sync::Arc;
use std::sync::Mutex;

use lore::interface::LoreGlobalArgs;
use lore::storage::handle;
use lore::storage::put::LoreStoragePutArgs;
use lore::storage::put::LoreStoragePutItem;
use lore::storage::put::put;
use lore::storage::put_file::LoreStoragePutFileArgs;
use lore::storage::put_file::LoreStoragePutFileItem;
use lore::storage::put_file::put_file;
use lore::storage::put_file_resolved::LoreStoragePutFileResolvedArgs;
use lore::storage::put_file_resolved::LoreStoragePutFileResolvedItem;
use lore::storage::put_file_resolved::put_file_resolved;
use lore::storage::put_resolved::LoreStoragePutResolvedArgs;
use lore::storage::put_resolved::LoreStoragePutResolvedItem;
use lore::storage::put_resolved::put_resolved;
use lore::storage::store::in_memory_for_tests;
use lore::storage::upload::LoreStorageUploadArgs;
use lore::storage::upload::LoreStorageUploadItem;
use lore::storage::upload::upload;
use lore_base::test_util::TempFile;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_revision::event::LoreBytes;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreString;
use lore_revision::store::event::LoreStoragePutStatsEventData;

/// The four put calls.
#[derive(Clone, Copy, Debug)]
enum Call {
    Put,
    PutFile,
    PutResolved,
    PutFileResolved,
}

const CALLS: [Call; 4] = [
    Call::Put,
    Call::PutFile,
    Call::PutResolved,
    Call::PutFileResolved,
];

/// What a call sent, in order, with the item events and `Complete` reduced to their kind.
#[derive(Debug, PartialEq)]
enum Seen {
    Item,
    Stats(Box<LoreStoragePutStatsEventData>),
    FragmentWrite,
    Complete,
}

fn partition() -> Partition {
    Partition::from([0x5E; 16])
}

fn context() -> Context {
    Context::from([0x5E; 16])
}

fn globals(stats: u32) -> LoreGlobalArgs {
    LoreGlobalArgs {
        stats,
        ..LoreGlobalArgs::default()
    }
}

/// A callback that records what it is sent into the returned list.
fn recorder() -> (
    Arc<Mutex<Vec<Seen>>>,
    lore_revision::interface::LoreEventCallback,
) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let callback: lore_revision::interface::LoreEventCallback =
        Some(Box::new(move |event: &LoreEvent| {
            let kind = match event {
                LoreEvent::StoragePutItemComplete(_) | LoreEvent::StorageUploadItemComplete(_) => {
                    Seen::Item
                }
                LoreEvent::StoragePutStats(data) => Seen::Stats(Box::new(*data)),
                LoreEvent::FragmentWrite(_) => Seen::FragmentWrite,
                LoreEvent::Complete(_) => Seen::Complete,
                _ => return,
            };
            sink.lock().unwrap().push(kind);
        }));
    (seen, callback)
}

/// Stores each of `contents` through a new handle on `store` with `call`, one item each, in the
/// partition `partitions` gives that item. Returns the call's status and what it sent.
async fn put_with(
    call: Call,
    store: &Arc<lore::storage::store::StoreInternal>,
    stats: u32,
    contents: &[&[u8]],
    partitions: &[Partition],
) -> (i32, Vec<Seen>) {
    let files: Vec<TempFile> = contents
        .iter()
        .map(|content| TempFile::with_contents("lore-put-stats-", content))
        .collect();
    let path = |index: usize| LoreString::from(files[index].path().to_str().expect("text"));
    let data = |index: usize| LoreBytes {
        ptr: contents[index].as_ptr().cast(),
        len: contents[index].len(),
    };
    let key = |index: usize| Hash::hash_buffer(&[index as u8 + 1]);
    let handle = handle::register(store.clone());
    let (seen, callback) = recorder();
    let indices = 0..contents.len();
    let status = match call {
        Call::Put => {
            let items = indices
                .map(|index| LoreStoragePutItem {
                    id: index as u64,
                    partition: partitions[index],
                    context: context(),
                    data: data(index),
                    remote_write: 0,
                    local_cache: 0,
                    remote_query: 0,
                    fixed_size_chunk: 0,
                })
                .collect();
            let args = LoreStoragePutArgs {
                handle,
                items: LoreArray::from_vec(items),
            };
            put(globals(stats), args, callback).await
        }
        Call::PutFile => {
            let items = indices
                .map(|index| LoreStoragePutFileItem {
                    id: index as u64,
                    partition: partitions[index],
                    context: context(),
                    path: path(index),
                    ..Default::default()
                })
                .collect();
            let args = LoreStoragePutFileArgs {
                handle,
                items: LoreArray::from_vec(items),
            };
            put_file(globals(stats), args, callback).await
        }
        Call::PutResolved => {
            let items = indices
                .map(|index| LoreStoragePutResolvedItem {
                    id: index as u64,
                    partition: partitions[index],
                    key: key(index),
                    context: context(),
                    data: data(index),
                    remote_write: 0,
                    local_cache: 0,
                    remote_query: 0,
                    fixed_size_chunk: 0,
                })
                .collect();
            let args = LoreStoragePutResolvedArgs {
                handle,
                items: LoreArray::from_vec(items),
            };
            put_resolved(globals(stats), args, callback).await
        }
        Call::PutFileResolved => {
            let items = indices
                .map(|index| LoreStoragePutFileResolvedItem {
                    id: index as u64,
                    partition: partitions[index],
                    key: key(index),
                    context: context(),
                    path: path(index),
                    ..Default::default()
                })
                .collect();
            let args = LoreStoragePutFileResolvedArgs {
                handle,
                items: LoreArray::from_vec(items),
            };
            put_file_resolved(globals(stats), args, callback).await
        }
    };
    handle::unregister(handle);
    let seen = std::mem::take(&mut *seen.lock().unwrap());
    (status, seen)
}

/// The stats events among `seen`.
fn stats_of(seen: &[Seen]) -> Vec<LoreStoragePutStatsEventData> {
    seen.iter()
        .filter_map(|seen| match seen {
            Seen::Stats(data) => Some(**data),
            _ => None,
        })
        .collect()
}

/// At level one each put call sends one stats event, after every item event and before
/// `Complete`, for a batch of one item and for a batch of several.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_put_sends_one_stats_event_between_its_items_and_complete() {
    for call in CALLS {
        for contents in [&[b"one item".as_slice()][..], &[b"first", b"second"]] {
            let store = in_memory_for_tests("put-stats-order").await;
            let partitions = vec![partition(); contents.len()];
            let (status, seen) = put_with(call, &store, 1, contents, &partitions).await;
            assert_eq!(status, 0, "{call:?}");
            let stats = stats_of(&seen);
            assert_eq!(stats.len(), 1, "{call:?}: {seen:?}");
            let items = contents.len();
            assert!(
                seen[..items].iter().all(|seen| *seen == Seen::Item)
                    && matches!(seen[items], Seen::Stats(_))
                    && seen[items + 1] == Seen::Complete,
                "{call:?}: {seen:?}"
            );
            assert_eq!(stats[0].items, items as u64, "{call:?}");
            assert_eq!(stats[0].items_failed, 0, "{call:?}");
            assert_eq!(
                stats[0].fragments.fragments_produced, items as u64,
                "{call:?}"
            );
            assert_eq!(stats[0].fragments.fragments_deduplicated, 0, "{call:?}");
        }
    }
}

/// A put of content the store already holds reports every fragment as deduplicated, and writes no
/// payload.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repeated_put_reports_its_fragments_as_deduplicated() {
    for call in CALLS {
        let store = in_memory_for_tests("put-stats-dedup").await;
        let contents = [b"content stored twice".as_slice()];
        put_with(call, &store, 1, &contents, &[partition()]).await;
        let (status, seen) = put_with(call, &store, 1, &contents, &[partition()]).await;
        assert_eq!(status, 0, "{call:?}");
        let stats = stats_of(&seen);
        assert_eq!(stats.len(), 1, "{call:?}: {seen:?}");
        let fragments = stats[0].fragments;
        assert_eq!(fragments.fragments_produced, 1, "{call:?}");
        assert_eq!(fragments.fragments_deduplicated, 1, "{call:?}");
        assert_eq!(fragments.local_payload_writes, 0, "{call:?}");
    }
}

/// A call one of whose items fails still sends its stats event, with the failed item counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_with_a_failed_item_reports_it() {
    for call in CALLS {
        let store = in_memory_for_tests("put-stats-failed").await;
        let contents = [b"stored".as_slice(), b"rejected"];
        let partitions = [partition(), Partition::default()];
        let (status, seen) = put_with(call, &store, 1, &contents, &partitions).await;
        assert_ne!(status, 0, "{call:?}");
        let stats = stats_of(&seen);
        assert_eq!(stats.len(), 1, "{call:?}: {seen:?}");
        assert_eq!(stats[0].items, 2, "{call:?}");
        assert_eq!(stats[0].items_failed, 1, "{call:?}");
        assert_eq!(seen.last(), Some(&Seen::Complete), "{call:?}");
    }
}

/// At level zero no put call sends a stats event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_stats_event_at_level_zero() {
    for call in CALLS {
        let store = in_memory_for_tests("put-stats-level-zero").await;
        let (status, seen) = put_with(call, &store, 0, &[b"content"], &[partition()]).await;
        assert_eq!(status, 0, "{call:?}");
        assert_eq!(seen, [Seen::Item, Seen::Complete], "{call:?}");
    }
}

/// At level two each put call sends what it sends at level one, and no event per fragment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn level_two_sends_what_level_one_does() {
    for call in CALLS {
        let store = in_memory_for_tests("put-stats-level-two").await;
        let (status, seen) = put_with(call, &store, 2, &[b"content"], &[partition()]).await;
        assert_eq!(status, 0, "{call:?}");
        assert!(
            matches!(seen[..], [Seen::Item, Seen::Stats(_), Seen::Complete]),
            "{call:?}: {seen:?}"
        );
    }
}

/// A put call of no items sends no stats event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_put_of_no_items_sends_no_stats_event() {
    for call in CALLS {
        let store = in_memory_for_tests("put-stats-no-items").await;
        let (status, seen) = put_with(call, &store, 1, &[], &[]).await;
        assert_eq!(status, 0, "{call:?}");
        assert_eq!(seen, [Seen::Complete], "{call:?}");
    }
}

/// An upload that fails before its items run, on a handle with no remote, sends no stats event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upload_rejected_before_its_items_sends_no_stats_event() {
    let store = in_memory_for_tests("put-stats-upload-rejected").await;
    let handle = handle::register(store);
    let (seen, callback) = recorder();
    let status = upload(
        globals(1),
        LoreStorageUploadArgs {
            handle,
            items: LoreArray::from_vec(vec![LoreStorageUploadItem {
                id: 1,
                partition: partition(),
                ..Default::default()
            }]),
        },
        callback,
    )
    .await;
    handle::unregister(handle);
    assert_ne!(status, 0);
    assert_eq!(*seen.lock().unwrap(), [Seen::Complete]);
}
