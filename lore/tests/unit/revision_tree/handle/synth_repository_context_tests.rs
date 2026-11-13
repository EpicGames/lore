// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore::interface::LoreGlobalArgs;
use lore::revision_tree::handle::synth_repository_context;
use lore::storage::store::BoundFlags;
use lore::storage::store::StoreInternal;
use lore::storage::store::in_memory_for_tests;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_revision::immutable::read_options_from_repository;
use lore_revision::protocol::RemoteConnections;
use lore_revision::protocol::RemoteEndpoint;
use lore_revision::repository::RemoteStatus;
use lore_revision::state::State;
use lore_transport::Connection;
use lore_transport::test_util::CopyingPeer;
use lore_transport::traits::Storage;

use crate::storage::in_memory_store;
use crate::storage::under_globals;

/// A storage handle's state over in-memory stores, whose remote for `partition` is a connection to
/// a peer that starts every session it is asked for; and that connection, which counts the session
/// pools started on it ([`Connection::storage_session_starts`]).
async fn handle_over_a_peer(partition: Partition) -> (StoreInternal, Arc<Connection>) {
    let peer: Arc<dyn Storage> = CopyingPeer::new(0, None);
    let connection = Connection::detached(vec![peer]);
    let endpoint = RemoteEndpoint::over(RemoteConnections::with_connection(
        partition,
        connection.clone(),
    ));
    let store = in_memory_store(Some(Arc::new(endpoint)), BoundFlags::default()).await;
    (store, connection)
}

#[tokio::test]
async fn synth_repository_context_round_trips_empty_state_via_zero_hash_deserialize() {
    let store = in_memory_for_tests("synth-context-test").await;
    let partition = Partition::from([0x77u8; 16]);

    let repo_context = synth_repository_context(&store, partition).await;

    State::deserialize(repo_context.clone(), Hash::default())
        .await
        .expect("zero hash must deserialize to an empty state");

    assert!(
        repo_context.paths.is_none(),
        "synthesized context must have no working-tree path"
    );
    assert_eq!(
        repo_context.id, partition,
        "synthesized context must carry the supplied partition"
    );
    assert!(
        matches!(repo_context.remote_status().await, RemoteStatus::Offline),
        "in-memory store has no remote, so the context must be Offline"
    );
}

/// A revision tree's context is the only holder of its session pool, so the connection pins none:
/// once one context is dropped, the next over the same connection, under the same correlation id,
/// starts a pool of its own.
#[tokio::test]
async fn synth_repository_contexts_leave_no_session_pool_pinned_on_the_connection() {
    let partition = Partition::from([0x78u8; 16]);
    let (store, connection) = handle_over_a_peer(partition).await;

    under_globals(LoreGlobalArgs::default(), async {
        for started in 1..=2 {
            let context = synth_repository_context(&store, partition).await;
            context.session_pool("tree").await.expect("starts a pool");
            drop(context);
            assert_eq!(connection.storage_session_starts(), started);
        }
    })
    .await;
}

/// A revision tree's context trusts the remote as the storage handle does, whatever the call's
/// globals say: through a handle that trusts it, as one opened with `skip_verify` does, a remote
/// read checks only sizes under `trust_remote` 0, and through one that doesn't, it verifies under
/// `trust_remote` 1.
#[tokio::test]
async fn synth_repository_context_trusts_the_remote_as_the_handle_does() {
    let partition = Partition::from([0x79u8; 16]);
    for (handle_trusts, trust_remote) in [(true, 0), (false, 1)] {
        let (mut store, _connection) = handle_over_a_peer(partition).await;
        store.trust_remote = handle_trusts;
        let globals = LoreGlobalArgs {
            trust_remote,
            ..Default::default()
        };

        let options = under_globals(globals, async {
            let context = synth_repository_context(&store, partition).await;
            read_options_from_repository(&context)
        })
        .await;

        assert_eq!(
            options.trust_remote, handle_trusts,
            "trust_remote {trust_remote}"
        );
    }
}
