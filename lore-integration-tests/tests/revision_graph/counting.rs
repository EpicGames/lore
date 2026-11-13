// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A storage handle over the fixture handle's local stores that counts the calls a call makes on
//! them, so a test can tell what a verb reads and writes in the local store, and can fail the
//! local store's payload reads.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use lore::storage::handle;
use lore::storage::handle::LoreStore;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_revision::protocol::RemoteEndpoint;
use lore_storage::StoreError;
use lore_storage::fragment_flags::FragmentFlags;
use lore_storage::hash;
use lore_storage::test_util::HookedImmutableStore;
use lore_storage::test_util::HookedStore;
use lore_storage::test_util::ImmutableCall;
use lore_storage::test_util::StoreCall;

use super::support::Fixture;
use super::support::fixture;
use super::support::handle_state;
use super::support::in_client;
use super::support::register_handle;
use crate::common::net_common::close_handle;

/// The calls a counted handle's mutable store saw: each load, store, compare-and-swap and list,
/// with the key and key type it names, a zero key for a list.
#[derive(Default)]
pub(crate) struct CountingMutable {
    calls: Mutex<Vec<(StoreCall, Hash, KeyType)>>,
}

impl CountingMutable {
    /// The calls of kind `call` counted so far, on keys of any type.
    pub(crate) fn count(&self, call: StoreCall) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(seen, _, _)| *seen == call)
            .count()
    }

    /// The calls of kind `call` on keys of type `key_type` counted so far.
    pub(crate) fn count_for(&self, call: StoreCall, key_type: KeyType) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(seen, _, seen_type)| *seen == call && *seen_type == key_type)
            .count()
    }

    /// The calls of every kind counted so far.
    pub(crate) fn total(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    /// The key and key type of each store and compare-and-swap counted so far, in order.
    pub(crate) fn written(&self) -> Vec<(Hash, KeyType)> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(call, _, _)| matches!(call, StoreCall::Store | StoreCall::CompareAndSwap))
            .map(|(_, key, key_type)| (*key, *key_type))
            .collect()
    }

    fn record(&self, call: StoreCall, key: Hash, key_type: KeyType) {
        self.calls.lock().unwrap().push((call, key, key_type));
    }
}

/// The content calls a counted handle's immutable store saw, and the switch that fails its payload
/// reads.
#[derive(Default)]
pub(crate) struct CountingImmutable {
    calls: Mutex<Vec<ImmutableCall>>,
    fail_gets: AtomicBool,
}

impl CountingImmutable {
    /// The calls of kind `call` counted so far, payload reads [`fail_gets`](Self::fail_gets) failed
    /// included.
    pub(crate) fn count(&self, call: ImmutableCall) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|seen| **seen == call)
            .count()
    }

    /// The content calls of every kind counted so far.
    pub(crate) fn total(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    /// Makes every payload read from now on fail with `Internal`, as a failing disk read does.
    pub(crate) fn fail_gets(&self) {
        self.fail_gets.store(true, Ordering::SeqCst);
    }

    fn record(&self, call: ImmutableCall) -> Result<(), StoreError> {
        self.calls.lock().unwrap().push(call);
        if call == ImmutableCall::Get && self.fail_gets.load(Ordering::SeqCst) {
            return Err(StoreError::internal("the test fails payload reads"));
        }
        Ok(())
    }
}

/// A storage handle over the local stores of `f`'s handle, with `remote` as its remote, or the
/// remote of `f`'s handle where it is `None`, and the counts of the calls it makes on each store.
/// It is registered with [`register_handle`]; a test closes it with `close_handle` before
/// [`Fixture::close`].
pub(crate) fn open_counted_handle(
    f: &Fixture,
    remote: Option<Arc<RemoteEndpoint>>,
) -> (LoreStore, Arc<CountingMutable>, Arc<CountingImmutable>) {
    let state = handle_state(f);
    let mutable = Arc::new(CountingMutable::default());
    let immutable = Arc::new(CountingImmutable::default());
    let counted_mutable = mutable.clone();
    let counted_immutable = immutable.clone();
    let handle = register_handle(
        f,
        HookedImmutableStore::wrap(state.immutable.clone(), move |call, _, _| {
            counted_immutable.record(call)
        }),
        HookedStore::wrap(
            state.mutable.clone(),
            move |call, key, key_type| {
                counted_mutable.record(call, key, key_type);
                Ok(())
            },
            None,
        ),
        remote.or_else(|| state.remote.clone()),
    );
    (handle, mutable, immutable)
}

/// A counted handle counts each call on its stores by its kind and key type, and keeps the key and
/// key type of each store and compare-and-swap in order. It counts each payload read and put, a
/// read it fails included, and keeps the remote of the fixture's handle where it is given none.
#[tokio::test]
async fn a_counted_handle_counts_each_call_on_its_stores() {
    let f = fixture().await;
    let (counted, mutable, immutable) = open_counted_handle(&f, None);
    let state = handle::lookup(counted).expect("the counted handle is open");
    let [stored, swapped] = [1, 2].map(|byte| Hash::from([byte; 32]));
    let payload = Bytes::from_static(b"counted payload");
    let address = Address {
        hash: hash::hash_slice(&payload),
        context: Context::default(),
    };
    let fragment = Fragment {
        flags: FragmentFlags::PayloadStoredLocal.bits(),
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    };

    let failed_read = in_client(async {
        let mutable = state.mutable.clone();
        mutable
            .clone()
            .store(f.repository, stored, Hash::from([3; 32]), KeyType::Untyped)
            .await
            .expect("stores a key");
        mutable
            .clone()
            .compare_and_swap(
                f.repository,
                swapped,
                Hash::default(),
                Hash::from([4; 32]),
                KeyType::BranchMetadata,
            )
            .await
            .expect("swaps a key in");
        mutable
            .clone()
            .load(f.repository, stored, KeyType::Untyped)
            .await
            .expect("loads a key");
        mutable
            .list(f.repository, KeyType::Instance)
            .await
            .expect("lists keys");
        let immutable_store = state.immutable.clone();
        immutable_store
            .clone()
            .put(f.repository, address, fragment, Some(payload), false)
            .await
            .expect("puts a payload");
        immutable_store
            .clone()
            .get(f.repository, address)
            .await
            .expect("reads the payload");
        immutable.fail_gets();
        immutable_store.get(f.repository, address).await
    })
    .await;
    let fixture_remote = handle::lookup(f.store).and_then(|fixture| fixture.remote.clone());
    let kept_remote = state
        .remote
        .as_ref()
        .zip(fixture_remote.as_ref())
        .is_some_and(|(kept, fixture)| Arc::ptr_eq(kept, fixture));
    drop(state);
    close_handle(counted).await;

    assert_eq!(
        [
            StoreCall::Load,
            StoreCall::Store,
            StoreCall::CompareAndSwap,
            StoreCall::List
        ]
        .map(|call| mutable.count(call)),
        [1; 4]
    );
    assert_eq!(
        [
            mutable.count_for(StoreCall::CompareAndSwap, KeyType::BranchMetadata),
            mutable.count_for(StoreCall::List, KeyType::Instance),
            mutable.count_for(StoreCall::Load, KeyType::BranchMetadata),
        ],
        [1, 1, 0]
    );
    assert_eq!(
        mutable.written(),
        [
            (stored, KeyType::Untyped),
            (swapped, KeyType::BranchMetadata)
        ]
    );
    assert_eq!(
        [ImmutableCall::Put, ImmutableCall::Get].map(|call| immutable.count(call)),
        [1, 2]
    );
    assert_eq!(immutable.total(), 3);
    assert!(
        matches!(failed_read, Err(StoreError::Internal(_))),
        "expected Internal, got {:?}",
        failed_read.map(|_| ())
    );
    assert!(kept_remote, "the counted handle keeps the fixture's remote");
    f.close().await;
}
