// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Requests that reach a remote's storage. `RemoteImmutableStore` checks what the remote serves
//! and heals what fails the check. `RemoteMutableStore` reads and `repository::verify` pass on the
//! remote's refusal of the credentials. The session pools a remote store keeps per correlation id
//! are shared and replaced as `Pools` says, and a store wrapping a remote store holds them too.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_base::error::NotAuthenticated;
use lore_base::error::NotAuthorized;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_base::types::HealResult;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreString;
use lore_revision::repository::RemoteState;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::verify::VerifyFragmentArgs;
use lore_revision::repository::verify::verify_fragment_boxed;
use lore_revision::store::remote::Pools;
use lore_revision::store::remote::RemoteImmutableStore;
use lore_revision::store::remote::RemoteMutableStore;
use lore_revision::store::write_discarding::WriteDiscardingMutableStore;
use lore_revision::test_util::context_with_state;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_storage::StoreError;
use lore_transport::Connection;
use lore_transport::ProtocolError;
use lore_transport::SessionPool;
use lore_transport::StorageSession;

use super::serving::Serving;
use super::serving::ServingStorage;
use super::serving::remote_copy;
use super::serving::serving_remote;
use super::session_tests::under_execution_context;
use super::session_tests::under_globals;

/// A remote store whose connection for `source`'s partition reaches `remote`.
fn store_over(
    source: &Arc<RepositoryContext>,
    remote: &Arc<ServingStorage>,
) -> Arc<RemoteImmutableStore> {
    Arc::new(RemoteImmutableStore::with_connection(
        source.id,
        Connection::detached(vec![remote.clone()]),
    ))
}

/// A payload the remote serves altered is fetched again once the remote heals it, and served as the
/// heal restored it.
#[tokio::test]
async fn a_payload_failing_its_check_is_served_as_healed() {
    let content = b"content the remote serves altered until it heals it";
    let (source, address, _) = remote_copy(content, 0x71, 0).await;
    let remote = serving_remote(
        &source,
        Serving {
            tampered: Some(address),
            heals: true,
            ..Serving::default()
        },
    );
    let store = store_over(&source, &remote);

    let served = under_execution_context(|| store.clone().get(source.id, address))
        .await
        .expect("the healed payload is served");

    assert_eq!(served.payload.as_deref(), Some(&content[..]));
    assert_eq!((remote.gets(), remote.verifies()), (2, 1));
}

/// A store holds the session pool its reads share, so a command's reads over a connection out of
/// the connection cache, which pins no pool, still share one: two reads start one session.
#[tokio::test]
async fn reads_over_a_connection_out_of_the_cache_share_the_pool_the_store_holds() {
    let content = b"content read twice over a connection out of the cache";
    let (source, address, _) = remote_copy(content, 0x73, 0).await;
    let remote = serving_remote(&source, Serving::default());
    let connection = Connection::detached(vec![remote.clone()]);
    lore_transport::connection::remove_connection(connection.clone());
    let store = Arc::new(RemoteImmutableStore::with_connection(source.id, connection));

    under_execution_context(|| async {
        for _ in 0..2 {
            store
                .clone()
                .get(source.id, address)
                .await
                .expect("the payload is served");
        }
    })
    .await;

    assert_eq!(remote.starts(), 1, "each read started a pool of its own");
}

/// Reads `address` in `partition` from `store` under `correlation_id`.
async fn read_under(
    store: &Arc<RemoteImmutableStore>,
    partition: Partition,
    address: Address,
    correlation_id: &str,
) {
    let globals = LoreGlobalArgs {
        correlation_id: correlation_id.into(),
        ..LoreGlobalArgs::default()
    };
    under_globals(globals, || store.clone().get(partition, address))
        .await
        .expect("the payload is served");
}

/// Calls under correlation ids a hold names keep a pool each, which calls under the other leave in
/// place, where calls under correlation ids no hold names replace each other's.
#[tokio::test]
async fn held_correlation_ids_keep_a_pool_each() {
    let content = b"content read by two commands in turn";
    let (source, address, _) = remote_copy(content, 0x74, 0).await;
    let remote = serving_remote(&source, Serving::default());
    let store = store_over(&source, &remote);

    for correlation_id in ["unheld-1", "unheld-2", "unheld-1"] {
        read_under(&store, source.id, address, correlation_id).await;
    }
    let unheld = remote.starts();
    let _holds = (
        store.hold_correlation("held-1"),
        store.hold_correlation("held-2"),
    );
    for correlation_id in ["held-1", "held-2", "held-1", "held-2"] {
        read_under(&store, source.id, address, correlation_id).await;
    }

    assert_eq!(
        unheld, 3,
        "each read under another correlation id started a pool"
    );
    assert_eq!(
        remote.starts() - unheld,
        2,
        "one pool per held correlation id"
    );
}

/// A held correlation id's pool lives while any hold on the correlation id does, and stops its
/// sessions once the last hold drops.
#[tokio::test]
async fn a_held_pool_stops_its_sessions_once_its_last_hold_drops() {
    let content = b"content read by one command over two holds";
    let (source, address, _) = remote_copy(content, 0x75, 0).await;
    let remote = serving_remote(&source, Serving::default());
    let store = store_over(&source, &remote);
    let (first, second) = (
        store.hold_correlation("command"),
        store.hold_correlation("command"),
    );

    read_under(&store, source.id, address, "command").await;
    drop(first);
    read_under(&store, source.id, address, "command").await;
    assert_eq!(remote.starts(), 1, "the pool outlives one of its holds");
    assert_eq!(remote.stops(), 0);

    drop(second);
    let stopped = tokio::time::timeout(Duration::from_secs(10), async {
        while remote.stops() < 1 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(stopped.is_ok(), "the pool's session is stopped");
}

/// A live pool for calls under `correlation_id`, whose one session is never resolved: a test of
/// which pool a call takes asks nothing of its sessions.
fn unresolved_pool(correlation_id: &str) -> Arc<SessionPool> {
    let session = Arc::new(StorageSession::pending(|| async {
        Err(ProtocolError::internal(
            "a pooled session this test never resolves",
        ))
    }));
    Arc::new(SessionPool::new(Arc::from(correlation_id), vec![session]))
}

/// A pool kept for a held correlation id that went stale is replaced by the next pool kept, which
/// later calls share, and the stale one is held no longer.
#[tokio::test]
async fn a_held_pool_that_went_stale_is_replaced() {
    under_execution_context(|| async {
        let pools = Pools::default();
        let partition = Partition::from([0x7au8; 16]);
        let _hold = pools.hold("held");
        let stale = unresolved_pool("held");
        pools.keep(partition, "held", stale.clone());
        StorageSession::pooled(stale.clone()).invalidate().await;
        let replacement = unresolved_pool("held");

        let kept = pools.keep(partition, "held", replacement.clone());

        assert!(Arc::ptr_eq(&kept, &replacement));
        assert!(
            pools
                .get_live(partition, "held")
                .is_some_and(|live| Arc::ptr_eq(&live, &replacement)),
            "later calls share the replacement"
        );
        assert_eq!(
            Arc::strong_count(&stale),
            1,
            "the stale pool is held no longer"
        );
    })
    .await;
}

/// A pool kept for a held correlation id whose connection went stale is replaced by the next pool
/// kept, which later calls share, and the one on the stale connection is held no longer.
#[tokio::test]
async fn a_held_pool_whose_connection_went_stale_is_replaced() {
    let source = context_with_state(RemoteState::Offline).await;
    let connection = Connection::detached(vec![serving_remote(&source, Serving::default())]);
    under_execution_context(|| async {
        let pools = Pools::default();
        let _hold = pools.hold("held");
        let on_stale = connection
            .unpinned_session_pool(source.id, "held")
            .await
            .expect("a pool over the connection");
        pools.keep(source.id, "held", on_stale.clone());
        connection.stale.store(true, Ordering::Relaxed);
        let replacement = unresolved_pool("held");

        let kept = pools.keep(source.id, "held", replacement.clone());

        assert!(Arc::ptr_eq(&kept, &replacement));
        assert!(
            pools
                .get_live(source.id, "held")
                .is_some_and(|live| Arc::ptr_eq(&live, &replacement)),
            "later calls share the replacement"
        );
        assert_eq!(
            Arc::strong_count(&on_stale),
            1,
            "the pool on the stale connection is held no longer"
        );
    })
    .await;
}

/// Calls under a held correlation id that each started a pool for one partition, having found none
/// to share, all take the pool the first of them kept, and the pool another started is not kept.
#[test]
fn calls_under_a_held_correlation_id_take_the_pool_kept_first() {
    let pools = Pools::default();
    let partition = Partition::from([0x7bu8; 16]);
    let _hold = pools.hold("held");
    let (first, second) = (unresolved_pool("held"), unresolved_pool("held"));

    let kept = [first.clone(), second.clone()].map(|pool| pools.keep(partition, "held", pool));

    assert!(
        kept.iter().all(|kept| Arc::ptr_eq(kept, &first)),
        "a call took a pool other than the one kept first"
    );
    assert_eq!(
        Arc::strong_count(&second),
        1,
        "the pool started second is not kept"
    );
}

/// A store that trusts its remote serves what the remote serves with only its sizes checked: it
/// neither expands nor hashes the payload, so it asks no heal of a payload the remote altered.
#[tokio::test]
async fn a_trusting_store_serves_the_payload_unchecked() {
    let content = b"content the remote serves altered to a store that trusts it";
    let (source, address, _) = remote_copy(content, 0x76, 0).await;
    let remote = serving_remote(
        &source,
        Serving {
            tampered: Some(address),
            heals: true,
            ..Serving::default()
        },
    );
    let store = Arc::new(
        RemoteImmutableStore::with_connection(
            source.id,
            Connection::detached(vec![remote.clone()]),
        )
        .verifying(false),
    );

    let served = under_execution_context(|| store.clone().get(source.id, address))
        .await
        .expect("the payload is served unchecked");

    assert_ne!(served.payload.as_deref(), Some(&content[..]));
    assert!(served.content.is_none());
    assert_eq!((remote.gets(), remote.verifies()), (1, 0));
}

/// A store that trusts its remote still checks the sizes of what the remote serves: a payload whose
/// sizes disagree with its fragment is healed, and refused where it still disagrees after that one
/// heal.
#[tokio::test]
async fn a_trusting_store_heals_a_payload_whose_sizes_disagree_and_then_refuses_it() {
    let content = b"content the remote serves a byte short through its heal";
    let (source, address, _) = remote_copy(content, 0x78, 0).await;
    let remote = serving_remote(
        &source,
        Serving {
            tampered: Some(address),
            shortened: true,
            heals: true,
            heal_fails: true,
            ..Serving::default()
        },
    );
    let store = Arc::new(
        RemoteImmutableStore::with_connection(
            source.id,
            Connection::detached(vec![remote.clone()]),
        )
        .verifying(false),
    );

    let served = under_execution_context(|| store.clone().get(source.id, address)).await;

    assert!(
        served.as_ref().is_err_and(|err| err.is_internal()),
        "got {served:?}"
    );
    assert_eq!((remote.gets(), remote.verifies()), (2, 1));
}

/// A heal asks the remote to check its copy and repair it, and answers what the remote did.
#[tokio::test]
async fn a_heal_asks_the_remote_to_repair_its_copy() {
    let content = b"content the remote holds altered until it is asked to heal";
    let (source, address, _) = remote_copy(content, 0x77, 0).await;
    let remote = serving_remote(
        &source,
        Serving {
            tampered: Some(address),
            heals: true,
            ..Serving::default()
        },
    );
    let store = store_over(&source, &remote);

    let healed = under_execution_context(|| store.clone().heal(source.id, address)).await;

    assert!(matches!(healed, Ok(HealResult::Healed)), "{healed:?}");
    assert_eq!(remote.verifies(), 1);
}

/// A heal of an address the remote holds no copy of is not attempted, and is not a failure.
#[tokio::test]
async fn a_heal_of_an_address_the_remote_lacks_is_not_attempted() {
    let (source, address, _) = remote_copy(b"content the remote holds", 0x79, 0).await;
    let missing = Address {
        hash: lore_storage::hash::hash_slice(b"content the remote lacks and is asked to heal"),
        ..address
    };
    let remote = serving_remote(&source, Serving::default());
    let store = store_over(&source, &remote);

    let healed = under_execution_context(|| store.clone().heal(source.id, missing)).await;

    assert!(matches!(healed, Ok(HealResult::NotAttempted)), "{healed:?}");
    assert_eq!(remote.verifies(), 1);
}

/// A payload still altered after a heal the remote reports as made is refused, after that one
/// heal.
#[tokio::test]
async fn a_payload_still_failing_after_its_heal_is_refused() {
    let content = b"content the remote serves altered through its heal";
    let (source, address, _) = remote_copy(content, 0x72, 0).await;
    let remote = serving_remote(
        &source,
        Serving {
            tampered: Some(address),
            heals: true,
            heal_fails: true,
            ..Serving::default()
        },
    );
    let store = store_over(&source, &remote);

    let served = under_execution_context(|| store.clone().get(source.id, address)).await;

    assert!(
        served.as_ref().is_err_and(|err| err.is_internal()),
        "got {served:?}"
    );
    assert_eq!((remote.gets(), remote.verifies()), (2, 1));
}

/// A payload in a codec this build cannot decode, one no build assigns, is refused as
/// `NotSupported`, and no heal is asked for it: it is not corrupt.
#[tokio::test]
async fn an_undecodable_payload_asks_no_heal() {
    use lore_base::types::FragmentFlags;
    let content = b"content stored in a codec this build cannot decode";
    let unassigned = FragmentFlags::PayloadCompressed
        & !(FragmentFlags::PayloadCompressedLZ4
            | FragmentFlags::PayloadCompressedOodle2
            | FragmentFlags::PayloadCompressedZstd);
    let (source, address, _) = lore_storage::test_util::accepting_unvalidated_puts(remote_copy(
        content,
        0x73,
        unassigned.bits(),
    ))
    .await;
    let remote = serving_remote(&source, Serving::default());
    let store = store_over(&source, &remote);

    let served = under_execution_context(|| store.clone().get(source.id, address)).await;

    assert!(
        served.as_ref().is_err_and(|err| err.is_not_supported()),
        "got {served:?}"
    );
    assert_eq!((remote.gets(), remote.verifies()), (1, 0));
}

/// A get the remote refuses as unsupported fails as an internal error, as a remote the store cannot
/// reach does, not as `NotSupported`, which is a payload this build cannot decode, and asks no heal.
#[tokio::test]
async fn a_get_the_remote_does_not_support_is_an_internal_failure() {
    let content = b"content behind a remote that supports no get";
    let (source, address, _) = remote_copy(content, 0x74, 0).await;
    let remote = serving_remote(
        &source,
        Serving {
            unsupported: true,
            ..Serving::default()
        },
    );
    let store = store_over(&source, &remote);

    let served = under_execution_context(|| store.clone().get(source.id, address)).await;

    assert!(
        served.as_ref().is_err_and(|err| err.is_internal()),
        "got {served:?}"
    );
    assert_eq!((remote.gets(), remote.verifies()), (1, 0));
}

/// An address the remote lacks is answered `AddressNotFound`, which a server answers as not found,
/// and not as a failure of the store. It carries the remote's answer in its trace.
#[tokio::test]
async fn an_address_the_remote_lacks_is_not_found() {
    let (source, address, _) = remote_copy(b"content the remote holds", 0x75, 0).await;
    let missing = Address {
        hash: lore_storage::hash::hash_slice(b"content the remote lacks"),
        ..address
    };
    let remote = serving_remote(&source, Serving::default());
    let store = store_over(&source, &remote);

    let served = under_execution_context(|| store.clone().get(source.id, missing)).await;

    let Err(StoreError::AddressNotFound(miss)) = &served else {
        panic!("got {served:?}");
    };
    assert!(
        miss.trace()
            .locations()
            .last()
            .is_some_and(|chained| chained.context().is_some()),
        "the miss chains the remote's answer: {:?}",
        miss.trace()
    );
    assert_eq!((remote.gets(), remote.verifies()), (1, 0));
}

/// A get the remote refuses fails with the refusal, not as a miss.
#[tokio::test]
async fn a_get_the_remote_refuses_fails_with_the_refusal() {
    let (source, address, _) = remote_copy(b"content behind a refusing remote", 0x76, 0).await;
    let remote = serving_remote(
        &source,
        Serving {
            refusal: Some(ProtocolError::from(NotAuthorized)),
            ..Serving::default()
        },
    );
    let store = store_over(&source, &remote);

    let served = under_execution_context(|| store.clone().get(source.id, address)).await;

    assert!(
        served.as_ref().is_err_and(|err| err.is_not_authorized()),
        "got {served:?}"
    );
}

/// A remote answering every get, verify and mutable load with `refusal`, and the context it serves
/// from.
async fn refusing_remote(refusal: ProtocolError) -> (Arc<RepositoryContext>, Arc<ServingStorage>) {
    let source = context_with_state(RemoteState::Offline).await;
    let remote = serving_remote(
        &source,
        Serving {
            refusal: Some(refusal),
            ..Serving::default()
        },
    );
    (source, remote)
}

/// A mutable-store read the remote refuses fails with the refusal, as a write does.
#[tokio::test]
async fn a_mutable_load_the_remote_refuses_fails_with_the_refusal() {
    let (source, remote) = refusing_remote(ProtocolError::from(NotAuthorized)).await;
    let store = Arc::new(RemoteMutableStore::with_connection(
        source.id,
        Connection::detached(vec![remote]),
    ));

    let loaded = under_execution_context(|| {
        store
            .clone()
            .load(source.id, Hash::from([0x75; 32]), KeyType::Resolve)
    })
    .await;

    assert!(
        loaded.as_ref().is_err_and(|err| err.is_not_authorized()),
        "got {loaded:?}"
    );
}

/// A write-discarding store holds the pools of the remote store it wraps for a correlation id, so
/// loads under two held correlation ids in turn start one pool each.
#[tokio::test]
async fn a_write_discarding_store_holds_the_pools_of_the_remote_it_wraps() {
    let source = context_with_state(RemoteState::Offline).await;
    let remote = serving_remote(&source, Serving::default());
    let store = Arc::new(WriteDiscardingMutableStore::new(Arc::new(
        RemoteMutableStore::with_connection(source.id, Connection::detached(vec![remote.clone()])),
    )));
    let _holds = (
        store.hold_correlation("held-1"),
        store.hold_correlation("held-2"),
    );

    for correlation_id in ["held-1", "held-2", "held-1", "held-2"] {
        let globals = LoreGlobalArgs {
            correlation_id: correlation_id.into(),
            ..LoreGlobalArgs::default()
        };
        under_globals(globals, || {
            store
                .clone()
                .load(source.id, Hash::from([0x77; 32]), KeyType::Resolve)
        })
        .await
        .expect_err("the remote holds no mutable keys");
    }

    assert_eq!(remote.starts(), 2, "one pool per held correlation id");
}

/// A fragment verification the remote refuses fails with the refusal.
#[tokio::test]
async fn a_verification_the_remote_refuses_fails_with_the_refusal() {
    let (_, remote) = refusing_remote(ProtocolError::from(NotAuthenticated)).await;
    let repository = context_with_state(RemoteState::Connected(Connection::detached(vec![
        remote.clone(),
    ])))
    .await;
    let args = VerifyFragmentArgs {
        hash: LoreString::from(Hash::from([0x76; 32]).to_string()),
        context: LoreString::default(),
        heal: false,
    };

    let verified = under_execution_context(|| verify_fragment_boxed(repository, args)).await;

    assert!(
        verified
            .as_ref()
            .is_err_and(|err| err.is_not_authenticated()),
        "got {verified:?}"
    );
    assert_eq!(remote.verifies(), 1);
}
