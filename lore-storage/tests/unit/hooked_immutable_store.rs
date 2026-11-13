// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The hooked immutable store: the calls it shows its hook, and what it forwards to the store it
//! wraps.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use lore_base::test_util::TempDir;
use lore_base::types::HealResult;
use lore_storage::Address;
use lore_storage::CallerBuffer;
use lore_storage::Context;
use lore_storage::Fragment;
use lore_storage::Partition;
use lore_storage::errors::Disconnected;
use lore_storage::fragment_flags::FragmentFlags;
use lore_storage::hash;
use lore_storage::immutable_store::CopyBehavior;
use lore_storage::immutable_store::ImmutableStore;
use lore_storage::immutable_store::StoreError;
use lore_storage::immutable_store::query_one;
use lore_storage::local::immutable_store::ImmutableStoreSettings;
use lore_storage::local::immutable_store::LocalImmutableStore;
use lore_storage::store_types::PayloadRead;
use lore_storage::store_types::StoreMatch;
use lore_storage::store_types::StoreMatchResult;
use lore_storage::test_util::HookedImmutableStore;
use lore_storage::test_util::ImmutableCall;

/// A call a hook saw: the call, the partition and the address.
type Seen = (ImmutableCall, Partition, Address);

/// A store on disk under the returned directory, named with `prefix`, that isolates partitions, so
/// that its answers differ from the trait's defaults.
async fn isolating_store(prefix: &str) -> (TempDir, Arc<dyn ImmutableStore>) {
    let dir = TempDir::new(prefix);
    let store = LocalImmutableStore::new(
        Some(PathBuf::from(dir.as_ref())),
        ImmutableStoreSettings {
            isolate_partitions: true,
            ..Default::default()
        },
    )
    .await
    .expect("creates the store");
    (dir, store)
}

/// `store` wrapped, with each call its hook sees kept in the returned list.
fn recording(store: &Arc<dyn ImmutableStore>) -> (Arc<dyn ImmutableStore>, Arc<Mutex<Vec<Seen>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let kept = seen.clone();
    let hooked = HookedImmutableStore::wrap(store.clone(), move |call, partition, address| {
        kept.lock().unwrap().push((call, partition, address));
        Ok(())
    });
    (hooked, seen)
}

/// The address and fragment of `payload`, stored whole and uncompressed under `context`.
fn whole(payload: &[u8], context: Context) -> (Address, Fragment) {
    let address = Address {
        hash: hash::hash_slice(payload),
        context,
    };
    let fragment = Fragment {
        flags: FragmentFlags::PayloadStoredLocal.bits(),
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    };
    (address, fragment)
}

/// The level at which `store` holds `address` in `partition`.
async fn held(
    store: &Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
) -> StoreMatch {
    query_one(store, partition, address)
        .await
        .expect("the store answers")
        .match_made
}

/// Each content call reaches the wrapped store, after the hook sees it with its partition and
/// address: a read through `get` or `get_into` as a get, a heal, a query once per address, and a
/// copy with its source.
#[tokio::test]
async fn the_hook_sees_each_content_call_the_store_then_answers() {
    let (_dir, store) = isolating_store("lore-storage-hooked-immutable-sees-each-call-").await;
    let (hooked, seen) = recording(&store);
    let partition = Partition::from([1; 16]);
    let destination = Partition::from([2; 16]);
    let payload = Bytes::from_static(b"hooked immutable store payload");
    let (address, fragment) = whole(&payload, Context::from([3; 16]));
    let absent = whole(b"never stored", Context::default()).0;

    hooked
        .clone()
        .put(partition, address, fragment, Some(payload.clone()), false)
        .await
        .expect("stores the payload");
    let read = hooked
        .clone()
        .get(partition, address)
        .await
        .expect("reads the payload")
        .into_payload()
        .expect("the read carries the payload")
        .1;
    let mut buffer = vec![0u8; payload.len()];
    // SAFETY: the buffer outlives the read and nothing else touches it.
    let mut dst = unsafe { CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
    let (_, placed) = hooked
        .clone()
        .get_into(partition, address, &mut dst)
        .await
        .expect("reads the payload into the buffer");
    let described = hooked
        .clone()
        .get_metadata(partition, address)
        .await
        .expect("describes the payload");
    let healed = hooked
        .clone()
        .heal(partition, address)
        .await
        .expect("checks the payload");
    let mut results = [StoreMatchResult::default(); 2];
    hooked
        .clone()
        .query(partition, &[address, absent], &mut results)
        .await
        .expect("answers the query");
    hooked
        .clone()
        .copy(
            partition,
            address,
            destination,
            address.context,
            CopyBehavior::default(),
        )
        .await
        .expect("copies the payload");
    let copied = held(&store, destination, address).await;
    hooked
        .clone()
        .obliterate(partition, address, Arc::default())
        .await
        .expect("obliterates the payload");

    assert_eq!(read, payload);
    assert!(matches!(placed, PayloadRead::IntoBuffer));
    assert_eq!(buffer, payload);
    assert_eq!(described.fragment.size_content, fragment.size_content);
    assert_eq!(healed, HealResult::NotAttempted);
    assert_eq!(
        results.map(|result| result.match_made),
        [StoreMatch::MatchFull, StoreMatch::MatchNone]
    );
    assert_eq!(copied, StoreMatch::MatchFull);
    assert_eq!(
        held(&store, partition, address).await,
        StoreMatch::MatchNone
    );
    assert_eq!(
        *seen.lock().unwrap(),
        [
            (ImmutableCall::Put, partition, address),
            (ImmutableCall::Get, partition, address),
            (ImmutableCall::Get, partition, address),
            (ImmutableCall::GetMetadata, partition, address),
            (ImmutableCall::Heal, partition, address),
            (ImmutableCall::Query, partition, address),
            (ImmutableCall::Query, partition, absent),
            (ImmutableCall::Copy, partition, address),
            (ImmutableCall::Obliterate, partition, address),
        ]
    );
}

/// A call the hook fails returns the hook's error and never reaches the wrapped store: a failed put
/// stores nothing, and a failed read of a payload the store holds reaches no store.
#[tokio::test]
async fn a_call_the_hook_fails_never_reaches_the_store() {
    let (_dir, store) = isolating_store("lore-storage-hooked-immutable-failed-call-").await;
    let (observed, reached) = recording(&store);
    let hooked = HookedImmutableStore::wrap(observed, |call, _, _| match call {
        ImmutableCall::Put | ImmutableCall::Get => Err(StoreError::from(Disconnected)),
        _ => Ok(()),
    });
    let partition = Partition::from([4; 16]);
    let payload = Bytes::from_static(b"refused by the hook");
    let (address, fragment) = whole(&payload, Context::from([5; 16]));

    let refused_put = hooked
        .clone()
        .put(partition, address, fragment, Some(payload.clone()), false)
        .await;
    let unstored = held(&store, partition, address).await;
    store
        .clone()
        .put(partition, address, fragment, Some(payload), false)
        .await
        .expect("stores the payload in the wrapped store");
    let refused_get = hooked.clone().get(partition, address).await;

    assert!(
        matches!(refused_put, Err(StoreError::Disconnected(_))),
        "expected the hook's Disconnected, got {refused_put:?}"
    );
    assert_eq!(unstored, StoreMatch::MatchNone);
    assert!(
        matches!(refused_get, Err(StoreError::Disconnected(_))),
        "expected the hook's Disconnected, got {:?}",
        refused_get.map(|_| ())
    );
    let reached = reached.lock().unwrap();
    assert!(reached.is_empty(), "the refused calls reached {reached:?}");
}

/// The store answers what the store it wraps answers where that differs from the trait's
/// defaults: it is local, isolates partitions and searches as narrowly as that asks, has the
/// wrapped store's lock and claims, and counts its fragments.
#[tokio::test]
async fn the_store_answers_as_the_store_it_wraps() {
    let (_dir, store) = isolating_store("lore-storage-hooked-immutable-answers-as-wrapped-").await;
    let (hooked, _) = recording(&store);
    let payload = b"counted fragment";
    let (address, fragment) = whole(payload, Context::from([6; 16]));
    store
        .clone()
        .put(
            Partition::from([7; 16]),
            address,
            fragment,
            Some(Bytes::from_static(payload)),
            false,
        )
        .await
        .expect("stores a payload");

    let claim = hooked
        .clone()
        .hold_for_command()
        .await
        .expect("claims the store");
    let joined = hooked.try_join_claim();

    assert!(hooked.is_local());
    assert!(hooked.isolates_partitions());
    assert_eq!(
        (hooked.read_scope(), hooked.query_scope()),
        (StoreMatch::MatchFull, StoreMatch::MatchPartition)
    );
    assert_eq!(hooked.max_query_batch(), store.max_query_batch());
    assert!(
        hooked
            .store_lock()
            .zip(store.store_lock())
            .is_some_and(|(hooked, wrapped)| Arc::ptr_eq(&hooked, &wrapped)),
        "the store has the wrapped store's lock"
    );
    assert!(claim.is_some(), "claims the wrapped store's lock");
    assert!(
        joined.is_some(),
        "joins the claim held on the wrapped store"
    );
    assert_eq!(hooked.clone().fragment_count().await, Some(1));
}
