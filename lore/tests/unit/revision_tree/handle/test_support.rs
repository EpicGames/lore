// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Test-only fixture builder for [`RevisionTreeInternal`]. The
//! production constructor lives with the load verb; this fixture
//! lets the registry / guard unit tests run against a minimally-
//! populated value without depending on `load`.
//!
//! The fixture builds a real `Arc<StoreInternal>` via the storage
//! crate's `in_memory_for_tests` helper and a real `Arc<State>` /
//! `Arc<RepositoryContext>` via the `lore-revision` in-memory test
//! plumbing, so the registry tests run against the same type shape
//! the production load verb produces.
use std::sync::Arc;

use lore::revision_tree::handle::RevisionTreeInternal;
use lore::storage::store::StoreInternal;
use lore::storage::store::in_memory_for_tests;
use lore_base::types::Partition;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryContextCreationArgs;
use lore_revision::repository::create_client_memory_stores;
use lore_revision::state::State;
use lore_transport::ProtocolError;

/// Build a `RevisionTreeInternal` for tests. Uses in-memory stores so
/// no filesystem touch happens and no cleanup is required.
pub(crate) async fn new_for_testing() -> Arc<RevisionTreeInternal> {
    let store_internal: Arc<StoreInternal> = in_memory_for_tests("revision-tree-test").await;
    on_store(store_internal).await
}

/// The same fixture, loaded against a storage handle the IPC connection `connection_id`
/// opened — what the connection-teardown close cascade matches on.
pub(crate) async fn new_for_testing_on_connection(connection_id: u64) -> Arc<RevisionTreeInternal> {
    let store_internal = Arc::into_inner(in_memory_for_tests("revision-tree-test").await)
        .expect("a store just built has one owner")
        .with_connection_id(connection_id);
    on_store(Arc::new(store_internal)).await
}

/// The fixture, loaded against the storage handle whose state is `store_internal`.
async fn on_store(store_internal: Arc<StoreInternal>) -> Arc<RevisionTreeInternal> {
    let (immutable, mutable) = create_client_memory_stores()
        .await
        .expect("create_client_memory_stores");
    let repository = Partition::default();
    let repository_context = Arc::new(RepositoryContext::new(RepositoryContextCreationArgs {
        paths: None,
        immutable_store: immutable,
        mutable_store: mutable,
        id: repository,
        instance_id: Default::default(),
        remote: Err(ProtocolError::from(lore_base::error::NoRemote)),
        filter: Arc::default(),
        filesystem_provider: None,
    }));
    let state = State::new();
    Arc::new(RevisionTreeInternal::new(
        store_internal,
        repository,
        repository_context,
        state,
    ))
}
