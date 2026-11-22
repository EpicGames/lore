// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_graph_branch_push`: publish a branch's local history to the remote.
//!
//! The call pushes the branch alone through the push core
//! ([`push_branch`](lore_revision::branch::push::push_branch)), with its linked revisions checked
//! on the remote, and moves the local latest only from the value it pushed. One pushed event
//! reports the outcome once the call has read the remote latest.

use std::convert::Infallible;
use std::sync::Arc;

use lore_base::error::BranchAlreadyExists;
use lore_base::error::BranchNotFound;
use lore_base::error::Divergent;
use lore_base::error::NoRemote;
use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_macro::ValidateText;
use lore_revision::branch;
use lore_revision::branch::push::MountPolicy;
use lore_revision::branch::push::PushError;
use lore_revision::branch::push::PushReport;
use lore_revision::branch::push::PushRun;
use lore_revision::branch::push::push_branch;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_graph::LoreRevisionGraphBranchPushedEventData;
use lore_revision::protocol::RemoteEndpoint;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_transport::BranchRecord;
use lore_transport::Revision;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_graph::RevisionGraphError;
use crate::revision_graph::call::revision_graph_call;
use crate::revision_graph::locations::branch_record_at;
use crate::revision_graph::locations::full_context;
use crate::revision_graph::locations::revision_service;
use crate::storage::handle::LoreStore;
use crate::storage::store::PerCallFlags;
use crate::storage::store::StoreInternal;

/// Options of `lore_revision_graph_branch_push`.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, ValidateText, bitcode::Encode, bitcode::Decode,
)]
pub struct LoreRevisionGraphBranchPushOptions {
    /// Non-zero: offer the local history even where the remote latest has moved, which overwrites
    /// it. Branch protection still applies.
    pub force: u8,
    /// Non-zero: let the remote merge onto a remote latest that has moved, where the merge has no
    /// conflicts.
    pub fast_forward_merge: u8,
}

/// Arguments for `lore_revision_graph_branch_push`.
#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(branch_push_impl)]
pub struct LoreRevisionGraphBranchPushArgs {
    /// Caller-chosen id echoed back in every event this surface defines for the call. Separate
    /// from `globals.correlation_id`.
    pub id: u64,
    /// Open storage handle the call reaches the repository through
    pub store: LoreStore,
    /// Repository the branch is pushed in
    pub repository: Partition,
    /// Branch whose local history the call publishes. Not zero.
    pub branch: BranchId,
    /// How the call pushes
    pub options: LoreRevisionGraphBranchPushOptions,
}

/// Publish a branch's local history to the remote.
pub fn branch_push(
    globals: LoreGlobalArgs,
    args: LoreRevisionGraphBranchPushArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    dispatch_call(globals, args, callback, branch_push_impl)
}

fn branch_push_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionGraphBranchPushArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    let per_call = PerCallFlags::from_globals(&globals);
    revision_graph_call(
        globals,
        callback,
        args.store,
        args,
        branch_push,
        async move |store, args| {
            let remote = validate(&store, per_call, &args)?;
            let ctx = full_context(&store, args.repository, Some(&remote)).await?;
            push(&ctx, &args).await
        },
    )
}

/// The storage handle's endpoint, which a push reaches the remote through, after checking the
/// arguments and the flags, as a push does before any read.
///
/// # Errors
///
/// `InvalidArguments` for a zero branch, contradicting flags, and `local` or `offline` on the call
/// or the storage handle, since a push writes to the remote; `NoRemote` for a storage handle
/// without a remote.
fn validate(
    store: &StoreInternal,
    per_call: PerCallFlags,
    args: &LoreRevisionGraphBranchPushArgs,
) -> Result<Arc<RemoteEndpoint>, RevisionGraphError> {
    if args.branch.is_zero() {
        return Err(RevisionGraphError::invalid("the branch id is zero"));
    }
    if store.effective_flags(per_call)?.no_remote {
        return Err(RevisionGraphError::invalid(
            "a push writes to the remote, which the `local` and `offline` flags exclude",
        ));
    }
    store
        .remote
        .clone()
        .ok_or_else(|| RevisionGraphError::from(NoRemote))
}

/// Pushes the branch `args` names through `ctx`, the call's context, from the local latest it
/// reads, and sends the pushed event: on success, and on a failure once a remote latest has been
/// read ([`refused`]). A push that published nothing reads the local latest's revision number for
/// the event; where that read fails, the call fails with its error and sends the failure event.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
///
/// # Errors
///
/// `BranchNotFound` for a branch without a local latest, and the push's failure as [`classify`]
/// reports it.
#[allow(clippy::manual_async_fn)]
fn push<'a>(
    ctx: &'a Arc<RepositoryContext>,
    args: &'a LoreRevisionGraphBranchPushArgs,
) -> impl Future<Output = Result<(), RevisionGraphError>> + 'a {
    async move {
        let branch = args.branch;
        let local_latest = branch::mutable_value(ctx.clone(), branch::LATEST, branch)
            .await
            .forward_any::<RevisionGraphError>("loading the local latest")?
            .unwrap_or_default();
        if local_latest.is_zero() {
            return Err(RevisionGraphError::from(BranchNotFound {
                branch: branch.to_string(),
            }));
        }
        ctx.set_disable_upload(false);
        let token = ctx
            .try_write_token()
            .ok_or_else(|| RevisionGraphError::internal("the push's context carries no token"))?;
        let run = PushRun {
            force: args.options.force != 0,
            dry_run: false,
            fast_forward_merge: args.options.fast_forward_merge != 0,
            mounts: MountPolicy::RequireOnRemote,
            expected_local_latest: Some(local_latest),
        };
        let mut report = PushReport::default();
        let pushed = push_branch(ctx.clone(), token, &run, branch, local_latest, &mut report).await;
        if let Err(err) = pushed {
            return Err(Box::pin(refused(ctx, args, err, &report, local_latest)).await);
        }
        let local_number = if report.published.is_zero() {
            match State::deserialize(ctx.clone(), local_latest).await {
                Ok(state) => state.revision_number(),
                Err(err) => {
                    emit(pushed_on_failure(
                        args.id,
                        branch,
                        &report,
                        report.remote_latest,
                        local_latest,
                    ));
                    return Err(err).forward_any("reading the local latest's revision number");
                }
            }
        } else {
            0
        };
        emit(pushed_on_success(
            args.id,
            branch,
            &report,
            local_latest,
            local_number,
        ));
        Ok(())
    }
}

/// The failure that reports `err`, the push core's failure of the push `args` asks for, from the
/// local latest `local_latest` it pushed, and what the push read (`report`).
///
/// One branch query at the remote reads the remote latest the pushed event reports, and one by
/// name names the branch that holds the branch's name, where the remote held the branch absent or
/// archived before the push or holds it archived now. The pushed event is sent where that query or
/// the push read a remote latest.
///
/// Boxed by its caller as the cold path: it runs only where the push fails.
async fn refused(
    ctx: &Arc<RepositoryContext>,
    args: &LoreRevisionGraphBranchPushArgs,
    err: PushError,
    report: &PushReport,
    local_latest: Hash,
) -> RevisionGraphError {
    let branch = args.branch;
    let confirmed = confirm(ctx, branch).await;
    let left = branch::mutable_value(ctx.clone(), branch::LATEST, branch)
        .await
        .ok()
        .flatten()
        .unwrap_or(local_latest);
    let Ok((service, record)) = confirmed else {
        if report.remote_read {
            emit(pushed_on_failure(
                args.id,
                branch,
                report,
                report.remote_latest,
                left,
            ));
        }
        return classify(err, branch, None);
    };
    let remote_latest = record
        .as_ref()
        .map_or_else(Hash::default, |record| record.latest);
    emit(pushed_on_failure(
        args.id,
        branch,
        report,
        remote_latest,
        left,
    ));
    let archived_now = record.as_ref().is_some_and(|record| record.deleted);
    let name_holder = if report.remote_absent_before || report.remote_deleted_before || archived_now
    {
        name_holder(ctx, service.as_ref(), branch, record.as_ref()).await
    } else {
        None
    };
    classify(err, branch, name_holder)
}

/// The remote's revision service for the repository of `ctx`, reached over `ctx`'s connection, and
/// its record of `branch`, `None` where the remote doesn't hold it.
async fn confirm(
    ctx: &RepositoryContext,
    branch: BranchId,
) -> Result<(Arc<dyn Revision>, Option<BranchRecord>), RevisionGraphError> {
    let service = revision_service(ctx).await?;
    let record = branch_record_at(service.as_ref(), branch, "confirming the remote latest").await?;
    Ok((service, record))
}

/// The branch the remote maps the name of `branch` to, through `service`: the name `record`, the
/// remote's record of the branch, gives, or the name the local store's metadata of the branch
/// gives where the remote holds none. `None` where neither gives a name, the remote maps none, or
/// a read fails.
async fn name_holder(
    ctx: &Arc<RepositoryContext>,
    service: &dyn Revision,
    branch: BranchId,
    record: Option<&BranchRecord>,
) -> Option<BranchId> {
    let name = match record {
        Some(record) => record.name.clone(),
        None => local_name(ctx, branch).await?,
    };
    service
        .branch_query(None, Some(&name))
        .await
        .ok()
        .map(|holder| holder.id)
}

/// The name the local store's metadata of `branch` gives, `None` where the store holds none or a
/// read fails.
async fn local_name(ctx: &Arc<RepositoryContext>, branch: BranchId) -> Option<String> {
    let (_, metadata) = branch::local_metadata(ctx.clone(), branch).await.ok()??;
    branch::name(&metadata).ok().map(str::to_string)
}

/// Sends the pushed event `data`.
fn emit(data: LoreRevisionGraphBranchPushedEventData) {
    LoreEvent::RevisionGraphBranchPushed(data).send();
}

/// The pushed event of call `id`, a push of `branch` that succeeded, from `report`: the revision
/// published last, the remote latest and the local latest the push left. Where the push published
/// nothing, the event names `local_latest`, the local latest the push read, numbered
/// `local_number`, which the remote holds.
#[lore_macro::test_pub]
pub(crate) fn pushed_on_success(
    id: u64,
    branch: BranchId,
    report: &PushReport,
    local_latest: Hash,
    local_number: u64,
) -> LoreRevisionGraphBranchPushedEventData {
    let (revision_hash, revision_number) = if report.published.is_zero() {
        (local_latest, local_number)
    } else {
        (report.published, report.revision_number)
    };
    LoreRevisionGraphBranchPushedEventData {
        id,
        branch,
        revision_hash,
        revision_number,
        fast_forward_merged: report.fast_forward_merged.into(),
        remote_latest: report.remote_latest,
        local_latest: if report.local_latest.is_zero() {
            local_latest
        } else {
            report.local_latest
        },
    }
}

/// The pushed event of call `id`, a push of `branch` that failed, with `remote_latest` and
/// `local_latest` as the call last read them, and the revision `report` says the push published
/// last, zero where it published none.
#[lore_macro::test_pub]
pub(crate) fn pushed_on_failure(
    id: u64,
    branch: BranchId,
    report: &PushReport,
    remote_latest: Hash,
    local_latest: Hash,
) -> LoreRevisionGraphBranchPushedEventData {
    LoreRevisionGraphBranchPushedEventData {
        id,
        branch,
        revision_hash: report.published,
        revision_number: report.revision_number,
        fast_forward_merged: report.fast_forward_merged.into(),
        remote_latest,
        local_latest,
    }
}

/// The failure that reports `err`, the push core's failure of a push of `branch`. `name_holder` is
/// the branch the remote's mapping of the branch's name names, read where the remote held the
/// branch absent or archived.
///
/// A refusal of the caller, the remote's refusal of a push onto a remote latest that has moved, a
/// linked revision the remote lacks, a missing or oversized blob, and an overloaded or unreachable
/// remote or one in maintenance keep their codes. A name another branch holds is
/// `BranchAlreadyExists`, a divergence the core reports `Divergent`, and anything else `Internal`
/// ([`RevisionGraphError::remote_refusal`]).
#[lore_macro::test_pub]
pub(crate) fn classify(
    err: PushError,
    branch: BranchId,
    name_holder: Option<BranchId>,
) -> RevisionGraphError {
    const CONTEXT: &str = "pushing the branch";
    let link_divergent = err.is_link_divergent();
    let Err(err) = Err::<Infallible, _>(err).forward_any::<RevisionGraphError>(CONTEXT);
    if err.keeps_its_code_at_the_remote()
        || err.is_branch_advanced()
        || err.is_revision_not_found()
        || err.is_address_not_found()
        || err.is_oversized()
    {
        return err;
    }
    if name_holder.is_some_and(|holder| holder != branch) {
        let taken = BranchAlreadyExists {
            branch: branch.to_string(),
        };
        return RevisionGraphError::BranchAlreadyExists(taken.chain_err_from(err, CONTEXT));
    }
    if link_divergent || err.is_divergent() {
        return RevisionGraphError::Divergent(Divergent.chain_err_from(err, CONTEXT));
    }
    RevisionGraphError::remote_refusal(err, CONTEXT)
}
