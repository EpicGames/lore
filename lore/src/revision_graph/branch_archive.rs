// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_graph_branch_archive`: archive a branch at each location a call consults, the
//! remote first.
//!
//! Where the call will archive the branch in the local store ([`branch::archives_locally`]), it
//! first reads the instances on the branch, and a live one refuses the call before either location
//! changes. At the remote, it reads the branch's record and metadata, refuses a protected branch,
//! the default branch and one without a branch point, and archives the branch with Lore Server's
//! delete, reading the refusal of the delete again. In the local store, it archives the branch as
//! the archive core does ([`branch::archive_local`]). A location where the branch is archived
//! already reports it archived and writes nothing more, once a stopped restore is settled. Each
//! location that holds the branch reports its record with `archived` set, and each location ends
//! with its location complete event. A failure at the remote stops the call before the local store.

use std::sync::Arc;

use lore_base::error::BranchNotFound;
use lore_base::error::DeleteCurrent;
use lore_base::error::DeleteDefault;
use lore_base::error::DeleteProtected;
use lore_base::types::BranchId;
use lore_base::types::Partition;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::branch;
use lore_revision::instance;
use lore_revision::instance::InstanceStaleness;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::metadata::Metadata;
use lore_revision::protocol::RemoteEndpoint;
use lore_revision::repository::RepositoryContext;
use lore_transport::ProtocolError;
use lore_transport::Revision;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_graph::RevisionGraphError;
use crate::revision_graph::call::revision_graph_call;
use crate::revision_graph::locations::CallOutcome;
use crate::revision_graph::locations::LocationAnswer;
use crate::revision_graph::locations::LocationMiss;
use crate::revision_graph::locations::PreCheck;
use crate::revision_graph::locations::default_branch_at;
use crate::revision_graph::locations::ended_at;
use crate::revision_graph::locations::local_write_context;
use crate::revision_graph::locations::locations;
use crate::revision_graph::locations::remote_revision_service;
use crate::revision_graph::record::BranchRecordOut;
use crate::revision_graph::record::emit_branch;
use crate::revision_graph::record::emit_instances;
use crate::revision_graph::record::emit_location_complete;
use crate::revision_graph::record::local_branch_record;
use crate::revision_graph::record::remote_record_out;
use crate::storage::handle::LoreStore;
use crate::storage::store::PerCallFlags;
use crate::storage::store::StoreInternal;

/// Arguments for `lore_revision_graph_branch_archive`.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(branch_archive_impl)]
pub struct LoreRevisionGraphBranchArchiveArgs {
    /// Caller-chosen id echoed back in every event this surface defines for the call. Separate
    /// from `globals.correlation_id`.
    pub id: u64,
    /// Open storage handle the call reaches the repository through
    pub store: LoreStore,
    /// Repository the branch is archived in
    pub repository: Partition,
    /// Branch to archive. Not zero.
    pub branch: BranchId,
}

/// The branch the remote holds, as its record reports it, and whether the archive's delete still
/// has to run there.
struct RemoteBranch {
    record: BranchRecordOut,
    deletes: bool,
}

/// Archive a branch at each location the call consults.
pub fn branch_archive(
    globals: LoreGlobalArgs,
    args: LoreRevisionGraphBranchArchiveArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    dispatch_call(globals, args, callback, branch_archive_impl)
}

fn branch_archive_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionGraphBranchArchiveArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    let per_call = PerCallFlags::from_globals(&globals);
    revision_graph_call(
        globals,
        callback,
        args.store,
        args,
        branch_archive,
        async move |store, args| {
            if args.branch.is_zero() {
                return Err(RevisionGraphError::invalid("the branch id is zero"));
            }
            let locations = locations(&store, per_call)?;
            let local = if locations.local() {
                let ctx = local_write_context(&store, args.repository);
                let gate = refuse_live_instances(&ctx, args.id, args.branch).await;
                ended_at(args.id, LoreBranchLocation::Local, gate)??;
                Some(ctx)
            } else {
                None
            };
            let mut outcome = CallOutcome::default();
            if let Some(remote) = locations.remote() {
                let mut records = 0;
                let answer = archive_remote(&mut records, &store, &args, remote).await?;
                emit_location_complete(args.id, LoreBranchLocation::Remote, records, &answer);
                outcome.add(Ok(answer?));
            }
            if let Some(ctx) = local {
                let mut records = 0;
                let answer = archive_in_local_store(&mut records, ctx, args.id, args.branch).await;
                emit_location_complete(args.id, LoreBranchLocation::Local, records, &answer);
                outcome.add(answer);
            }
            outcome.finish(|_| {
                RevisionGraphError::from(BranchNotFound {
                    branch: args.branch.to_string(),
                })
            })
        },
    )
}

/// Refuses an archive of `branch` that would archive it in the local store, which `ctx` reads,
/// while a live repository instance over the store has it as its current branch: the call `id`
/// sends the instance record of each live one, and the refusal is `DeleteCurrent`. An archive that
/// finds the branch absent or archived already in the local store reads no instance.
async fn refuse_live_instances(
    ctx: &Arc<RepositoryContext>,
    id: u64,
    branch: BranchId,
) -> PreCheck<()> {
    if !branch::archives_locally(ctx.clone(), branch)
        .await
        .forward_any::<RevisionGraphError>("reading whether the local store holds the branch")?
    {
        return Ok(Ok(()));
    }
    let mut live = instance::records_on_branch(ctx, branch)
        .await
        .forward_any::<RevisionGraphError>("reading the instances on the branch")?;
    live.retain(|record| record.staleness == InstanceStaleness::Active);
    if live.is_empty() {
        return Ok(Ok(()));
    }
    emit_instances(id, branch, live);
    Ok(Err(RevisionGraphError::from(DeleteCurrent {
        branch: branch.to_string(),
    })))
}

/// Archives the branch `args` names at the remote that `remote`, the storage handle's endpoint,
/// reaches, and sends its record with `archived` set, adding the record events sent to `records`.
/// The remote's checks ([`remote_checks`]) come first, and refuse the call before the delete. A
/// refusal of the delete is read with the checks again ([`delete_refusal`]).
///
/// # Errors
///
/// The outer error is a refusal the checks found before the delete, which emits no location
/// complete event; the answer's error is a failure at the remote.
async fn archive_remote(
    records: &mut u64,
    store: &StoreInternal,
    args: &LoreRevisionGraphBranchArchiveArgs,
    remote: &Arc<RemoteEndpoint>,
) -> Result<LocationAnswer<()>, RevisionGraphError> {
    let reached = remote_revision_service(store, args.repository, remote).await;
    let (ctx, service) = match reached {
        Ok(reached) => reached,
        Err(err) => return Ok(Err(err)),
    };
    let found = match remote_checks(&ctx, service.as_ref(), args.branch).await {
        Ok(Ok(found)) => found,
        Ok(Err(refusal)) => return Err(refusal),
        Err(err) => return Ok(Err(err)),
    };
    let Some(RemoteBranch {
        mut record,
        deletes,
    }) = found
    else {
        return Ok(Ok(Err(LocationMiss::BranchNotFound)));
    };
    if deletes {
        if let Err(err) = service.branch_delete(args.branch).await {
            return Ok(Err(delete_refusal(
                &ctx,
                service.as_ref(),
                args.branch,
                err,
            )
            .await));
        }
        record.archived = true;
    }
    *records += emit_branch(args.id, LoreBranchLocation::Remote, record);
    Ok(Ok(Ok(())))
}

/// The remote's record of `branch` read through `ctx`, the remote's read context, and `service`,
/// its revision service, with `protect` from its metadata, and whether the archive's delete still
/// has to run: not for a branch the remote has archived, unless its metadata records a stopped
/// restore or a released name, which the delete settles. The checks come first, for an archived
/// branch too, as the archive core and Lore Server's delete run them. `None` where the remote
/// doesn't hold the branch.
///
/// # Errors
///
/// A failed request or read fails the location. The refusals are `DeleteProtected` for a protected
/// branch, and `DeleteDefault` for the repository's default branch and a branch without a branch
/// point.
async fn remote_checks(
    ctx: &Arc<RepositoryContext>,
    service: &dyn Revision,
    branch: BranchId,
) -> PreCheck<Option<RemoteBranch>> {
    let queried = match service.branch_query(Some(branch), None).await {
        Ok(queried) => queried,
        Err(err) if err.is_not_found() => return Ok(Ok(None)),
        Err(err) => {
            return Err(err)
                .forward_any::<RevisionGraphError>("querying the remote for the branch");
        }
    };
    let metadata = Metadata::deserialize(ctx.clone(), queried.metadata)
        .await
        .forward_any::<RevisionGraphError>("reading the branch metadata")?;
    let pending = branch::holds_pending_name(&metadata);
    let empty_stack = queried.stack.is_empty();
    let mut record = remote_record_out(queried);
    record.protect = branch::protected(&metadata);
    if record.protect {
        return Ok(Err(RevisionGraphError::from(DeleteProtected {
            branch: record.name.as_str().to_owned(),
        })));
    }
    if empty_stack || default_branch_at(ctx, true).await? == Some(branch) {
        return Ok(Err(RevisionGraphError::from(DeleteDefault {
            branch: record.name.as_str().to_owned(),
        })));
    }
    let deletes = !record.archived || pending;
    Ok(Ok(Some(RemoteBranch { record, deletes })))
}

/// The failure that reports `err`, the remote's refusal of the delete of `branch`: the refusal the
/// remote's checks ([`remote_checks`]), read again through `ctx` and `service`, find now, and
/// otherwise `err` as [`RevisionGraphError::remote_refusal`] reports it.
async fn delete_refusal(
    ctx: &Arc<RepositoryContext>,
    service: &dyn Revision,
    branch: BranchId,
    err: ProtocolError,
) -> RevisionGraphError {
    if let Ok(Err(refusal)) = remote_checks(ctx, service, branch).await {
        return refusal;
    }
    RevisionGraphError::remote_refusal(err, "the remote refused the branch delete")
}

/// Archives `branch` in the local store through `ctx` as the archive core does
/// ([`branch::archive_local`]), and sends its record of call `id` with `archived` set, a branch
/// archived already included, adding the record events sent to `records`. The record is read
/// before the archive, which changes none of its fields but `archived`.
async fn archive_in_local_store(
    records: &mut u64,
    ctx: Arc<RepositoryContext>,
    id: u64,
    branch: BranchId,
) -> LocationAnswer<()> {
    let Some(mut record) = local_branch_record(ctx.clone(), branch).await? else {
        return Ok(Err(LocationMiss::BranchNotFound));
    };
    branch::archive_local(ctx, branch)
        .await
        .forward_any::<RevisionGraphError>("archiving the branch in the local store")?;
    record.archived = true;
    *records += emit_branch(id, LoreBranchLocation::Local, record);
    Ok(Ok(()))
}
