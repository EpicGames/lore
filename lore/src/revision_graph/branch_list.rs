// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_graph_branch_list`: list a repository's branches at each location a call
//! consults, the remote first.
//!
//! Each location reports one branch record per branch, and the local store follows each of its
//! records with the instance records of the instances on that branch. Each location ends with its
//! location complete event. Each branch record is built by a task of its own, at most 16 at a time
//! per location, and sent from the call's task.

use std::collections::HashSet;
use std::hash::BuildHasherDefault;
use std::sync::Arc;

use futures::Stream;
use futures::StreamExt;
use futures::stream;
use lore_base::lore_spawn;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_base::types::Partition;
use lore_base::types::PassThroughHasher;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::branch;
use lore_revision::instance;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::interface::LoreString;
use lore_revision::protocol::RemoteEndpoint;
use lore_revision::repository::RepositoryContext;
use lore_transport::BranchRecord;
use tokio::task::JoinError;
use tokio::task::JoinSet;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_graph::RevisionGraphError;
use crate::revision_graph::call::revision_graph_call;
use crate::revision_graph::locations::local_read_context;
use crate::revision_graph::locations::locations;
use crate::revision_graph::locations::remote_revision_service;
use crate::revision_graph::record::BranchRecordOut;
use crate::revision_graph::record::branch_record_from_metadata;
use crate::revision_graph::record::emit_branch;
use crate::revision_graph::record::emit_local_branch;
use crate::revision_graph::record::emit_location_complete;
use crate::revision_graph::record::live_branch_record;
use crate::revision_graph::record::remote_branch_record;
use crate::storage::handle::LoreStore;
use crate::storage::store::PerCallFlags;
use crate::storage::store::StoreInternal;

/// The most record builders a location runs at once.
#[lore_macro::test_pub]
const RECORDS_IN_FLIGHT: usize = 16;

/// Arguments for `lore_revision_graph_branch_list`.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(branch_list_impl)]
pub struct LoreRevisionGraphBranchListArgs {
    /// Caller-chosen id echoed back in every event this surface defines for the call. Separate
    /// from `globals.correlation_id`.
    pub id: u64,
    /// Open storage handle the call reaches the repository through
    pub store: LoreStore,
    /// Repository whose branches are listed
    pub repository: Partition,
    /// Non-zero to list archived branches too, each with the latest revision it kept when it was
    /// archived
    pub include_archived: u8,
    /// When non-empty, lists only the branches whose creator equals it exactly, compared
    /// case-sensitively
    pub creator: LoreString,
}

/// List the branches of a repository at each location the call consults.
pub async fn branch_list(
    globals: LoreGlobalArgs,
    args: LoreRevisionGraphBranchListArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, branch_list_impl).await
}

fn branch_list_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionGraphBranchListArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    let per_call = PerCallFlags::from_globals(&globals);
    revision_graph_call(
        globals,
        callback,
        args.store,
        args,
        branch_list,
        async move |store, args| {
            let locations = locations(&store, per_call)?;
            let mut outcome = Ok(());
            if let Some(remote) = locations.remote() {
                let mut records = 0;
                outcome = list_remote(&mut records, &store, &args, remote).await;
                emit_location_complete(args.id, LoreBranchLocation::Remote, records, &outcome);
            }
            if locations.local() {
                let mut records = 0;
                let local = list_local(&mut records, &store, &args).await;
                emit_location_complete(args.id, LoreBranchLocation::Local, records, &local);
                outcome = outcome.and(local);
            }
            outcome
        },
    )
}

/// Lists the branches at the remote that `remote`, the storage handle's endpoint, reaches, as
/// `args` asks, one record per branch ([`one_record_per_branch`]), and adds the record events sent
/// to `records`.
async fn list_remote(
    records: &mut u64,
    store: &StoreInternal,
    args: &LoreRevisionGraphBranchListArgs,
    remote: &Arc<RemoteEndpoint>,
) -> Result<(), RevisionGraphError> {
    let filter = creator_filter(args);
    let (ctx, listed) = {
        let (ctx, service) = remote_revision_service(store, args.repository, remote).await?;
        let listed = service
            .branch_list(filter, args.include_archived != 0)
            .await
            .forward_any::<RevisionGraphError>("listing the remote's branches")?
            .list;
        (ctx, listed)
    };
    build_records(
        records,
        stream::iter(one_record_per_branch(listed)),
        |record| remote_record_builder(filter, &ctx, record),
        |record| emit_branch(args.id, LoreBranchLocation::Remote, record),
    )
    .await
}

/// `listed`, the remote's records, sorted by branch in place with any one of each branch's records
/// kept: the remote lists a branch once per name that maps to it, and its store can map two names
/// to one branch.
///
/// Takes the records by value: sorted in the caller, the borrowed vector would stay reserved in the
/// caller's future across its next await.
#[lore_macro::test_pub]
fn one_record_per_branch(mut listed: Vec<BranchRecord>) -> Vec<BranchRecord> {
    listed.sort_unstable_by_key(|record| record.id);
    listed.dedup_by_key(|record| record.id);
    listed
}

/// Lists the branches in the local store as `args` asks, each followed by the instance records of
/// the instances on it, and adds the record events sent to `records`.
///
/// The registered instances are read once, before the branches, for every branch, so the creator
/// filter doesn't narrow those reads. The live branches are those a name maps to, each listed once
/// however many names map to it. With `include_archived`, only the blobs of the branch metadata
/// keys whose values the live pass didn't read are read: those of the archived branches, of a live
/// branch whose metadata changed after the live pass read it, and of a live branch whose blob the
/// live pass missed. A blob that names a live branch adds no record. A zero branch id or metadata
/// value, which the built-in mutable stores never list and a mutable store plugin may, names no
/// branch. A create writes its metadata key before it claims its name, so a branch created during
/// the call, or one whose create loses its name claim, can list as archived. A live branch whose
/// id an older version derived from its name, and whose name has no mapping, lists only with
/// `include_archived`, and as archived, until a file-system lookup of its name maps it; one without
/// a metadata key, or whose blob the store lacks, doesn't list.
///
/// In both passes the creator filter reads the creator from the metadata blob, so a branch it
/// leaves out costs no load of its latest or its latest status.
async fn list_local(
    records: &mut u64,
    store: &StoreInternal,
    args: &LoreRevisionGraphBranchListArgs,
) -> Result<(), RevisionGraphError> {
    let filter = creator_filter(args).map(Arc::<str>::from);
    let include_archived = args.include_archived != 0;
    let ctx = local_read_context(store, args.repository);
    let mut instances = instance::records_by_branch(&ctx)
        .await
        .forward_any::<RevisionGraphError>("reading the instances on each branch")?;
    let mut live_metadata = HashSet::<Hash, BuildHasherDefault<PassThroughHasher>>::default();
    let mut live_branches = HashSet::new();
    let branches = branch::list(ctx.clone())
        .await
        .forward_any::<RevisionGraphError>("listing the branches")?;
    build_records(
        records,
        branches,
        |branch| {
            (!branch.is_zero() && live_branches.insert(branch))
                .then(|| live_branch_record(ctx.clone(), branch, filter.clone()))
        },
        |live| {
            let Some((metadata, record)) = live else {
                return 0;
            };
            if include_archived {
                live_metadata.insert(metadata);
            }
            record.map_or(0, |record| {
                let on_branch = instances.remove(&record.branch).unwrap_or_default();
                emit_local_branch(args.id, record, on_branch)
            })
        },
    )
    .await?;
    if !include_archived {
        return Ok(());
    }
    let metadata_keys = ctx
        .read_mutable_store()
        .list(args.repository, KeyType::BranchMetadata)
        .await
        .forward_any::<RevisionGraphError>("listing the branch metadata keys")?;
    build_records(
        records,
        metadata_keys,
        |(_, metadata)| {
            (!metadata.is_zero() && !live_metadata.contains(&metadata))
                .then(|| archived_branch_record(ctx.clone(), metadata, filter.clone()))
        },
        |record| match record {
            Some(record) if !live_branches.contains(&record.branch) => {
                let on_branch = instances.remove(&record.branch).unwrap_or_default();
                emit_local_branch(args.id, record, on_branch)
            }
            _ => 0,
        },
    )
    .await
}

/// The record of the archived branch whose metadata blob in the local store is `metadata_hash`,
/// read through `ctx`, where `filter` keeps the blob's creator: the blob, which names the branch,
/// then the latest revision the branch kept and its latest status. `None` where the store doesn't
/// hold the blob, where the blob names no branch, an entry Lore Server's list leaves out too, and
/// where `filter` leaves the branch out.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
///
/// # Errors
///
/// A failed or corrupt read, and a blob without a name that `filter` keeps.
#[allow(clippy::manual_async_fn)]
fn archived_branch_record(
    ctx: Arc<RepositoryContext>,
    metadata_hash: Hash,
    filter: Option<Arc<str>>,
) -> impl Future<Output = Result<Option<BranchRecordOut>, RevisionGraphError>> {
    async move {
        let Some(metadata) = branch::local_metadata_blob(ctx.clone(), metadata_hash)
            .await
            .forward_any::<RevisionGraphError>("reading the branch metadata blob")?
        else {
            return Ok(None);
        };
        let Ok(branch) = branch::id(&metadata) else {
            return Ok(None);
        };
        if !branch::metadata_creator_matches(filter.as_deref(), &metadata) {
            return Ok(None);
        }
        let mut record = branch_record_from_metadata(ctx, branch, metadata).await?;
        record.archived = true;
        Ok(Some(record))
    }
}

/// The builder of the record of `record`, one of the remote's, read through `ctx`, where `filter`
/// keeps its creator. The remote filters by creator too, and a remote that ignores the filter
/// yields the same records.
#[lore_macro::test_pub]
#[inline]
fn remote_record_builder(
    filter: Option<&str>,
    ctx: &Arc<RepositoryContext>,
    record: BranchRecord,
) -> Option<impl Future<Output = Result<BranchRecordOut, RevisionGraphError>> + use<>> {
    branch::creator_matches(filter, &record.creator)
        .then(|| remote_branch_record(ctx.clone(), record))
}

/// The creator filter `args` asks for: `None` for an empty `creator`, which keeps every branch.
fn creator_filter(args: &LoreRevisionGraphBranchListArgs) -> Option<&str> {
    (!args.creator.is_empty()).then(|| args.creator.as_str())
}

/// Builds a record for each of `items` that `build` returns a builder for, each builder in a task
/// of its own and at most [`RECORDS_IN_FLIGHT`] at once, and passes each record to `emit` in the
/// calling task, adding the number of record events `emit` sent to `records`. After each spawn the
/// calling task passes on the records of the builders that have finished, and while
/// [`RECORDS_IN_FLIGHT`] run it waits for the next to finish.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
///
/// # Errors
///
/// The error of the first builder that fails, which ends the run once every builder still running
/// has stopped, so that no read outlives the run.
#[lore_macro::test_pub]
#[allow(clippy::manual_async_fn)]
fn build_records<I, T, B>(
    records: &mut u64,
    mut items: impl Stream<Item = I> + Unpin,
    mut build: impl FnMut(I) -> Option<B>,
    mut emit: impl FnMut(T) -> u64,
) -> impl Future<Output = Result<(), RevisionGraphError>>
where
    T: Send + 'static,
    B: Future<Output = Result<T, RevisionGraphError>> + Send + 'static,
{
    async move {
        let mut builders = JoinSet::new();
        let outcome = async {
            loop {
                let Some(item) = items.next().await else {
                    break;
                };
                let Some(builder) = build(item) else {
                    continue;
                };
                lore_spawn!(builders, builder);
                while let Some(joined) = builders.try_join_next() {
                    *records += emit(built(joined)?);
                }
                if builders.len() == RECORDS_IN_FLIGHT
                    && let Some(joined) = builders.join_next().await
                {
                    *records += emit(built(joined)?);
                }
            }
            while let Some(joined) = builders.join_next().await {
                *records += emit(built(joined)?);
            }
            Ok(())
        }
        .await;
        if outcome.is_err() {
            builders.shutdown().await;
        }
        outcome
    }
}

/// The output of a record builder's task, a task that didn't run to its end as an internal error.
fn built<T>(
    joined: Result<Result<T, RevisionGraphError>, JoinError>,
) -> Result<T, RevisionGraphError> {
    joined.unwrap_or_else(|err| {
        Err(RevisionGraphError::internal_with_context(
            err,
            "running a branch record builder",
        ))
    })
}
