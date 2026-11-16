// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use lore_base::error::Disconnected;
use lore_base::error::NotAuthorized;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_revision::immutable::*;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::metadata::METADATA_MAX_SIZE;
use lore_revision::metadata::Metadata;
use lore_revision::metadata::MetadataError;
use lore_revision::repository::RemoteState;
use lore_revision::repository::RepositoryContext;
use lore_revision::test_util::context_with_state;
use lore_storage::StoreError;
use lore_storage::StoreMatch;
use lore_storage::StoreMatchResult;
use lore_storage::test_util::HookedImmutableStore;
use lore_storage::test_util::ImmutableCall;
use lore_transport::Connection;
use lore_transport::ProtocolError;
use lore_transport::test_util::MemoryPeer;

use super::session_tests::under_globals;
use crate::xorshift::incompressible;

fn globals() -> LoreGlobalArgs {
    LoreGlobalArgs {
        correlation_id: "remote-metadata".into(),
        ..LoreGlobalArgs::default()
    }
}

fn remote(peer: &Arc<MemoryPeer>) -> RemoteState {
    RemoteState::Connected(Connection::detached(vec![peer.clone()]))
}

/// A context on fresh in-memory stores whose remote is `peer`.
async fn connected_to(peer: &Arc<MemoryPeer>) -> Arc<RepositoryContext> {
    context_with_state(remote(peer)).await
}

/// A context on fresh in-memory stores whose remote is `remote`, and whose immutable store runs
/// `hook` before each content call ([`HookedImmutableStore`]).
async fn hooked_context(
    hook: impl Fn(ImmutableCall, Partition, Address) -> Result<(), StoreError> + Send + Sync + 'static,
    remote: RemoteState,
) -> Arc<RepositoryContext> {
    let (immutable, mutable) = lore_revision::repository::create_client_memory_stores()
        .await
        .expect("in-memory stores should be creatable");
    Arc::new(RepositoryContext::new_with_state(
        None,
        HookedImmutableStore::wrap(immutable, hook),
        mutable,
        lore_revision::lore::RepositoryId::default(),
        lore_revision::instance::InstanceId::default(),
        remote,
        Arc::default(),
        None,
    ))
}

/// `metadata` written at the remote through `repository`.
async fn serialized_remote(
    metadata: &Metadata,
    repository: &Arc<RepositoryContext>,
) -> Result<Hash, MetadataError> {
    // Boxed: the write pipeline's future sits at the crate's `future-size-threshold`.
    under_globals(globals(), || {
        Box::pin(metadata.serialize_remote(repository.clone()))
    })
    .await
}

/// The metadata blob `hash` names, read through `repository`.
async fn deserialized(repository: &Arc<RepositoryContext>, hash: Hash) -> Metadata {
    under_globals(globals(), || {
        Box::pin(Metadata::deserialize(repository.clone(), hash))
    })
    .await
    .expect("the metadata blob reads back")
}

/// What the local store of `repository` records at `address`.
async fn local_entry(repository: &Arc<RepositoryContext>, address: Address) -> StoreMatchResult {
    let mut result = [StoreMatchResult::default()];
    repository
        .immutable_store()
        .query(repository.id, &[address], &mut result)
        .await
        .expect("the local store answers");
    result[0]
}

fn blob(hash: Hash) -> Address {
    Address {
        hash,
        context: Context::default(),
    }
}

/// The blob reaches the remote, and the local store records it without keeping its payload.
#[tokio::test]
async fn a_remote_serialize_puts_the_blob_and_leaves_a_payload_less_local_entry() {
    let peer = MemoryPeer::new();
    let repository = connected_to(&peer).await;
    let mut metadata = Metadata::new();
    metadata
        .set_string("owner", "a remote serialize puts the blob")
        .expect("sets the entry");

    let hash = serialized_remote(&metadata, &repository)
        .await
        .expect("the blob is written");

    let (_, payload) = peer.held(&blob(hash)).expect("the peer holds the blob");
    assert!(payload.is_some(), "the peer holds the blob's payload");
    let entry = local_entry(&repository, blob(hash)).await;
    assert_ne!(entry.match_made, StoreMatch::MatchNone);
    assert!(
        entry.stored_durable && !entry.stored_local,
        "the local entry records the upload and holds no payload: {entry:?}"
    );
}

/// Metadata over one fragment reaches the remote as leaves and a fragment list, which read back
/// through the remote alone. Locally the list keeps its payload, and no leaf does.
#[tokio::test]
async fn a_remote_serialize_over_one_fragment_reads_back() {
    let peer = MemoryPeer::new();
    let repository = connected_to(&peer).await;
    let value = incompressible(300 * 1024, 0x0062_0300);
    let mut metadata = Metadata::new();
    metadata
        .set_binary("large", &value)
        .expect("sets the entry");

    let hash = serialized_remote(&metadata, &repository)
        .await
        .expect("the blob is written");

    let root = blob(hash);
    let (list, _) = peer.held(&root).expect("the peer holds the fragment list");
    assert_ne!(list.flags & FragmentFlags::PayloadFragmented, 0);
    let leaves: Vec<Address> = peer
        .addresses()
        .into_iter()
        .filter(|address| *address != root)
        .collect();
    assert!(leaves.len() >= 2, "the blob was cut into {leaves:?}");
    assert_eq!(peer.puts(), leaves.len() + 1, "one put per fragment");

    let reader = connected_to(&peer).await;
    let read = deserialized(&reader, hash).await;
    assert_eq!(
        read.get_binary("large").expect("the entry reads back"),
        value.as_slice()
    );

    assert!(local_entry(&repository, root).await.stored_local);
    for leaf in leaves {
        let entry = local_entry(&repository, leaf).await;
        assert!(
            entry.match_made != StoreMatch::MatchNone && !entry.stored_local,
            "the local store records leaf {leaf:?} without its payload: {entry:?}"
        );
    }
}

/// An upload the remote refuses still answers the blob's hash, and the local store keeps the
/// blob whole.
#[tokio::test]
async fn a_remote_serialize_whose_upload_fails_answers_the_hash_and_keeps_the_blob_locally() {
    let peer = MemoryPeer::refusing_puts(ProtocolError::from(NotAuthorized));
    let repository = connected_to(&peer).await;
    let mut metadata = Metadata::new();
    metadata
        .set_string("owner", "a remote serialize whose upload fails")
        .expect("sets the entry");

    let hash = serialized_remote(&metadata, &repository)
        .await
        .expect("a refused upload still answers the hash");

    assert_eq!(peer.puts(), 1, "the upload was tried");
    assert!(peer.addresses().is_empty());
    let entry = local_entry(&repository, blob(hash)).await;
    assert!(
        entry.stored_local && !entry.stored_durable,
        "the local store keeps the blob it could not upload: {entry:?}"
    );
    let read = deserialized(&repository, hash).await;
    assert_eq!(
        read.get_string("owner").expect("the entry reads back"),
        "a remote serialize whose upload fails"
    );
}

/// A context without a remote fails before any write, so the local store holds no entry for the
/// blob.
#[tokio::test]
async fn a_remote_serialize_without_a_remote_fails_no_remote_and_writes_nothing() {
    let repository = context_with_state(RemoteState::Offline).await;
    let mut metadata = Metadata::new();
    metadata
        .set_string("owner", "a remote serialize without a remote")
        .expect("sets the entry");

    let written = serialized_remote(&metadata, &repository).await;

    assert!(
        written.as_ref().is_err_and(|err| err.is_no_remote()),
        "got {written:?}"
    );
    let entry = local_entry(
        &repository,
        blob(lore_storage::hash::hash_slice(&metadata.buffer)),
    )
    .await;
    assert_eq!(
        entry.match_made,
        StoreMatch::MatchNone,
        "the local store holds no entry: {entry:?}"
    );
}

#[tokio::test]
async fn a_remote_serialize_of_empty_metadata_is_the_zero_hash() {
    let peer = MemoryPeer::new();
    let repository = connected_to(&peer).await;

    let hash = serialized_remote(&Metadata::new(), &repository)
        .await
        .expect("empty metadata needs no write");

    assert!(hash.is_zero());
    assert_eq!(peer.puts(), 0);
}

/// Metadata over the limit fails before the write reaches either store.
#[tokio::test]
async fn a_remote_serialize_over_one_mebibyte_writes_nothing() {
    let peer = MemoryPeer::new();
    let local_calls = Arc::new(AtomicUsize::new(0));
    let counted = local_calls.clone();
    let repository = hooked_context(
        move |_, _, _| {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
        remote(&peer),
    )
    .await;
    let before = local_calls.load(Ordering::SeqCst);
    let half = incompressible(METADATA_MAX_SIZE / 2, 0x0062_0500);
    let mut metadata = Metadata::new();
    metadata.set_binary("first", &half).expect("sets the entry");
    metadata
        .set_binary("second", &half)
        .expect("sets the entry");

    let written = serialized_remote(&metadata, &repository).await;

    assert!(
        written.as_ref().is_err_and(|err| err.is_oversized()),
        "got {written:?}"
    );
    assert_eq!(peer.puts(), 0);
    assert_eq!(
        local_calls.load(Ordering::SeqCst),
        before,
        "the local store sees no call"
    );
}

/// A get of an address the peer lacks reads as the address not found, as a server's `NotFound`
/// does.
#[tokio::test]
async fn a_read_of_an_address_the_memory_peer_lacks_is_address_not_found() {
    let peer = MemoryPeer::new();
    let repository = connected_to(&peer).await;
    let absent = blob(Hash::from([0x62u8; 32]));

    let answer = under_globals(globals(), || {
        read(
            repository.clone(),
            absent,
            None,
            read_options_from_repository(&repository),
        )
    })
    .await;

    assert!(
        answer.as_ref().is_err_and(|err| err.is_address_not_found()),
        "got {answer:?}"
    );
    assert!(peer.gets() > 0, "the read reached the peer");
}

/// `content` stored in the local store of `repository`, with its payload where `payload` is set,
/// and otherwise as the entry an eviction leaves.
async fn stored_locally(
    repository: &Arc<RepositoryContext>,
    content: &'static [u8],
    payload: bool,
) -> Address {
    let address = blob(lore_storage::hash::hash_slice(content));
    let fragment = Fragment {
        flags: if payload {
            0
        } else {
            FragmentFlags::PayloadStoredDurable.bits()
        },
        size_payload: content.len() as u32,
        size_content: content.len() as u64,
    };
    repository
        .immutable_store()
        .put(
            repository.id,
            address,
            fragment,
            payload.then(|| Bytes::from_static(content)),
            false,
        )
        .await
        .expect("the local store takes the entry");
    address
}

#[tokio::test]
async fn first_not_held_locally_names_an_evicted_payload() {
    let repository = context_with_state(RemoteState::Offline).await;
    let held = stored_locally(&repository, b"first not held locally: held", true).await;
    let evicted = stored_locally(&repository, b"first not held locally: evicted", false).await;
    let both = [held, evicted];

    let first = under_globals(globals(), || first_not_held_locally(&repository, &both))
        .await
        .expect("the local store answers");
    let held_only = under_globals(globals(), || {
        first_not_held_locally(&repository, &both[..1])
    })
    .await
    .expect("the local store answers");

    assert_eq!(first, Some(evicted));
    assert_eq!(held_only, None);
}

/// A query that fails is the error, not an address read as lacking. No addresses make no query,
/// so they answer `None` all the same.
#[tokio::test]
async fn first_not_held_locally_fails_where_the_query_fails() {
    let repository = hooked_context(
        |call, _, _| match call {
            ImmutableCall::Query => Err(StoreError::from(Disconnected)),
            _ => Ok(()),
        },
        RemoteState::Offline,
    )
    .await;
    let addresses = [blob(Hash::from([0x62u8; 32]))];

    let first = under_globals(globals(), || {
        first_not_held_locally(&repository, &addresses)
    })
    .await;
    let none = under_globals(globals(), || first_not_held_locally(&repository, &[])).await;

    assert!(
        first.as_ref().is_err_and(|err| err.is_disconnected()),
        "got {first:?}"
    );
    assert!(matches!(none, Ok(None)), "got {none:?}");
}

#[tokio::test]
async fn missing_remote_lists_the_addresses_the_remote_lacks() {
    let peer = MemoryPeer::new();
    let session = peer.session(Partition::default());
    let held = blob(Hash::from([0x65u8; 32]));
    session
        .put(held, Fragment::default(), None)
        .await
        .expect("the peer takes the put");
    let lacking = [
        blob(Hash::from([0x66u8; 32])),
        blob(Hash::from([0x67u8; 32])),
    ];

    let missing = missing_remote(&session, &[lacking[0], held, lacking[1]])
        .await
        .expect("the peer answers the query");

    assert_eq!(missing, lacking.to_vec());
    assert_eq!(peer.queries(), 1);
}

/// An answer with fewer statuses than addresses fails, rather than read the unanswered addresses
/// as held.
#[tokio::test]
async fn missing_remote_fails_where_the_answer_is_short() {
    let peer = MemoryPeer::answering_queries_short();
    let session = peer.session(Partition::default());
    let addresses = [
        blob(Hash::from([0x6cu8; 32])),
        blob(Hash::from([0x6du8; 32])),
    ];

    let missing = missing_remote(&session, &addresses).await;

    assert!(
        missing.as_ref().is_err_and(|err| err.is_internal()),
        "got {missing:?}"
    );
    assert_eq!(peer.queries(), 1);
}
