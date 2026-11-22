// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_graph_branch_metadata_get`: read a branch's metadata at each location a call
//! consults, the remote first.
//!
//! Each location that holds the branch reports one branch metadata record per entry of its
//! metadata a record can carry (see `record::emit_branch_metadata`), or only for the entry the
//! call's key names, and each location ends with its location complete event.

use std::sync::Arc;

use futures::FutureExt;
use lore_base::error::BranchNotFound;
use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::branch;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::interface::LoreString;
use lore_revision::metadata::Metadata;
use lore_revision::protocol::RemoteEndpoint;
use lore_revision::repository::RepositoryContext;
use lore_transport::Revision;

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
use crate::revision_graph::record::emit_branch_metadata;
use crate::revision_graph::record::emit_location_complete;
use crate::storage::handle::LoreStore;
use crate::storage::store::PerCallFlags;
use crate::storage::store::StoreInternal;

/// Arguments for `lore_revision_graph_branch_metadata_get`.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(branch_metadata_get_impl)]
pub struct LoreRevisionGraphBranchMetadataGetArgs {
    /// Caller-chosen id echoed back in every event this surface defines for the call. Separate
    /// from `globals.correlation_id`.
    pub id: u64,
    /// Open storage handle the call reaches the repository through
    pub store: LoreStore,
    /// Repository the branch is in
    pub repository: Partition,
    /// Branch whose metadata is read. Not zero.
    pub branch: BranchId,
    /// Key of the one entry to report, or empty to report every entry a record can carry; the
    /// others are left out, with one `LORE_EVENT_LOG` warning from each location that leaves any
    /// out
    pub key: LoreString,
}

/// Read a branch's metadata at each location the call consults.
pub fn branch_metadata_get(
    globals: LoreGlobalArgs,
    args: LoreRevisionGraphBranchMetadataGetArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    dispatch_call(globals, args, callback, branch_metadata_get_impl)
}

fn branch_metadata_get_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionGraphBranchMetadataGetArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    let per_call = PerCallFlags::from_globals(&globals);
    revision_graph_call(
        globals,
        callback,
        args.store,
        args,
        branch_metadata_get,
        async move |store, args| {
            if args.branch.is_zero() {
                return Err(RevisionGraphError::invalid("the branch id is zero"));
            }
            let locations = locations(&store, per_call)?;
            let mut outcome = CallOutcome::default();
            if let Some(remote) = locations.remote() {
                let mut records = 0;
                let answer = get_remote(&mut records, &store, &args, remote).await;
                emit_location_complete(args.id, LoreBranchLocation::Remote, records, &answer);
                outcome.add(answer);
            }
            if locations.local() {
                let mut records = 0;
                let answer = get_local(&mut records, &store, &args).await;
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

/// Reads the metadata of the branch `args` names at the remote that `remote`, the storage
/// handle's endpoint, reaches, and sends its records, adding the record events sent to `records`.
/// The remote's revision service answers the hash of the branch's metadata blob
/// ([`remote_metadata_hash`]), which is read through the remote's read context.
async fn get_remote(
    records: &mut u64,
    store: &StoreInternal,
    args: &LoreRevisionGraphBranchMetadataGetArgs,
    remote: &Arc<RemoteEndpoint>,
) -> LocationAnswer<()> {
    let (ctx, metadata_hash) = {
        let (ctx, service) = remote_revision_service(store, args.repository, remote).await?;
        match remote_metadata_hash(service.as_ref(), args.branch).await? {
            Some(hash) => (ctx, hash),
            None => return Ok(Err(LocationMiss::BranchNotFound)),
        }
    };
    let metadata = read_remote_metadata(&ctx, metadata_hash).await?;
    *records += emit_branch_metadata(
        args.id,
        LoreBranchLocation::Remote,
        args.branch,
        &metadata,
        Option::from(&args.key),
    );
    Ok(Ok(()))
}

/// Reads the metadata of the branch `args` names in the local store, as
/// [`branch::local_metadata`] reads it, and sends its records, adding the record events sent to
/// `records`. A metadata key that names a blob the store doesn't hold leaves the store without
/// metadata for the branch.
async fn get_local(
    records: &mut u64,
    store: &StoreInternal,
    args: &LoreRevisionGraphBranchMetadataGetArgs,
) -> LocationAnswer<()> {
    let ctx = local_read_context(store, args.repository);
    let Some((_, metadata)) = branch::local_metadata(ctx, args.branch)
        .await
        .forward_any::<RevisionGraphError>("reading the branch metadata")?
    else {
        return Ok(Err(LocationMiss::BranchNotFound));
    };
    *records += emit_branch_metadata(
        args.id,
        LoreBranchLocation::Local,
        args.branch,
        &metadata,
        Option::from(&args.key),
    );
    Ok(Ok(()))
}

/// The hash of the metadata blob of `branch` that `service`, the remote's revision service,
/// answers: `None` for a zero hash or `NotFound`, the answers of a remote without the branch. Lore
/// Server also answers `NotFound` where its read of the hash fails, so that failure is `None` too.
///
/// Returns the request's future mapped to the hash, without a future of its own.
#[inline]
pub(crate) fn remote_metadata_hash(
    service: &dyn Revision,
    branch: BranchId,
) -> impl Future<Output = Result<Option<Hash>, RevisionGraphError>> + '_ {
    service
        .branch_metadata_get(branch)
        .map(|answer| match answer {
            Ok(hash) => Ok((!hash.is_zero()).then_some(hash)),
            Err(err) if err.is_not_found() => Ok(None),
            Err(err) => Err(err)
                .forward_any::<RevisionGraphError>("querying the remote for the branch metadata"),
        })
}

/// The metadata blob `hash`, read at the remote through `ctx`, the remote's read context. A blob
/// the remote lacks fails with `AddressNotFound`.
///
/// Returns the read's future with its error forwarded, without a future of its own.
#[inline]
pub(crate) fn read_remote_metadata(
    ctx: &Arc<RepositoryContext>,
    hash: Hash,
) -> impl Future<Output = Result<Metadata, RevisionGraphError>> + use<> {
    Metadata::deserialize(ctx.clone(), hash)
        .map(|read| read.forward_any::<RevisionGraphError>("reading the branch metadata"))
}
