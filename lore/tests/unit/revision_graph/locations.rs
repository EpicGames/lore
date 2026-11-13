// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use bytes::Bytes;
use lore::interface::LoreGlobalArgs;
use lore::revision_graph::RevisionGraphError;
use lore::revision_graph::locations::Locations;
use lore::revision_graph::locations::full_context;
use lore::revision_graph::locations::local_read_context;
use lore::revision_graph::locations::local_write_context;
use lore::revision_graph::locations::locations;
use lore::revision_graph::locations::remote_read_context;
use lore::storage::store::BoundFlags;
use lore::storage::store::PerCallFlags;
use lore::storage::store::StoreInternal;
use lore_base::test_util::closed_port_url;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_revision::immutable::read_options_from_repository;
use lore_revision::immutable::store_raw_store_retry;
use lore_revision::metadata::Metadata;
use lore_revision::metadata::MetadataError;
use lore_revision::protocol::RemoteConnections;
use lore_revision::protocol::RemoteEndpoint;
use lore_revision::repository::RemoteStatus;
use lore_revision::repository::RepositoryContext;
use lore_storage::test_util::accepting_unvalidated_puts;
use lore_transport::Connection;

use crate::storage::in_memory_store;
use crate::storage::under_globals;

fn repository() -> Partition {
    Partition::from([0x6Cu8; 16])
}

/// An endpoint at a loopback port that refuses connections.
fn closed_port_endpoint() -> Arc<RemoteEndpoint> {
    Arc::new(RemoteEndpoint::new(closed_port_url("grpc"), None))
}

/// An endpoint already connected for [`repository`], to no remote.
fn connected_endpoint() -> Arc<RemoteEndpoint> {
    Arc::new(RemoteEndpoint::over(RemoteConnections::with_connection(
        repository(),
        Connection::detached(Vec::new()),
    )))
}

/// The per-call flags a verb reads from globals carrying `offline`, `local` and `remote`.
fn per_call(offline: u8, local: u8, remote: u8) -> PerCallFlags {
    PerCallFlags::from_globals(&LoreGlobalArgs {
        offline,
        local,
        remote,
        ..Default::default()
    })
}

/// Asserts that `found` consults `remote`, the same endpoint or none, and the local store when
/// `local` is set.
fn assert_locations(
    found: Result<Locations, RevisionGraphError>,
    remote: Option<&Arc<RemoteEndpoint>>,
    local: bool,
) {
    let found = found.unwrap_or_else(|err| panic!("locations failed: {err}"));
    assert_eq!(
        found.remote().map(Arc::as_ptr),
        remote.map(Arc::as_ptr),
        "the remote the call consults"
    );
    assert_eq!(
        found.local(),
        local,
        "whether the call consults the local store"
    );
}

async fn is_offline(context: &RepositoryContext) -> bool {
    matches!(context.remote_status().await, RemoteStatus::Offline)
}

async fn is_connected(context: &RepositoryContext) -> bool {
    matches!(context.remote_status().await, RemoteStatus::Connected(_))
}

fn holds_endpoint(context: &RepositoryContext, endpoint: &Arc<RemoteEndpoint>) -> bool {
    context
        .endpoint
        .as_ref()
        .is_some_and(|held| Arc::ptr_eq(held, endpoint))
}

/// Stores at `hash`, in the local store of `store`, a payload whose sizes pass their check and
/// which doesn't decode.
async fn store_undecodable(store: &StoreInternal, hash: Hash) {
    let fragment = Fragment {
        flags: FragmentFlags::PayloadCompressedLZ4.bits(),
        size_payload: 64,
        size_content: 4096,
    };
    let address = Address {
        hash,
        context: Context::default(),
    };
    accepting_unvalidated_puts(store_raw_store_retry(
        store.immutable.clone(),
        repository(),
        address,
        fragment,
        Some(Bytes::from(vec![0xff; 64])),
    ))
    .await
    .expect("stores the payload");
}

#[tokio::test]
async fn no_flag_on_a_local_handle_is_local_only() {
    let store = in_memory_store(None, BoundFlags::default()).await;

    assert_locations(locations(&store, per_call(0, 0, 0)), None, true);
}

#[tokio::test]
async fn remote_flag_on_a_local_handle_fails_no_remote() {
    let store = in_memory_store(None, BoundFlags::default()).await;

    let found = locations(&store, per_call(0, 0, 1));

    assert!(
        found.as_ref().is_err_and(RevisionGraphError::is_no_remote),
        "expected NoRemote, got {:?}",
        found.err()
    );
}

#[tokio::test]
async fn local_and_remote_together_fail_invalid_arguments() {
    let store = in_memory_store(Some(closed_port_endpoint()), BoundFlags::default()).await;

    let found = locations(&store, per_call(0, 1, 1));

    assert!(
        found
            .as_ref()
            .is_err_and(RevisionGraphError::is_invalid_arguments),
        "expected InvalidArguments, got {:?}",
        found.err()
    );
}

/// `offline` or `local`, per call or bound at open, keeps the call to the local store on a handle
/// with a remote.
#[tokio::test]
async fn offline_flag_is_local_only() {
    let opened_offline = BoundFlags {
        offline: true,
        ..Default::default()
    };
    let opened_local = BoundFlags {
        local: true,
        ..Default::default()
    };
    for (bound, flags) in [
        (BoundFlags::default(), per_call(1, 0, 0)),
        (BoundFlags::default(), per_call(0, 1, 0)),
        (opened_offline, per_call(0, 0, 0)),
        (opened_local, per_call(0, 0, 0)),
    ] {
        let store = in_memory_store(Some(closed_port_endpoint()), bound).await;

        assert_locations(locations(&store, flags), None, true);
    }
}

/// `remote`, per call or bound at open, keeps the call to the remote.
#[tokio::test]
async fn remote_flag_on_a_handle_with_a_remote_is_remote_only() {
    let opened_remote = BoundFlags {
        remote: true,
        ..Default::default()
    };
    for (bound, flags) in [
        (BoundFlags::default(), per_call(0, 0, 1)),
        (opened_remote, per_call(0, 0, 0)),
    ] {
        let endpoint = closed_port_endpoint();
        let store = in_memory_store(Some(endpoint.clone()), bound).await;

        assert_locations(locations(&store, flags), Some(&endpoint), false);
    }
}

#[tokio::test]
async fn a_per_call_remote_on_a_handle_opened_local_fails_invalid_arguments() {
    let opened_local = BoundFlags {
        local: true,
        ..Default::default()
    };
    let store = in_memory_store(Some(closed_port_endpoint()), opened_local).await;

    let found = locations(&store, per_call(0, 0, 1));

    assert!(
        found
            .as_ref()
            .is_err_and(RevisionGraphError::is_invalid_arguments),
        "expected InvalidArguments, got {:?}",
        found.err()
    );
}

#[tokio::test]
async fn an_unreachable_remote_does_not_fail_locations() {
    let endpoint = closed_port_endpoint();
    let store = in_memory_store(Some(endpoint.clone()), BoundFlags::default()).await;

    assert_locations(locations(&store, per_call(0, 0, 0)), Some(&endpoint), true);
}

#[tokio::test]
async fn remote_read_context_returns_the_connect_error_on_a_closed_port() {
    let endpoint = closed_port_endpoint();
    let store = in_memory_store(Some(endpoint.clone()), BoundFlags::default()).await;

    let context = under_globals(
        LoreGlobalArgs::default(),
        remote_read_context(&store, repository(), &endpoint),
    )
    .await;

    assert!(
        context
            .as_ref()
            .is_err_and(RevisionGraphError::is_disconnected),
        "expected Disconnected, got {:?}",
        context.err()
    );
}

/// The local store's read context stays offline on a handle with a remote, carries no write
/// token, and its read options tell a failed read from a miss.
#[tokio::test]
async fn local_read_context_is_offline_token_less_and_strict() {
    let store = in_memory_store(Some(connected_endpoint()), BoundFlags::default()).await;

    let context = local_read_context(&store, repository());

    assert!(is_offline(&context).await, "the context reaches no remote");
    assert!(context.try_write_mutable_store().is_none());
    let options = read_options_from_repository(&context);
    assert!(options.local && options.strict_local, "{options:?}");
}

#[tokio::test]
async fn local_write_context_is_strict() {
    let store = in_memory_store(Some(connected_endpoint()), BoundFlags::default()).await;

    let context = local_write_context(&store, repository());

    assert!(is_offline(&context).await, "the context reaches no remote");
    assert!(context.try_write_mutable_store().is_some());
    let options = read_options_from_repository(&context);
    assert!(options.local && options.strict_local, "{options:?}");
}

/// The remote's read context is connected, takes its session pools from the handle's endpoint,
/// carries no write token, and its read options neither read the local store nor store in it.
#[tokio::test]
async fn remote_read_context_is_connected_token_less_and_remote_only() {
    let endpoint = connected_endpoint();
    let store = in_memory_store(Some(endpoint.clone()), BoundFlags::default()).await;

    let context = remote_read_context(&store, repository(), &endpoint)
        .await
        .expect("connects");

    assert!(is_connected(&context).await);
    assert!(holds_endpoint(&context, &endpoint));
    assert!(context.try_write_mutable_store().is_none());
    let options = read_options_from_repository(&context);
    assert!(
        !options.local && options.no_local_write && !options.strict_local,
        "{options:?}"
    );
}

/// The remote's read context trusts the remote as the storage handle does, whatever the call's
/// globals say: through a handle that trusts it, as one opened with `skip_verify` does, a remote
/// read checks only sizes under `trust_remote` 0, and through one that doesn't, it verifies under
/// `trust_remote` 1.
#[tokio::test]
async fn remote_read_context_trusts_the_remote_as_the_handle_does() {
    for (handle_trusts, trust_remote) in [(true, 0), (false, 1)] {
        let endpoint = connected_endpoint();
        let mut store = in_memory_store(Some(endpoint.clone()), BoundFlags::default()).await;
        store.trust_remote = handle_trusts;
        let globals = LoreGlobalArgs {
            trust_remote,
            ..Default::default()
        };

        let options = under_globals(globals, async {
            let context = remote_read_context(&store, repository(), &endpoint)
                .await
                .expect("connects");
            read_options_from_repository(&context)
        })
        .await;

        assert_eq!(
            options.trust_remote, handle_trusts,
            "trust_remote {trust_remote}"
        );
    }
}

/// The full context writes, and reaches the remote through the endpoint when given one.
#[tokio::test]
async fn full_context_writes_and_reaches_the_remote_it_is_given() {
    let endpoint = connected_endpoint();
    let store = in_memory_store(Some(endpoint.clone()), BoundFlags::default()).await;

    let connected = full_context(&store, repository(), Some(&endpoint))
        .await
        .expect("connects");
    let offline = full_context(&store, repository(), None)
        .await
        .expect("builds offline");

    assert!(is_connected(&connected).await);
    assert!(holds_endpoint(&connected, &endpoint));
    assert!(connected.try_write_mutable_store().is_some());
    assert!(is_offline(&offline).await);
    assert!(offline.endpoint.is_none());
    assert!(offline.try_write_mutable_store().is_some());
}

/// Without a remote, the full context's reads are strict: a block the local store holds that
/// doesn't load fails the read rather than reading as absent.
#[tokio::test]
async fn an_offline_full_context_fails_a_read_of_a_corrupt_block() {
    let store = in_memory_store(None, BoundFlags::default()).await;
    let corrupt = Hash::from([0xc0u8; 32]);
    store_undecodable(&store, corrupt).await;

    let read = under_globals(LoreGlobalArgs::default(), async {
        let context = full_context(&store, repository(), None)
            .await
            .expect("builds offline");
        Metadata::deserialize(context, corrupt).await
    })
    .await;

    assert!(
        read.as_ref().is_err_and(MetadataError::is_internal),
        "expected Internal, got {:?}",
        read.err()
    );
}
