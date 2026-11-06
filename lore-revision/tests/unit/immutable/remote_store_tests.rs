// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Requests that reach a remote's storage. `RemoteImmutableStore` checks what the remote serves
//! and heals what fails the check. `RemoteMutableStore` reads and `repository::verify` pass on the
//! remote's refusal of the credentials.

use std::sync::Arc;

use lore_base::error::NotAuthenticated;
use lore_base::error::NotAuthorized;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_revision::interface::LoreString;
use lore_revision::repository::RemoteState;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::verify::VerifyFragmentArgs;
use lore_revision::repository::verify::verify_fragment_boxed;
use lore_revision::store::remote::RemoteImmutableStore;
use lore_revision::store::remote::RemoteMutableStore;
use lore_revision::test_util::context_with_state;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_transport::Connection;
use lore_transport::ProtocolError;

use super::serving::Serving;
use super::serving::ServingStorage;
use super::serving::remote_copy;
use super::serving::serving_remote;
use super::session_tests::under_execution_context;

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

/// A remote answering every verify and mutable load with `refusal`, and the context it serves
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
