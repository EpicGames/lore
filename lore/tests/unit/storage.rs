// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::types::Address;
use lore_storage::StorageError;

mod call;
mod close;
mod handle;
mod open;
mod store;

use std::sync::Arc;

use lore::storage::store::StoreInternal;
use lore::storage::store::in_memory_for_tests;
use lore::storage::*;
use lore_error_set::FfiError;

fn address_not_found() -> StorageError {
    StorageError::from(lore_base::error::AddressNotFound::from(Address::default()))
}

/// Reduce a whole batch, as the fan-out does one item at a time.
fn reduce(
    results: impl IntoIterator<Item = Result<(), StorageError>>,
    total: usize,
) -> Result<(), StorageError> {
    let mut outcomes = ItemOutcomes::default();
    for result in results {
        outcomes.push(result);
    }
    outcomes.into_call_result(total, "get")
}

#[test]
fn no_failures_reduce_to_success() {
    assert!(reduce([Ok(()), Ok(())], 2).is_ok());
    assert!(reduce([], 0).is_ok());
}

/// The call reports the failing item's own error, not a summary code.
#[test]
fn a_lone_failure_reaches_the_call_unchanged() {
    let err =
        reduce([Err(address_not_found())], 1).expect_err("one failed item must fail the call");

    assert_eq!(
        err.ffi_code(),
        lore_base::error::AddressNotFound::FFI_CODE,
        "the call must carry the item's own code"
    );
    assert!(err.is_address_not_found());
}

/// Severity order, not arrival order, decides which failure stands for the call.
#[test]
fn the_most_actionable_failure_stands_for_the_call() {
    for results in [
        vec![Err(invalid_item("bad item")), Err(address_not_found())],
        vec![Err(address_not_found()), Err(invalid_item("bad item"))],
    ] {
        let err = reduce(results, 2).expect_err("failures must fail the call");
        assert!(
            err.is_invalid_arguments(),
            "InvalidArguments must outrank AddressNotFound, got {err}"
        );
    }
}

/// A miss must never outrank a retry hint, whichever variant it arrived as. A remote lookup
/// reports `NotFound` or `NoRemote` where a local one reports `AddressNotFound`, so all four
/// have to rank as misses, or one absent key hides a `SlowDown` raised by another item.
#[test]
fn a_miss_never_outranks_a_retry_hint() {
    let misses = [
        address_not_found(),
        StorageError::from(lore_base::error::PayloadNotFound { hash: [0u8; 32] }),
        StorageError::from(lore_base::error::NotFound),
        StorageError::from(lore_base::error::NoRemote),
    ];

    for miss in misses {
        let reported = format!("{miss}");
        let err = reduce(
            [
                Err(miss),
                Err(StorageError::from(lore_base::error::SlowDown)),
            ],
            2,
        )
        .expect_err("failures must fail the call");

        assert!(
            err.is_slow_down(),
            "SlowDown must outrank the miss '{reported}', got {err}"
        );
    }
}

/// Folding two reductions together matches reducing every item at once.
#[test]
fn folding_two_reductions_matches_one() {
    let mut local = ItemOutcomes::default();
    local.push(Err(address_not_found()));
    local.push(Ok(()));
    let mut remote = ItemOutcomes::default();
    remote.push(Err(invalid_item("bad item")));

    local.absorb(remote);
    let err = local
        .into_call_result(3, "get_metadata")
        .expect_err("failures must fail the call");

    assert!(
        err.is_invalid_arguments(),
        "the dominant failure must survive the fold, got {err}"
    );
    assert!(
        err.trace()
            .locations()
            .iter()
            .any(|location| location.context() == Some("2/3 get_metadata items failed")),
        "both paths' failures must be counted"
    );
}

/// The failure count reaches the caller on the trace, not in the message.
#[test]
fn the_failure_count_lands_on_the_trace() {
    let err = reduce(
        [Ok(()), Err(address_not_found()), Err(address_not_found())],
        3,
    )
    .expect_err("failures must fail the call");

    assert!(
        err.trace()
            .locations()
            .iter()
            .any(|location| location.context() == Some("2/3 get items failed")),
        "the count must reach the caller as trace context, got {:?}",
        err.trace().locations()
    );
}

/// `drain_in_parallel` is the worker `close_all_handles` and `close_for_connection`
/// invoke after they collect entries. Driving it directly with a hand-built entry list
/// avoids racing against other tests through the process-global registry. Each entry's
/// store gets marked invalid and a flush task spawns.
#[tokio::test]
async fn drain_in_parallel_marks_each_store_invalid() {
    let s1 = in_memory_for_tests("drain-1").await;
    let s2 = in_memory_for_tests("drain-2").await;
    let w1 = Arc::downgrade(&s1);
    let w2 = Arc::downgrade(&s2);

    drain_in_parallel(vec![(1, s1), (2, s2)]).await;

    // The flush task holds an Arc clone, so the strong count may still be > 0 after the
    // drain returns — assert against the invalid flag instead.
    for w in [w1, w2] {
        let invalid = w
            .upgrade()
            .is_none_or(|s| s.invalid.load(std::sync::atomic::Ordering::Acquire));
        assert!(invalid, "store should be marked invalid after drain");
    }
}

/// `handle::drain_for_connection` returns only the entries whose `connection_id` matches.
/// The full `close_for_connection` flow then funnels them through `drain_in_parallel`;
/// this test verifies the registry-side filter without sweeping the live registry's
/// other entries.
#[tokio::test]
async fn drain_for_connection_filter_only_returns_matching_entries() {
    let conn_7_a = build_connection_store("conn-7-a", 7).await;
    let conn_7_b = build_connection_store("conn-7-b", 7).await;
    let conn_8 = build_connection_store("conn-8", 8).await;
    let client = in_memory_for_tests("client").await;

    let h_a = lore::storage::handle::register(conn_7_a);
    let h_b = lore::storage::handle::register(conn_7_b);
    let h_8 = lore::storage::handle::register(conn_8);
    let h_client = lore::storage::handle::register(client);

    let drained = lore::storage::handle::drain_for_connection(7);
    let drained_ids: std::collections::HashSet<u64> = drained.iter().map(|(id, _)| *id).collect();
    assert!(drained_ids.contains(&h_a.handle_id));
    assert!(drained_ids.contains(&h_b.handle_id));
    assert!(!drained_ids.contains(&h_8.handle_id));
    assert!(!drained_ids.contains(&h_client.handle_id));
    assert!(lore::storage::handle::immutable_for_test(h_8).is_some());
    assert!(lore::storage::handle::immutable_for_test(h_client).is_some());

    for h in [h_8, h_client] {
        lore::storage::handle::unregister(h);
    }
}

/// Teardown closes the revision tree handles on the connection's storage handles.
/// Handles on another connection, or on none, are left registered.
#[tokio::test]
async fn close_for_connection_closes_the_revision_handles_loaded_on_it() {
    use lore::revision_tree::handle as tree_handle;
    use lore::revision_tree::handle::LoreRevisionTree;
    use lore::revision_tree::load::LoreRevisionTreeLoadArgs;
    use lore::revision_tree::load::load;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_revision::event::LoreEvent;
    use lore_revision::interface::LoreEventCallback;
    use lore_revision::interface::LoreGlobalArgs;

    async fn load_tree(
        store: lore::storage::handle::LoreStore,
        repository: Partition,
    ) -> LoreRevisionTree {
        let loaded: Arc<std::sync::Mutex<Option<u64>>> = Arc::new(std::sync::Mutex::new(None));
        let sink = loaded.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::RevisionTreeLoaded(data) = event {
                *sink.lock().unwrap() = Some(data.handle_id);
            }
        }));
        let status = load(
            LoreGlobalArgs::default(),
            LoreRevisionTreeLoadArgs {
                store,
                repository,
                revision_hash: Hash::default(),
            },
            callback,
        )
        .await;
        assert_eq!(status, 0, "loading the revision tree fixture must succeed");
        LoreRevisionTree {
            handle_id: loaded
                .lock()
                .unwrap()
                .expect("load must emit RevisionTreeLoaded"),
        }
    }

    const CONNECTION: u64 = 0x18A;

    let dropped =
        lore::storage::handle::register(build_connection_store("teardown", CONNECTION).await);
    let client = lore::storage::handle::register(in_memory_for_tests("teardown-client").await);
    let on_dropped = load_tree(dropped, Partition::from([0x1Au8; 16])).await;
    let on_client = load_tree(client, Partition::from([0x1Bu8; 16])).await;

    close_for_connection(CONNECTION).await;

    assert!(
        lore::storage::handle::lookup(dropped).is_none(),
        "the connection's storage handle must be closed",
    );
    assert!(
        tree_handle::lookup(on_dropped).is_none(),
        "the revision tree loaded on it must be closed with it",
    );
    assert!(
        tree_handle::lookup(on_client).is_some(),
        "a revision tree on a handle the connection did not own must survive",
    );

    tree_handle::unregister(on_client);
    lore::storage::handle::unregister(client);
}

async fn build_connection_store(identity: &str, connection_id: u64) -> Arc<StoreInternal> {
    let bare = in_memory_for_tests(identity).await;
    let immutable = bare.immutable.clone();
    let mutable = bare.mutable.clone();
    Arc::new(
        StoreInternal::new(
            identity,
            immutable,
            mutable,
            None,
            lore::storage::store::BoundFlags::default(),
            false,
            lore_storage::local::store_lock::StoreHold::default(),
        )
        .with_connection_id(connection_id),
    )
}
