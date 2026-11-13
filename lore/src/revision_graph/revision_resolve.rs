// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_graph_revision_resolve`: resolve a revision identifier at each location a call
//! consults, the remote first.
//!
//! Each location that holds the revision reports one revision record, with the revision's
//! number and the branch its metadata names, and each location ends with its location complete
//! event. [`resolve_at`] resolves an identifier at one location, [`latest_at`] reads a branch's
//! latest revision there, and [`revision_record_at`] the number and branch of a revision a hash
//! names.

use std::sync::Arc;

use futures::FutureExt;
use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::branch;
use lore_revision::find;
use lore_revision::find::NumberWalk;
use lore_revision::find::StateMiss;
use lore_revision::immutable::read_options_from_repository;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::metadata::Metadata;
use lore_revision::protocol::RemoteEndpoint;
use lore_revision::repository::RepositoryContext;
use lore_transport::Revision;
use lore_transport::RevisionItem;
use lore_transport::RevisionListIdentifier;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_graph::LoreRevisionIdentifier;
use crate::revision_graph::RevisionForm;
use crate::revision_graph::RevisionGraphError;
use crate::revision_graph::call::revision_graph_call;
use crate::revision_graph::locations::CallOutcome;
use crate::revision_graph::locations::LocationAnswer;
use crate::revision_graph::locations::LocationMiss;
use crate::revision_graph::locations::local_read_context;
use crate::revision_graph::locations::locations;
use crate::revision_graph::locations::remote_revision_service;
use crate::revision_graph::record::emit_location_complete;
use crate::revision_graph::record::emit_revision;
use crate::storage::handle::LoreStore;
use crate::storage::store::PerCallFlags;
use crate::storage::store::StoreInternal;

/// Arguments for `lore_revision_graph_revision_resolve`.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(revision_resolve_impl)]
pub struct LoreRevisionGraphRevisionResolveArgs {
    /// Caller-chosen id echoed back in every event this surface defines for the call. Separate
    /// from `globals.correlation_id`.
    pub id: u64,
    /// Open storage handle the call reaches the repository through
    pub store: LoreStore,
    /// Repository the revision is resolved in
    pub repository: Partition,
    /// The revision to resolve
    pub revision: LoreRevisionIdentifier,
}

/// A revision one location resolves.
#[lore_macro::test_pub]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Resolved {
    /// The revision's hash.
    pub hash: Hash,
    /// The revision's number.
    pub number: u64,
    /// The branch the revision's metadata names, zero where the location doesn't record one.
    pub branch: BranchId,
}

/// Resolve a revision at each location the call consults.
pub async fn revision_resolve(
    globals: LoreGlobalArgs,
    args: LoreRevisionGraphRevisionResolveArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, revision_resolve_impl).await
}

fn revision_resolve_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionGraphRevisionResolveArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    let per_call = PerCallFlags::from_globals(&globals);
    revision_graph_call(
        globals,
        callback,
        args.store,
        args,
        revision_resolve,
        async move |store, args| {
            let form = args.revision.form()?;
            let locations = locations(&store, per_call)?;
            let mut outcome = CallOutcome::default();
            if let Some(remote) = locations.remote() {
                let answer = resolve_remote(&store, args.repository, remote, form).await;
                report(args.id, LoreBranchLocation::Remote, &answer);
                outcome.add(answer);
            }
            if locations.local() {
                let answer =
                    resolve_at(&local_read_context(&store, args.repository), None, form).await;
                report(args.id, LoreBranchLocation::Local, &answer);
                outcome.add(answer);
            }
            outcome.finish(|miss| miss.error(form))
        },
    )
}

/// Resolves `form` with [`resolve_at`] at the remote that `remote`, the storage handle's endpoint,
/// reaches, through its [`remote_revision_service`].
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn resolve_remote<'a>(
    store: &'a StoreInternal,
    repository: Partition,
    remote: &'a Arc<RemoteEndpoint>,
    form: RevisionForm,
) -> impl Future<Output = LocationAnswer<Resolved>> + 'a {
    async move {
        let (ctx, service) = remote_revision_service(store, repository, remote).await?;
        resolve_at(&ctx, Some(&service), form).await
    }
}

/// Sends the revision record of `answer`, the answer of `location`, where the location resolved
/// the revision, then the location's complete event.
fn report(id: u64, location: LoreBranchLocation, answer: &LocationAnswer<Resolved>) {
    let records = match answer {
        Ok(Ok(resolved)) => emit_revision(
            id,
            location,
            resolved.hash,
            resolved.number,
            resolved.branch,
        ),
        _ => 0,
    };
    emit_location_complete(id, location, records, answer);
}

/// Resolves `form` at one location through `ctx`: at the remote, given its revision service
/// `service`, where `ctx` reads the remote alone
/// ([`remote_read_context`](crate::revision_graph::locations::remote_read_context)), and otherwise
/// in the local store, which `ctx` reads strictly ([`local_read_context`]).
///
/// The latest revision of a branch is the one [`latest_at`] reads. A revision number names the
/// revision the remote's revision list holds with it ([`listed_revision`]), or the one a walk of
/// the line from the local latest finds ([`find::revision_by_number_local`]); a walk that reaches a
/// state the store lacks finds none. A hash names its revision. A revision's number and branch are
/// read as [`state_record`] reads them, except for a revision a number names, whose metadata hash
/// the list or the walk read from its state.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
///
/// # Errors
///
/// A failed or corrupt read, a failed request, a remote list item labelled the number without a
/// state of its own with that number, and a latest revision or a walked parent that is not a
/// revision state.
#[allow(clippy::manual_async_fn)]
pub(crate) fn resolve_at<'a>(
    ctx: &'a Arc<RepositoryContext>,
    service: Option<&'a Arc<dyn Revision>>,
    form: RevisionForm,
) -> impl Future<Output = LocationAnswer<Resolved>> + 'a {
    async move {
        let (hash, record) = match form {
            RevisionForm::Hash(hash) => (hash, revision_record_at(ctx, hash).await?),
            RevisionForm::Latest(branch) => {
                let latest = match latest_at(ctx, service, branch).await? {
                    Ok(latest) => latest,
                    Err(miss) => return Ok(Err(miss)),
                };
                let record = match state_record(ctx, latest).await? {
                    Ok(record) => Some(record),
                    Err(StateMiss::Missing) => None,
                    Err(StateMiss::NotAState) => {
                        return Err(RevisionGraphError::internal(format!(
                            "the latest revision {latest} is not a revision state"
                        )));
                    }
                };
                (latest, record)
            }
            RevisionForm::Number(branch, number) => {
                let (hash, metadata) = match numbered_at(ctx, service, branch, number).await? {
                    Ok(numbered) => numbered,
                    Err(miss) => return Ok(Err(miss)),
                };
                let branch = revision_branch(ctx.clone(), metadata).await?;
                (hash, Some((number, branch)))
            }
        };
        Ok(record
            .map(|(number, branch)| Resolved {
                hash,
                number,
                branch,
            })
            .ok_or(LocationMiss::RevisionNotFound))
    }
}

/// The number and branch of `revision`, a hash an identifier names, at the location `ctx` reads, as
/// [`state_record`] reads them. `None` where the location holds no state under the hash, a blob
/// that is not a revision state included, since an identifier can name any blob.
///
/// # Errors
///
/// A failed or corrupt read.
#[inline]
pub(crate) fn revision_record_at(
    ctx: &Arc<RepositoryContext>,
    revision: Hash,
) -> impl Future<Output = Result<Option<(u64, BranchId)>, RevisionGraphError>> + '_ {
    state_record(ctx, revision).map(|read| read.map(Result::ok))
}

/// The number and branch of `revision` at the location `ctx` reads, or why the location holds no
/// state under it ([`find::revision_state`]): the number its state records, and the branch its
/// metadata names ([`revision_branch`]).
///
/// Not an `async fn`, which would hold a second copy of its arguments.
///
/// # Errors
///
/// A failed or corrupt read.
#[allow(clippy::manual_async_fn)]
fn state_record(
    ctx: &Arc<RepositoryContext>,
    revision: Hash,
) -> impl Future<Output = Result<Result<(u64, BranchId), StateMiss>, RevisionGraphError>> + '_ {
    async move {
        let (number, metadata) =
            match find::revision_state(ctx, revision, read_options_from_repository(ctx))
                .await
                .forward_any::<RevisionGraphError>("reading the revision's state")?
            {
                Ok(state) => (state.revision_number, state.metadata_hash()),
                Err(miss) => return Ok(Err(miss)),
            };
        let branch = revision_branch(ctx.clone(), metadata).await?;
        Ok(Ok((number, branch)))
    }
}

/// The branch the revision metadata blob `metadata` names, read through `ctx`. Zero where the blob
/// names none, and where the location doesn't hold it, as the local store holds the state of a
/// revision cached from the remote's revision list without its metadata.
///
/// # Errors
///
/// A failed or corrupt read.
fn revision_branch(
    ctx: Arc<RepositoryContext>,
    metadata: Hash,
) -> impl Future<Output = Result<BranchId, RevisionGraphError>> {
    Metadata::deserialize(ctx, metadata).map(|read| match read {
        Ok(metadata) => Ok(metadata.get_branch().unwrap_or_default()),
        Err(err) if err.is_address_not_found() => Ok(BranchId::default()),
        Err(err) => Err(err).forward_any::<RevisionGraphError>("reading the revision's metadata"),
    })
}

/// The latest revision of `branch` at one location: at the remote, given its revision service
/// `service`, the latest its record of the branch names, and otherwise the local store's, read
/// through `ctx`. [`LocationMiss::RevisionNotFound`] for a branch the location holds without a
/// latest revision: in the local store, one with metadata and no latest.
/// [`LocationMiss::BranchNotFound`] for a branch the location doesn't hold: in the local store, one
/// with neither.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
///
/// # Errors
///
/// A failed read or request.
#[allow(clippy::manual_async_fn)]
pub(crate) fn latest_at<'a>(
    ctx: &'a Arc<RepositoryContext>,
    service: Option<&'a Arc<dyn Revision>>,
    branch: BranchId,
) -> impl Future<Output = LocationAnswer<Hash>> + 'a {
    async move {
        let Some(service) = service else {
            if let Some(latest) = branch::mutable_value(ctx.clone(), branch::LATEST, branch)
                .await
                .forward_any::<RevisionGraphError>("loading the latest revision")?
            {
                return Ok(Ok(latest));
            }
            let metadata = branch::mutable_value(ctx.clone(), branch::METADATA, branch)
                .await
                .forward_any::<RevisionGraphError>("loading the metadata key")?;
            return Ok(Err(match metadata {
                Some(_) => LocationMiss::RevisionNotFound,
                None => LocationMiss::BranchNotFound,
            }));
        };
        match service.branch_query(Some(branch), None).await {
            Ok(record) if record.latest.is_zero() => Ok(Err(LocationMiss::RevisionNotFound)),
            Ok(record) => Ok(Ok(record.latest)),
            Err(err) if err.is_not_found() => Ok(Err(LocationMiss::BranchNotFound)),
            Err(err) => Err(err).forward_any::<RevisionGraphError>("querying the remote's branch"),
        }
    }
}

/// The revision numbered `number` on `branch` at one location, and the hash of its metadata: at
/// the remote, given its revision service `service`, the one its revision list holds with that
/// number, as [`listed_revision`] reads it, and otherwise the one a walk of the line from the local
/// latest finds ([`find::revision_by_number_local`]), where a state the store lacks ends the walk
/// without one, since the local answer counts a block the store doesn't hold as absent. Lore
/// Server answers a list `NotFound` both for a branch it doesn't hold and for a number it can't
/// resolve, so a `NotFound` is followed by the branch's [`latest_at`], which tells the two apart.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
///
/// # Errors
///
/// A failed or corrupt read, a failed request, a list item labelled the number without a state of
/// its own with that number, and a walked parent that is not a revision state.
#[allow(clippy::manual_async_fn)]
fn numbered_at<'a>(
    ctx: &'a Arc<RepositoryContext>,
    service: Option<&'a Arc<dyn Revision>>,
    branch: BranchId,
    number: u64,
) -> impl Future<Output = LocationAnswer<(Hash, Hash)>> + 'a {
    async move {
        let Some(remote_service) = service else {
            let latest = match latest_at(ctx, None, branch).await? {
                Ok(latest) => latest,
                Err(miss) => return Ok(Err(miss)),
            };
            let walk = find::revision_by_number_local(ctx, latest, number)
                .await
                .forward_any::<RevisionGraphError>("walking the local latest's line")?;
            return Ok(match walk {
                NumberWalk::Found { revision, metadata } => Ok((revision, metadata)),
                NumberWalk::Absent | NumberWalk::Missing(_) => Err(LocationMiss::RevisionNotFound),
            });
        };
        match remote_service
            .revision_list(RevisionListIdentifier { branch, number }.into())
            .await
        {
            Ok(page) => {
                return Ok(
                    listed_revision(&page.items, number)?.ok_or(LocationMiss::RevisionNotFound)
                );
            }
            Err(err) if !err.is_not_found() => {
                return Err(err)
                    .forward_any::<RevisionGraphError>("listing the remote's revisions");
            }
            Err(_not_found) => {}
        }
        Ok(Err(match latest_at(ctx, service, branch).await? {
            Err(LocationMiss::BranchNotFound) => LocationMiss::BranchNotFound,
            _ => LocationMiss::RevisionNotFound,
        }))
    }
}

/// The revision `items`, a page of the remote's revision list, holds with `number`, and the hash of
/// its metadata, both read from the state its item carries, where that state is the revision's own
/// ([`find::verified_state`]): the number and the metadata beside a state are the server's claim.
/// Lore Server labels a number once per page and sends each item's own state. `None` where no item
/// is labelled `number`.
///
/// # Errors
///
/// `Internal` where the item labelled `number` carries no state of its own, or one with another
/// number.
#[lore_macro::test_pub]
fn listed_revision(
    items: &[RevisionItem],
    number: u64,
) -> Result<Option<(Hash, Hash)>, RevisionGraphError> {
    let Some(item) = items.iter().find(|item| item.number == number) else {
        return Ok(None);
    };
    match find::verified_state(item) {
        Some(state) if state.revision_number == number => {
            Ok(Some((item.signature, state.metadata_hash())))
        }
        _ => Err(RevisionGraphError::internal(format!(
            "the remote listed revision {} as number {number} without a state of its own with \
             that number",
            item.signature
        ))),
    }
}
