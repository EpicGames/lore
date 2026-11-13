// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_graph_branch_resolve`: resolve a branch name at each location a call consults,
//! the remote first.
//!
//! Each location where the name names a branch reports that branch's record, and the local store
//! follows its record with the instance records of the instances on the branch. Each location ends
//! with its location complete event.

use std::sync::Arc;

use lore_base::error::BranchNotFound;
use lore_base::error::InvalidArguments;
use lore_base::types::Partition;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::branch;
use lore_revision::instance;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::interface::LoreString;
use lore_revision::protocol::RemoteEndpoint;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_graph::RevisionGraphError;
use crate::revision_graph::call::revision_graph_call;
use crate::revision_graph::locations::CallOutcome;
use crate::revision_graph::locations::LocationAnswer;
use crate::revision_graph::locations::LocationMiss;
use crate::revision_graph::locations::local_read_context;
use crate::revision_graph::locations::locations;
use crate::revision_graph::locations::remote_revision_service;
use crate::revision_graph::record::emit_branch;
use crate::revision_graph::record::emit_local_branch;
use crate::revision_graph::record::emit_location_complete;
use crate::revision_graph::record::local_branch_record;
use crate::revision_graph::record::remote_branch_record;
use crate::storage::handle::LoreStore;
use crate::storage::store::PerCallFlags;
use crate::storage::store::StoreInternal;

/// Arguments for `lore_revision_graph_branch_resolve`.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(branch_resolve_impl)]
pub struct LoreRevisionGraphBranchResolveArgs {
    /// Caller-chosen id echoed back in every event this surface defines for the call. Separate
    /// from `globals.correlation_id`.
    pub id: u64,
    /// Open storage handle the call reaches the repository through
    pub store: LoreStore,
    /// Repository the branch is resolved in
    pub repository: Partition,
    /// Name of the branch to resolve, compared regardless of case. A branch whose id an older
    /// version derived from its name, and that no name maps to, is found only by that name as
    /// given.
    pub name: LoreString,
}

/// Resolve a branch name at each location the call consults.
pub async fn branch_resolve(
    globals: LoreGlobalArgs,
    args: LoreRevisionGraphBranchResolveArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, branch_resolve_impl).await
}

fn branch_resolve_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionGraphBranchResolveArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    let per_call = PerCallFlags::from_globals(&globals);
    revision_graph_call(
        globals,
        callback,
        args.store,
        args,
        branch_resolve,
        async move |store, args| {
            if !branch::is_valid_name(args.name.as_str()) {
                return Err(RevisionGraphError::from(InvalidArguments {
                    reason: format!(
                        "the branch name is empty, longer than {} bytes, or reads as a branch id",
                        branch::MAX_NAME_LEN
                    ),
                }));
            }
            let locations = locations(&store, per_call)?;
            let mut outcome = CallOutcome::default();
            if let Some(remote) = locations.remote() {
                let mut records = 0;
                let answer = resolve_remote(&mut records, &store, &args, remote).await;
                emit_location_complete(args.id, LoreBranchLocation::Remote, records, &answer);
                outcome.add(answer);
            }
            if locations.local() {
                let mut records = 0;
                let answer = resolve_local(&mut records, &store, &args).await;
                emit_location_complete(args.id, LoreBranchLocation::Local, records, &answer);
                outcome.add(answer);
            }
            outcome.finish(|_| {
                RevisionGraphError::from(BranchNotFound {
                    branch: args.name.as_str().to_owned(),
                })
            })
        },
    )
}

/// Resolves the name `args` gives at the remote that `remote`, the storage handle's endpoint,
/// reaches, and sends the record of the branch its revision service names, adding the record
/// events sent to `records`. Lore Server names only a live branch.
async fn resolve_remote(
    records: &mut u64,
    store: &StoreInternal,
    args: &LoreRevisionGraphBranchResolveArgs,
    remote: &Arc<RemoteEndpoint>,
) -> LocationAnswer<()> {
    let (ctx, record) = {
        let (ctx, service) = remote_revision_service(store, args.repository, remote).await?;
        match service.branch_query(None, Some(args.name.as_str())).await {
            Ok(record) => (ctx, record),
            Err(err) if err.is_not_found() => return Ok(Err(LocationMiss::BranchNotFound)),
            Err(err) => {
                return Err(err)
                    .forward_any::<RevisionGraphError>("querying the remote for the branch name");
            }
        }
    };
    *records += emit_branch(
        args.id,
        LoreBranchLocation::Remote,
        remote_branch_record(ctx, record).await?,
    );
    Ok(Ok(()))
}

/// Resolves the name `args` gives in the local store, and sends the record of the branch the
/// store's name lookup names ([`branch::lookup_name_local_unwritten`]), then the instance records
/// of the instances on it, adding the record events sent to `records`. A branch whose metadata the
/// store doesn't hold has no record there.
///
/// The instances are read before the branch's record rather than beside it, so that the future
/// holds no built record across their reads, as a join would, and a failed instance read costs no
/// record read and sends no record.
async fn resolve_local(
    records: &mut u64,
    store: &StoreInternal,
    args: &LoreRevisionGraphBranchResolveArgs,
) -> LocationAnswer<()> {
    let ctx = local_read_context(store, args.repository);
    let branch = match branch::lookup_name_local_unwritten(ctx.clone(), args.name.as_str())
        .await
        .forward_any::<RevisionGraphError>("looking up the branch name")?
    {
        Some(named) => named.branch(),
        None => return Ok(Err(LocationMiss::BranchNotFound)),
    };
    let on_branch = instance::records_on_branch(&ctx, branch)
        .await
        .forward_any::<RevisionGraphError>("reading the instances on the branch")?;
    let Some(record) = local_branch_record(ctx, branch).await? else {
        return Ok(Err(LocationMiss::BranchNotFound));
    };
    *records += emit_local_branch(args.id, record, on_branch);
    Ok(Ok(()))
}
