// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The locations a revision graph call consults, and the contexts it reaches them through.
//!
//! Each builder makes a context over the storage handle's stores, which the call drops when it
//! returns: [`local_read_context`] and `local_write_context` for the local store,
//! [`remote_read_context`] for the remote, with its revision service in
//! [`remote_revision_service`], and `full_context` for both. The two that write are built only
//! with the `test-util` feature.
//! The local store's context is offline, so work through it never reaches the remote. The remote's
//! context reads immutable data from the remote alone and stores none of it in the local store, in
//! each read that takes its options from the context
//! ([`read_options_from_repository`](lore_revision::immutable::read_options_from_repository)). Its
//! mutable store is the local one: work at the remote reaches the remote's mutable state through
//! RPCs, and a mutable read through it reads the local store.
//! A mutable store write needs a write token, which only a context for writing carries; an
//! immutable write needs none. Every context trusts the remote as the storage handle does, and
//! holds its session pools itself or takes them from the storage handle's endpoint, so none stays
//! pinned on the process-wide connection.
//!
//! A read verb that resolves one object takes its outcome from what each location answers
//! ([`LocationAnswer`]) through [`CallOutcome`].

#[cfg(feature = "test-util")]
use std::future;
use std::sync::Arc;

use futures::FutureExt;
#[cfg(feature = "test-util")]
use futures::future::Either;
use lore_base::error::BranchNotFound;
use lore_base::error::NoRemote;
use lore_base::error::RevisionNotFound;
use lore_base::types::Partition;
use lore_error_set::prelude::*;
use lore_revision::protocol::RemoteEndpoint;
use lore_revision::repository::RepositoryContext;
use lore_transport::ProtocolError;
use lore_transport::Revision;

use crate::revision_graph::RevisionForm;
use crate::revision_graph::RevisionGraphError;
use crate::storage::store::PerCallFlags;
use crate::storage::store::StoreInternal;

/// The locations a call consults.
pub enum Locations {
    /// The local store alone.
    Local,
    /// The remote alone, through the storage handle's endpoint.
    Remote(Arc<RemoteEndpoint>),
    /// The remote, through the storage handle's endpoint, and the local store.
    Both(Arc<RemoteEndpoint>),
}

impl Locations {
    /// The storage handle's endpoint, where the call consults the remote.
    pub fn remote(&self) -> Option<&Arc<RemoteEndpoint>> {
        match self {
            Self::Local => None,
            Self::Remote(remote) | Self::Both(remote) => Some(remote),
        }
    }

    /// Whether the call consults the local store.
    pub fn local(&self) -> bool {
        !matches!(self, Self::Remote(_))
    }
}

/// The locations a call on `store` consults under the flags the handle was opened with and
/// `per_call`: the local store alone under `local` or `offline`, the remote alone under `remote`,
/// and otherwise the local store and the remote, if the handle has one. Connects to nothing.
///
/// # Errors
///
/// `InvalidArguments` where the flags contradict each other, and `NoRemote` where the call asks for
/// the remote alone and the handle has none.
pub fn locations(
    store: &StoreInternal,
    per_call: PerCallFlags,
) -> Result<Locations, RevisionGraphError> {
    let flags = store.effective_flags(per_call)?;
    if flags.no_remote {
        return Ok(Locations::Local);
    }
    match (&store.remote, flags.no_local) {
        (Some(remote), true) => Ok(Locations::Remote(remote.clone())),
        (Some(remote), false) => Ok(Locations::Both(remote.clone())),
        (None, true) => Err(RevisionGraphError::from(NoRemote)),
        (None, false) => Ok(Locations::Local),
    }
}

/// Why a location holds no object a read call asks for.
#[lore_macro::test_pub]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LocationMiss {
    /// The location doesn't hold the branch the call names.
    BranchNotFound,
    /// The location holds the branch the call names without the revision, or no revision with the
    /// hash the call names.
    RevisionNotFound,
}

impl LocationMiss {
    /// The error that reports this miss for the revision `form` names.
    #[lore_macro::test_pub]
    pub(crate) fn error(self, form: RevisionForm) -> RevisionGraphError {
        let revision = match (self, form) {
            (
                Self::BranchNotFound,
                RevisionForm::Latest(branch) | RevisionForm::Number(branch, _),
            ) => {
                return RevisionGraphError::from(BranchNotFound {
                    branch: branch.to_string(),
                });
            }
            (_, RevisionForm::Hash(hash)) => hash.to_string(),
            (_, RevisionForm::Latest(branch)) => format!("{branch}@LATEST"),
            (_, RevisionForm::Number(branch, number)) => format!("{branch}@{number}"),
        };
        RevisionGraphError::from(RevisionNotFound { revision })
    }
}

/// What one location answers a read call: the `T` it found or why it holds none, or the failure
/// that ends its work.
#[lore_macro::test_pub]
pub(crate) type LocationAnswer<T> = Result<Result<T, LocationMiss>, RevisionGraphError>;

/// The outcome of a read call that resolves one object, from the answers of the locations it
/// consults, added remote first.
///
/// A failure at any location fails the call with that location's error, the remote's where both
/// failed, even where the other location found the object. Otherwise the call succeeds where a
/// location found the object, and fails with the error of [`LocationMiss::RevisionNotFound`] where
/// a location answered it, and of [`LocationMiss::BranchNotFound`] where every location lacked the
/// branch.
#[lore_macro::test_pub]
#[derive(Default)]
pub(crate) struct CallOutcome {
    failure: Option<RevisionGraphError>,
    found: bool,
    revision_missing: bool,
}

impl CallOutcome {
    /// Adds the answer of the next location the call consulted.
    #[lore_macro::test_pub]
    pub(crate) fn add<T>(&mut self, answer: LocationAnswer<T>) {
        match answer {
            Ok(Ok(_)) => self.found = true,
            Ok(Err(LocationMiss::RevisionNotFound)) => self.revision_missing = true,
            Ok(Err(LocationMiss::BranchNotFound)) => {}
            Err(err) => {
                self.failure.get_or_insert(err);
            }
        }
    }

    /// The call's outcome, with `missing` building the error of the miss that fails a call no
    /// location found the object for.
    #[lore_macro::test_pub]
    pub(crate) fn finish(
        self,
        missing: impl FnOnce(LocationMiss) -> RevisionGraphError,
    ) -> Result<(), RevisionGraphError> {
        match self.failure {
            Some(failure) => Err(failure),
            None if self.found => Ok(()),
            None if self.revision_missing => Err(missing(LocationMiss::RevisionNotFound)),
            None => Err(missing(LocationMiss::BranchNotFound)),
        }
    }
}

/// A context over the local store: offline, without a write token, and telling a failed read from
/// a block the store doesn't hold ([`RepositoryContext::strict_local_reads`]).
pub fn local_read_context(store: &StoreInternal, repository: Partition) -> Arc<RepositoryContext> {
    Arc::new(offline_context(store, repository).strict_local_reads())
}

/// [`local_read_context`] with the in-memory write token.
#[cfg(feature = "test-util")]
pub fn local_write_context(store: &StoreInternal, repository: Partition) -> Arc<RepositoryContext> {
    for_writing(offline_context(store, repository))
}

/// A context over the remote: connected over `remote`, the storage handle's endpoint, which holds
/// its session pools; reading immutable data from the remote alone
/// ([`RepositoryContext::reading_remote_only`]); and without a write token.
///
/// Returns the connect's future with the context mapped onto its output, without a future of its
/// own.
///
/// # Errors
///
/// The connect's error.
pub fn remote_read_context<'a>(
    store: &'a StoreInternal,
    repository: Partition,
    remote: &'a Arc<RemoteEndpoint>,
) -> impl Future<Output = Result<Arc<RepositoryContext>, RevisionGraphError>> + 'a {
    connected_context(store, repository, remote)
        .map(|connected| connected.map(|context| Arc::new(context.reading_remote_only())))
}

/// A [`remote_read_context`] over `remote` and the remote's revision service for `repository`,
/// reached over the context's connection, so that work at the remote reaches the service once.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
///
/// # Errors
///
/// The connect's error, and a failed request for the service.
#[allow(clippy::manual_async_fn)]
pub fn remote_revision_service<'a>(
    store: &'a StoreInternal,
    repository: Partition,
    remote: &'a Arc<RemoteEndpoint>,
) -> impl Future<Output = Result<(Arc<RepositoryContext>, Arc<dyn Revision>), RevisionGraphError>> + 'a
{
    async move {
        let ctx = remote_read_context(store, repository, remote).await?;
        let service = ctx
            .remote()
            .await
            .forward_any::<RevisionGraphError>("reaching the remote")?
            .revision(repository)
            .await
            .forward_any::<RevisionGraphError>("reaching the remote's revision service")?;
        Ok((ctx, service))
    }
}

/// A context with the in-memory write token over the local store and, given `remote`, connected
/// over it, which then holds its session pools; [`local_write_context`] without one. Its reads tell
/// a failed local read from a miss where no remote is in reach
/// ([`RepositoryContext::strict_local_reads`]).
///
/// Returns the connect's future with the context mapped onto its output, or the offline context
/// ready, without a future of its own.
///
/// # Errors
///
/// The connect's error.
#[cfg(feature = "test-util")]
pub fn full_context<'a>(
    store: &'a StoreInternal,
    repository: Partition,
    remote: Option<&'a Arc<RemoteEndpoint>>,
) -> impl Future<Output = Result<Arc<RepositoryContext>, RevisionGraphError>> + 'a {
    match remote {
        Some(remote) => Either::Left(
            connected_context(store, repository, remote)
                .map(|connected| connected.map(for_writing)),
        ),
        None => Either::Right(future::ready(Ok(local_write_context(store, repository)))),
    }
}

/// A context over `store` for `repository` with no remote.
fn offline_context(store: &StoreInternal, repository: Partition) -> RepositoryContext {
    store.repository_context(repository, Err(ProtocolError::from(NoRemote)))
}

/// `context` with strict local reads and the in-memory write token.
#[cfg(feature = "test-util")]
fn for_writing(context: RepositoryContext) -> Arc<RepositoryContext> {
    Arc::new(
        context
            .strict_local_reads()
            .with_write_token(StoreInternal::in_memory_write_token()),
    )
}

/// A context over `store` for `repository`, connected over `remote`, which holds its session pools.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn connected_context<'a>(
    store: &'a StoreInternal,
    repository: Partition,
    remote: &'a Arc<RemoteEndpoint>,
) -> impl Future<Output = Result<RepositoryContext, RevisionGraphError>> + 'a {
    async move {
        let connection = remote
            .connection(repository)
            .await
            .forward_any::<RevisionGraphError>("connecting to the remote")?;
        Ok(store
            .repository_context(repository, Ok(connection))
            .with_remote_endpoint(remote.clone()))
    }
}
