// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_graph_branch_sync`: sync a branch's local latest to a target revision.
//!
//! The call looks the target up at the remote where it consults the remote, and otherwise in the
//! local store ([`sync_target`]), and writes the local store alone. A branch the remote holds is
//! brought in first where the local store lacks its metadata key, or, for a branch live at the
//! remote, where its name doesn't map to it there. The call moves the local latest to the target
//! where the local latest is zero or the target descends from it along first parents, or with
//! `force`, with a compare-and-swap from the local latest it read. A local sync with no target
//! reports the instances on the branch and writes nothing. One synced event reports the outcome
//! once the call has read the local latest.

use std::sync::Arc;

use lore_base::error::BranchAdvanced;
use lore_base::error::BranchNotFound;
use lore_base::error::Divergent;
use lore_base::types::BranchId;
use lore_base::types::BranchPoint;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_macro::ValidateText;
use lore_revision::branch;
use lore_revision::branch::BranchLatestStatus;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_graph::LoreRevisionGraphBranchSyncedEventData;
use lore_revision::find;
use lore_revision::find::NumberWalk;
use lore_revision::history;
use lore_revision::instance;
use lore_revision::metadata::Metadata;
use lore_revision::protocol::RemoteEndpoint;
use lore_revision::repository::RepositoryContext;
use lore_transport::BranchRecord;
use lore_transport::Revision;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_graph::LoreRevisionIdentifier;
use crate::revision_graph::RevisionForm;
use crate::revision_graph::RevisionGraphError;
use crate::revision_graph::call::revision_graph_call;
use crate::revision_graph::locations::LocationMiss;
use crate::revision_graph::locations::branch_record_at;
use crate::revision_graph::locations::full_context;
use crate::revision_graph::locations::locations;
use crate::revision_graph::locations::remote_revision_service;
use crate::revision_graph::record::LocalLatest;
use crate::revision_graph::record::emit_instances;
use crate::revision_graph::record::local_latest;
use crate::revision_graph::revision_resolve::listed_at;
use crate::revision_graph::revision_resolve::numbered_at;
use crate::revision_graph::revision_resolve::revision_record_at;
use crate::storage::handle::LoreStore;
use crate::storage::store::PerCallFlags;
use crate::storage::store::StoreInternal;

/// Options of `lore_revision_graph_branch_sync`.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, ValidateText, bitcode::Encode, bitcode::Decode,
)]
pub struct LoreRevisionGraphBranchSyncOptions {
    /// Non-zero: move the local latest to the target even where the target isn't a first-parent
    /// descendant of it
    pub force: u8,
    /// Non-zero: sync only while the local latest is this revision. Zero sets no expectation.
    pub expected_local_latest: Hash,
}

/// Arguments for `lore_revision_graph_branch_sync`.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(branch_sync_impl)]
pub struct LoreRevisionGraphBranchSyncArgs {
    /// Caller-chosen id echoed back in every event this surface defines for the call. Separate
    /// from `globals.correlation_id`.
    pub id: u64,
    /// Open storage handle the call reaches the repository through
    pub store: LoreStore,
    /// Repository the branch is synced in
    pub repository: Partition,
    /// Branch whose local latest the call syncs. Not zero.
    pub branch: BranchId,
    /// The revision to sync the local latest to: a revision on the branch, the branch point the
    /// branch starts from, or a revision number on the branch or its latest. All zero names the
    /// branch's latest.
    pub target: LoreRevisionIdentifier,
    /// How the call syncs
    pub options: LoreRevisionGraphBranchSyncOptions,
}

/// The location a sync looks its target up in, with the branch's latest there and what gives the
/// branch's stack.
#[lore_macro::test_pub]
#[derive(Clone, Copy)]
pub(crate) enum TargetLocation<'a> {
    /// The remote, through its revision service, whose record of the branch names `latest` and
    /// `stack`.
    Remote {
        service: &'a Arc<dyn Revision>,
        latest: Hash,
        stack: &'a [BranchPoint],
    },
    /// The local store, where the branch's latest is `latest`, zero for none, and its metadata key
    /// holds `metadata`, the hash of the blob that records its stack.
    Local {
        latest: Hash,
        metadata: Option<Hash>,
    },
}

/// A sync's target: the revision it moves the local latest to, and whether the remote's history
/// line for the branch, the remote latest and the revisions it reaches along first parents, is
/// known to hold it.
#[lore_macro::test_pub]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SyncTarget {
    /// The revision the target names.
    pub revision: Hash,
    /// Whether the remote's history line for the branch is known to hold `revision`.
    pub on_remote_line: bool,
}

/// The target `form` names for a sync of `branch`, looked up at `at` through `ctx`, which reads the
/// remote alone at the remote. `None` for the latest of `branch` in the local store, which names no
/// target.
///
/// The latest of `branch` is the remote latest. A revision number names the revision the remote's
/// revision list holds with it ([`numbered_at`]), or the one a walk of the line from the local
/// latest finds ([`find::revision_by_number_local`]). A hash names the revision the location holds
/// under it, which must be a revision whose own branch is `branch`, or the branch point the
/// branch's stack at the location starts from ([`branch_point`]), read only for a revision that
/// doesn't record `branch` as its own.
///
/// The remote's history line for `branch` holds the remote latest, a revision its revision list
/// numbers, the branch point, and a hash the remote confirms ([`confirmed_on_remote_line`]). A
/// target looked up in the local store isn't known to be on it.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
///
/// # Errors
///
/// `InvalidArguments` for a branch form that names another branch and for a hash off the branch,
/// a revision whose branch the location doesn't record included. `RevisionNotFound` for a revision
/// the location doesn't hold, a zero remote latest included. A failed or corrupt read or a failed
/// request fails with its error.
#[lore_macro::test_pub]
#[allow(clippy::manual_async_fn)]
pub(crate) fn sync_target<'a>(
    ctx: &'a Arc<RepositoryContext>,
    at: TargetLocation<'a>,
    branch: BranchId,
    form: RevisionForm,
) -> impl Future<Output = Result<Option<SyncTarget>, RevisionGraphError>> + 'a {
    async move {
        let missing = || LocationMiss::RevisionNotFound.error(form);
        let target = |revision, on_remote_line| {
            Ok(Some(SyncTarget {
                revision,
                on_remote_line,
            }))
        };
        let named = match form {
            RevisionForm::Hash(hash) => {
                let Some((number, recorded)) = revision_record_at(ctx, hash).await? else {
                    return Err(missing());
                };
                if recorded == branch {
                    return target(
                        hash,
                        confirmed_on_remote_line(at, branch, number, hash).await?,
                    );
                }
                if branch_point(ctx, at).await? != Some(hash) {
                    return Err(RevisionGraphError::invalid(format!(
                        "the revision {hash} doesn't record branch {branch} as its own and isn't \
                         its branch point"
                    )));
                }
                return target(hash, matches!(at, TargetLocation::Remote { .. }));
            }
            RevisionForm::Latest(named) | RevisionForm::Number(named, _) => named,
        };
        if named != branch {
            return Err(RevisionGraphError::invalid(format!(
                "the target names branch {named}, not the branch synced, {branch}"
            )));
        }
        match (form, at) {
            (RevisionForm::Number(_, number), TargetLocation::Remote { service, .. }) => {
                match numbered_at(ctx, Some(service), branch, number).await? {
                    Ok((revision, _)) => target(revision, true),
                    Err(miss) => Err(miss.error(form)),
                }
            }
            (RevisionForm::Number(_, number), TargetLocation::Local { latest, .. }) => {
                match find::revision_by_number_local(ctx, latest, number)
                    .await
                    .forward_any::<RevisionGraphError>("walking the local latest's line")?
                {
                    NumberWalk::Found { revision, .. } => target(revision, false),
                    NumberWalk::Absent | NumberWalk::Missing(_) => Err(missing()),
                }
            }
            (_, TargetLocation::Remote { latest, .. }) if latest.is_zero() => Err(missing()),
            (_, TargetLocation::Remote { latest, .. }) => target(latest, true),
            (_, TargetLocation::Local { .. }) => Ok(None),
        }
    }
}

/// Whether the remote at `at` confirms that its history line for `branch` holds `revision`, a
/// revision whose own branch is `branch`, numbered `number`: where `revision` is the remote latest,
/// or the revision the remote's revision list for `branch` numbers `number` ([`listed_at`]), which
/// takes one request. False in the local store, which can't confirm it.
///
/// # Errors
///
/// A failed request.
async fn confirmed_on_remote_line(
    at: TargetLocation<'_>,
    branch: BranchId,
    number: u64,
    revision: Hash,
) -> Result<bool, RevisionGraphError> {
    let TargetLocation::Remote {
        service, latest, ..
    } = at
    else {
        return Ok(false);
    };
    if revision == latest {
        return Ok(true);
    }
    Ok(listed_at(service, branch, number)
        .await?
        .flatten()
        .is_some_and(|(listed, _)| listed == revision))
}

/// The branch point the branch's stack at `at` starts from: the first of the remote's record's
/// stack, or of the stack the local metadata blob records, read from the local store through `ctx`.
/// `None` for an empty stack, and where the local store holds no metadata blob.
///
/// # Errors
///
/// A failed or corrupt read.
async fn branch_point(
    ctx: &Arc<RepositoryContext>,
    at: TargetLocation<'_>,
) -> Result<Option<Hash>, RevisionGraphError> {
    let first = |stack: &[BranchPoint]| stack.first().map(|point| point.revision);
    let metadata = match at {
        TargetLocation::Remote { stack, .. } => return Ok(first(stack)),
        TargetLocation::Local { metadata: None, .. } => return Ok(None),
        TargetLocation::Local {
            metadata: Some(metadata),
            ..
        } => metadata,
    };
    Ok(branch::local_metadata_blob(ctx.clone(), metadata)
        .await
        .forward_any::<RevisionGraphError>("reading the branch metadata")?
        .and_then(|blob| first(&branch::stack(&blob))))
}

/// The branch the remote holds, as its record reports it, with the remote's read context and its
/// revision service, which look the target up there.
struct RemoteBranch {
    ctx: Arc<RepositoryContext>,
    service: Arc<dyn Revision>,
    record: BranchRecord,
}

/// Sync a branch's local latest to a target revision.
pub fn branch_sync(
    globals: LoreGlobalArgs,
    args: LoreRevisionGraphBranchSyncArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    dispatch_call(globals, args, callback, branch_sync_impl)
}

fn branch_sync_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionGraphBranchSyncArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    let per_call = PerCallFlags::from_globals(&globals);
    revision_graph_call(
        globals,
        callback,
        args.store,
        args,
        branch_sync,
        async move |store, args| {
            let form = validate(&args)?;
            let locations = locations(&store, per_call)?;
            let remote = locations.remote();
            let ctx = full_context(&store, args.repository, remote).await?;
            let mut observed = None;
            let synced = sync(&mut observed, &store, &ctx, remote, &args, form).await;
            if let Some(previous) = observed {
                let local_latest = *synced.as_ref().unwrap_or(&previous);
                emit_synced(args.id, args.branch, previous, local_latest);
            }
            synced.map(drop)
        },
    )
}

/// The revision `args` syncs the branch to, after checking the arguments, as a sync does before
/// any read: an all-zero target names the branch's latest.
///
/// # Errors
///
/// `InvalidArguments` for a zero branch and a malformed target.
fn validate(args: &LoreRevisionGraphBranchSyncArgs) -> Result<RevisionForm, RevisionGraphError> {
    if args.branch.is_zero() {
        return Err(RevisionGraphError::invalid("the branch id is zero"));
    }
    if args.target == LoreRevisionIdentifier::default() {
        return Ok(RevisionForm::Latest(args.branch));
    }
    args.target.form()
}

/// Syncs the local latest of the branch `args` names to the revision `form` names, through `ctx`,
/// the call's context, and answers the local latest it leaves. With `remote`, the storage handle's
/// endpoint, the target is looked up at the remote `store` reaches through it, and otherwise in the
/// local store, as [`resolve`] reads it. `observed` holds the local latest the sync last read, from
/// its first read on.
///
/// The target equal to the local latest writes no local latest; where it is the remote latest, the
/// branch is recorded in sync with the remote ([`branch::record_in_sync`]), and where the remote's
/// history line for the branch is otherwise known to hold it ([`SyncTarget`]), a divergent latest
/// status is cleared ([`branch::clear_divergent_status`]). Otherwise the local latest moves where
/// it is zero, with `force`, or where the target descends from it ([`fast_forwards`]), with a
/// compare-and-swap from the value read: convergent with the last synchronized revision stored
/// where the target is the remote latest, convergent where the remote's history line for the branch
/// is known to hold the target ([`SyncTarget`]), and divergent otherwise.
///
/// A local sync with no target, the latest of a branch looked up in the local store, writes
/// nothing: it sends the instance records of the instances on the branch, the local current, and
/// answers the local latest.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
///
/// # Errors
///
/// [`resolve`]'s errors; `BranchAdvanced` for a compare-and-swap another writer won, whose value
/// `observed` then holds where its read succeeds; `Divergent` for a target that doesn't descend
/// from the local latest; the walk's error; and a failed write.
#[allow(clippy::manual_async_fn)]
fn sync<'a>(
    observed: &'a mut Option<Hash>,
    store: &'a StoreInternal,
    ctx: &'a Arc<RepositoryContext>,
    remote: Option<&'a Arc<RemoteEndpoint>>,
    args: &'a LoreRevisionGraphBranchSyncArgs,
    form: RevisionForm,
) -> impl Future<Output = Result<Hash, RevisionGraphError>> + 'a {
    async move {
        let branch = args.branch;
        let Resolved {
            local:
                LocalLatest {
                    latest: previous,
                    divergent,
                },
            target,
            remote_latest,
        } = resolve(observed, store, ctx, remote, args, form).await?;
        let Some(SyncTarget {
            revision: target,
            on_remote_line,
        }) = target
        else {
            let on_branch = instance::records_on_branch(ctx, branch)
                .await
                .forward_any::<RevisionGraphError>("reading the instances on the branch")?;
            emit_instances(args.id, branch, on_branch);
            return Ok(previous);
        };
        if target == previous {
            if remote_latest == Some(target) {
                branch::record_in_sync(ctx, branch, target, divergent)
                    .await
                    .forward_any::<RevisionGraphError>("clearing the latest status")?;
            } else if on_remote_line && divergent {
                branch::clear_divergent_status(ctx.clone(), branch, target)
                    .await
                    .forward_any::<RevisionGraphError>("clearing the latest status")?;
            }
            return Ok(previous);
        }
        if !previous.is_zero()
            && args.options.force == 0
            && !fast_forwards(ctx, target, previous).await?
        {
            return Err(RevisionGraphError::from(Divergent));
        }
        let written = if remote_latest == Some(target) {
            branch::store_synced_latest(ctx, branch, previous, target).await
        } else {
            let status = if on_remote_line {
                BranchLatestStatus::Convergent
            } else {
                BranchLatestStatus::Divergent
            };
            branch::store_latest(ctx.clone(), branch, previous, target, status).await
        };
        let lost = match written {
            Ok(()) => return Ok(target),
            Err(err) if err.is_branch_advanced() => err,
            Err(err) => {
                return Err(err).forward_any::<RevisionGraphError>("storing the local latest");
            }
        };
        if let Ok(won) = branch::mutable_value(ctx.clone(), branch::LATEST, branch).await {
            *observed = Some(won.unwrap_or_default());
        }
        Err(lost).forward_any::<RevisionGraphError>("storing the local latest")
    }
}

/// What a sync reads before it decides: the local latest, the target, and the remote latest where
/// the sync consults the remote.
struct Resolved {
    local: LocalLatest,
    target: Option<SyncTarget>,
    remote_latest: Option<Hash>,
}

/// The local latest of the branch `args` names, read through `ctx`, and the target `form` names,
/// as a sync reads them before it decides, with the remote latest where `remote` is the storage
/// handle's endpoint, through which the target is looked up at the remote `store` reaches, and
/// otherwise in the local store ([`sync_target`]). `observed` holds the local latest read.
///
/// The local store holds the branch where it holds the branch's metadata key, and the local latest
/// is read there first. A local latest without the metadata key, as a tree commit leaves it,
/// doesn't make the local store hold the branch. The remote's record of the branch is read with
/// one branch query. Where the local store lacks the metadata key, or the mapping of the name the
/// remote's record of a live branch gives doesn't name the branch, the branch is brought in
/// ([`bring_in`]), which writes no local latest, and the local latest of a branch whose metadata
/// key the local store lacked is read after it.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
///
/// # Errors
///
/// `BranchNotFound` for a branch the remote doesn't hold, and, without a remote, for one the local
/// store doesn't hold; `BranchAdvanced` for a local latest other than `expected_local_latest`;
/// [`sync_target`]'s errors; and a failed read or request.
#[allow(clippy::manual_async_fn)]
fn resolve<'a>(
    observed: &'a mut Option<Hash>,
    store: &'a StoreInternal,
    ctx: &'a Arc<RepositoryContext>,
    remote: Option<&'a Arc<RemoteEndpoint>>,
    args: &'a LoreRevisionGraphBranchSyncArgs,
    form: RevisionForm,
) -> impl Future<Output = Result<Resolved, RevisionGraphError>> + 'a {
    async move {
        let branch = args.branch;
        let metadata = branch::mutable_value(ctx.clone(), branch::METADATA, branch)
            .await
            .forward_any::<RevisionGraphError>("loading the metadata key")?;
        let mut local = match metadata {
            Some(_) => Some(local_latest(ctx.clone(), branch).await?),
            None => None,
        };
        *observed = local.map(|read| read.latest);
        let at_remote = match remote {
            Some(remote) => Some(remote_branch(store, args.repository, remote, branch).await?),
            None => None,
        };
        if let Some(RemoteBranch { record, .. }) = &at_remote {
            if brings_in(ctx, metadata.is_some(), record).await? {
                bring_in(ctx, record).await?;
            }
            if local.is_none() {
                let read = local_latest(ctx.clone(), branch).await?;
                *observed = Some(read.latest);
                local = Some(read);
            }
        }
        let Some(local) = local else {
            return Err(RevisionGraphError::from(BranchNotFound {
                branch: branch.to_string(),
            }));
        };
        let expected = args.options.expected_local_latest;
        if !expected.is_zero() && expected != local.latest {
            return Err(RevisionGraphError::from(BranchAdvanced));
        }
        let Some(RemoteBranch {
            ctx: remote_ctx,
            service,
            record,
        }) = at_remote
        else {
            let at = TargetLocation::Local {
                latest: local.latest,
                metadata,
            };
            let target = sync_target(ctx, at, branch, form).await?;
            return Ok(Resolved {
                local,
                target,
                remote_latest: None,
            });
        };
        let at = TargetLocation::Remote {
            service: &service,
            latest: record.latest,
            stack: &record.stack,
        };
        let target = sync_target(&remote_ctx, at, branch, form).await?;
        Ok(Resolved {
            local,
            target,
            remote_latest: Some(record.latest),
        })
    }
}

/// The remote's record of `branch` in `repository`, read with one branch query at the remote that
/// `store` reaches through `remote`, the storage handle's endpoint.
///
/// # Errors
///
/// `BranchNotFound` for a branch the remote doesn't hold, and the connect's error and a failed
/// request.
async fn remote_branch(
    store: &StoreInternal,
    repository: Partition,
    remote: &Arc<RemoteEndpoint>,
    branch: BranchId,
) -> Result<RemoteBranch, RevisionGraphError> {
    let (ctx, service) = remote_revision_service(store, repository, remote).await?;
    let Some(record) = branch_record_at(
        service.as_ref(),
        branch,
        "querying the remote for the branch",
    )
    .await?
    else {
        return Err(RevisionGraphError::from(BranchNotFound {
            branch: branch.to_string(),
        }));
    };
    Ok(RemoteBranch {
        ctx,
        service,
        record,
    })
}

/// Whether a sync brings in the branch `record`, the remote's record, describes: where the local
/// store `ctx` reads holds no metadata key of the branch, `has_metadata` false, or, for a branch
/// live at the remote, where the mapping of the name `record` gives names no branch or another one.
///
/// # Errors
///
/// A failed read.
async fn brings_in(
    ctx: &Arc<RepositoryContext>,
    has_metadata: bool,
    record: &BranchRecord,
) -> Result<bool, RevisionGraphError> {
    if !has_metadata {
        return Ok(true);
    }
    if record.deleted {
        return Ok(false);
    }
    match branch::load_name_to_id_local(ctx.clone(), &record.name).await {
        Ok(held) => Ok(held != record.id),
        Err(err) if err.is_branch_not_found() => Ok(true),
        Err(err) => Err(err).forward_any::<RevisionGraphError>("loading the name's mapping"),
    }
}

/// Brings the branch `record`, the remote's record, describes into the local store through `ctx`,
/// under its id: reads the metadata blob `record` names, which the read keeps in the local store,
/// then writes the branch's local record ([`branch::bring_in_record`]), claiming its name where the
/// remote holds the branch live.
///
/// # Errors
///
/// `BranchAlreadyExists` where another local branch holds the name, or another writer changed the
/// branch's local metadata meanwhile; `Oversized` for a blob larger than metadata may be; the
/// remote's error for a failed read; and a failed write.
async fn bring_in(
    ctx: &Arc<RepositoryContext>,
    record: &BranchRecord,
) -> Result<(), RevisionGraphError> {
    Metadata::deserialize(ctx.clone(), record.metadata)
        .await
        .forward_any::<RevisionGraphError>("copying the branch metadata from the remote")?;
    branch::bring_in_record(
        ctx.clone(),
        record.id,
        &record.name,
        record.metadata,
        !record.deleted,
    )
    .await
    .forward_any::<RevisionGraphError>("bringing the branch in")
}

/// Whether `target` descends from `previous` along first parents, as the ancestry walk through
/// `ctx` finds it ([`history::find_branch_point`]): `previous` is the revision both lines share.
///
/// # Errors
///
/// The walk's error: `RevisionNotFound` where a line stops at a revision the local store lacks and
/// the remote can't move it, and the remote's error where its request failed.
async fn fast_forwards(
    ctx: &Arc<RepositoryContext>,
    target: Hash,
    previous: Hash,
) -> Result<bool, RevisionGraphError> {
    let (_, _, above_previous) = history::find_branch_point(ctx.clone(), target, previous)
        .await
        .forward_any::<RevisionGraphError>("walking the target's and the local latest's lines")?;
    Ok(above_previous.is_empty())
}

/// Sends the synced event of call `id` for `branch`, with the local latest the call read and the
/// local latest it left.
fn emit_synced(id: u64, branch: BranchId, previous_local_latest: Hash, local_latest: Hash) {
    LoreEvent::RevisionGraphBranchSynced(LoreRevisionGraphBranchSyncedEventData {
        id,
        branch,
        previous_local_latest,
        local_latest,
    })
    .send();
}
