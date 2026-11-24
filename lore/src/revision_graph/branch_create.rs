// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_graph_branch_create`: create a branch at each location a call consults, the
//! remote first.
//!
//! The call checks its arguments and the creator before any read. Before it creates anything, it
//! reads at each location where the new branch starts ([`parent_at`]) and whether its name is
//! unclaimed there ([`name_free_at`]). Each answers a [`PreCheck`]: a failed read fails the
//! location, and a refusal stops the call before it starts work at any location. [`settle`] takes
//! the branch point and the stack from what the locations answered. The call then creates the
//! branch with a new id at the remote, through Lore Server's create, and in the local store, through
//! [`branch::create_record`], and each location reports the record it created and ends with its
//! location complete event. A failure at the remote stops the call before the local store.

use std::sync::Arc;

use lore_base::error::BranchAlreadyExists;
use lore_base::error::BranchNotFound;
use lore_base::error::Divergent;
use lore_base::types::BranchId;
use lore_base::types::BranchPoint;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::branch;
use lore_revision::branch::BranchLatestStatus;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::interface::LoreString;
use lore_revision::lore::execution_context;
use lore_revision::metadata::Metadata;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::util::time::timestamp;
use lore_transport::BranchRecord;
use lore_transport::ProtocolError;
use lore_transport::Revision;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_graph::LoreRevisionIdentifier;
use crate::revision_graph::RevisionForm;
use crate::revision_graph::RevisionGraphError;
use crate::revision_graph::call::revision_graph_call;
use crate::revision_graph::locations::LocationMiss;
use crate::revision_graph::locations::PreCheck;
use crate::revision_graph::locations::branch_record_at;
use crate::revision_graph::locations::default_branch_at;
use crate::revision_graph::locations::ended_at;
use crate::revision_graph::locations::local_write_context;
use crate::revision_graph::locations::locations;
use crate::revision_graph::locations::remote_revision_service;
use crate::revision_graph::record::BranchRecordOut;
use crate::revision_graph::record::emit_branch;
use crate::revision_graph::record::emit_location_complete;
use crate::revision_graph::record::event_stack;
use crate::revision_graph::record::remote_record_out;
use crate::revision_graph::revision_resolve::numbered_at;
use crate::revision_graph::revision_resolve::revision_record_at;
use crate::storage::handle::LoreStore;
use crate::storage::store::PerCallFlags;
use crate::storage::store::StoreInternal;

/// Arguments for `lore_revision_graph_branch_create`.
#[repr(C)]
#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    LoreArgs,
    bitcode::Encode,
    bitcode::Decode,
    lore_macro::WireSchema,
)]
#[handler(branch_create_impl)]
pub struct LoreRevisionGraphBranchCreateArgs {
    /// Caller-chosen id echoed back in every event this surface defines for the call. Separate
    /// from `globals.correlation_id`.
    pub id: u64,
    /// Open storage handle the call reaches the repository through
    pub store: LoreStore,
    /// Repository the branch is created in
    pub repository: Partition,
    /// Name of the new branch: not empty, at most 1000 bytes, and not a branch id. No live branch
    /// may hold it, compared regardless of case.
    pub name: LoreString,
    /// Category of the new branch, at most 1000 bytes. Empty means the default category.
    pub category: LoreString,
    /// The revision the new branch starts at. A hash makes the revision's own branch the parent,
    /// and a branch makes that branch the parent, at its latest or at the revision number given.
    pub from: LoreRevisionIdentifier,
}

/// Where a new branch starts at one location, as [`parent_at`] reads it.
#[lore_macro::test_pub]
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ParentAt {
    /// The branch the new branch is created from, zero where the location holds the revision the
    /// identifier names without recording its branch.
    pub parent: BranchId,
    /// The revision the new branch starts at.
    pub branch_point: Hash,
    /// The parent's stack, which follows the branch point in the new branch's stack. `None` where
    /// the location holds no metadata of the parent.
    pub stack: Option<Vec<BranchPoint>>,
    /// Whether the branch point is zero and the location holds no repository metadata, so the
    /// location can't tell whether the parent is the default branch.
    pub default_unchecked: bool,
}

/// The parent of a new branch at one location, before the checks [`parent_at`] runs on it, with
/// whether its metadata puts it in the personal category.
struct Parent {
    at: ParentAt,
    personal: bool,
}

/// Where a branch created from the revision `form` names starts at one location: at the remote,
/// given its revision service `service`, where `ctx` reads the remote alone, and otherwise in the
/// local store, which `ctx` reads strictly.
///
/// A hash makes the parent the revision's own branch, the one its metadata names, and the branch
/// point the revision. A branch makes the parent that branch, and the branch point its latest
/// revision or the revision with the number the identifier gives, as [`numbered_at`] finds it. The
/// parent's stack and category come from the remote's record of the parent, or from its metadata
/// in the local store, read with its latest. A parent the local store holds with metadata and
/// without a latest has a zero branch point.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
///
/// # Errors
///
/// A failed or corrupt read and a failed request fail the location. The refusals are
/// `BranchNotFound` for a parent branch the location doesn't hold, `RevisionNotFound` for a hash
/// or a number the location doesn't hold, a number on a parent without a latest included, and
/// `InvalidArguments` for a parent in the personal category, which can't have children, and for a
/// zero branch point on a parent the location's repository metadata doesn't name as the default
/// branch.
#[lore_macro::test_pub]
#[allow(clippy::manual_async_fn)]
pub(crate) fn parent_at<'a>(
    ctx: &'a Arc<RepositoryContext>,
    service: Option<&'a Arc<dyn Revision>>,
    form: RevisionForm,
) -> impl Future<Output = PreCheck<ParentAt>> + 'a {
    async move {
        let read = match service {
            Some(service) => remote_parent(ctx, service, form).await?,
            None => local_parent(ctx, form).await?,
        };
        let Parent { mut at, personal } = match read {
            Ok(parent) => parent,
            Err(miss) => return Ok(Err(miss.error(form))),
        };
        if personal {
            return Ok(Err(RevisionGraphError::invalid(format!(
                "the parent branch {} is personal, which can't have children",
                at.parent
            ))));
        }
        if at.branch_point.is_zero() {
            match default_branch_at(ctx, service.is_some()).await? {
                Some(default) if default == at.parent => {}
                Some(_) => {
                    return Ok(Err(RevisionGraphError::invalid(format!(
                        "the parent branch {} has no latest revision and isn't the default branch",
                        at.parent
                    ))));
                }
                None => at.default_unchecked = true,
            }
        }
        Ok(Ok(at))
    }
}

/// The trace context of a failed query of the parent branch at the remote.
const PARENT_QUERY: &str = "querying the remote for the parent branch";

/// The parent of a branch created from the revision `form` names at the remote, through `ctx`,
/// the remote's read context, and `service`, its revision service. The parent's record is read
/// with one branch query, beside the remote's revision list for a revision number.
async fn remote_parent(
    ctx: &Arc<RepositoryContext>,
    service: &Arc<dyn Revision>,
    form: RevisionForm,
) -> Result<Result<Parent, LocationMiss>, RevisionGraphError> {
    let (parent, branch_point, record) = match form {
        RevisionForm::Hash(hash) => {
            let Some((_, parent)) = revision_record_at(ctx, hash).await? else {
                return Ok(Err(LocationMiss::RevisionNotFound));
            };
            let record = if parent.is_zero() {
                None
            } else {
                branch_record_at(service.as_ref(), parent, PARENT_QUERY).await?
            };
            (parent, hash, record)
        }
        RevisionForm::Latest(branch) => {
            let Some(record) = branch_record_at(service.as_ref(), branch, PARENT_QUERY).await?
            else {
                return Ok(Err(LocationMiss::BranchNotFound));
            };
            (branch, record.latest, Some(record))
        }
        RevisionForm::Number(branch, number) => {
            let (numbered, record) = tokio::join!(
                numbered_at(ctx, Some(service), branch, number),
                branch_record_at(service.as_ref(), branch, PARENT_QUERY),
            );
            let revision = match numbered? {
                Ok((revision, _)) => revision,
                Err(miss) => return Ok(Err(miss)),
            };
            (branch, revision, record?)
        }
    };
    Ok(Ok(Parent {
        personal: record
            .as_ref()
            .is_some_and(|record| record.category == branch::personal_category()),
        at: ParentAt {
            parent,
            branch_point,
            stack: record.map(|record| record.stack),
            default_unchecked: false,
        },
    }))
}

/// The parent of a branch created from the revision `form` names in the local store, through
/// `ctx`. A branch's metadata and latest are read together, and its metadata alone after a walk
/// to a revision number.
async fn local_parent(
    ctx: &Arc<RepositoryContext>,
    form: RevisionForm,
) -> Result<Result<Parent, LocationMiss>, RevisionGraphError> {
    let (parent, branch_point, metadata) = match form {
        RevisionForm::Hash(hash) => {
            let Some((_, parent)) = revision_record_at(ctx, hash).await? else {
                return Ok(Err(LocationMiss::RevisionNotFound));
            };
            let metadata = if parent.is_zero() {
                None
            } else {
                parent_metadata(ctx, parent).await?
            };
            (parent, hash, metadata)
        }
        RevisionForm::Latest(branch) => {
            let (metadata, latest) = tokio::join!(
                parent_metadata(ctx, branch),
                branch::mutable_value(ctx.clone(), branch::LATEST, branch),
            );
            let latest =
                latest.forward_any::<RevisionGraphError>("loading the parent's latest revision")?;
            let metadata = metadata?;
            if metadata.is_none() && latest.is_none() {
                return Ok(Err(LocationMiss::BranchNotFound));
            }
            (branch, latest.unwrap_or_default(), metadata)
        }
        RevisionForm::Number(branch, number) => {
            let revision = match numbered_at(ctx, None, branch, number).await? {
                Ok((revision, _)) => revision,
                Err(miss) => return Ok(Err(miss)),
            };
            (branch, revision, parent_metadata(ctx, branch).await?)
        }
    };
    Ok(Ok(Parent {
        personal: metadata.as_ref().is_some_and(|metadata| {
            branch::category(metadata).is_ok_and(|category| category == branch::personal_category())
        }),
        at: ParentAt {
            parent,
            branch_point,
            stack: metadata.map(|metadata| branch::stack(&metadata)),
            default_unchecked: false,
        },
    }))
}

/// The metadata of `parent` in the local store, read through `ctx` with [`branch::local_metadata`].
async fn parent_metadata(
    ctx: &Arc<RepositoryContext>,
    parent: BranchId,
) -> Result<Option<Metadata>, RevisionGraphError> {
    Ok(branch::local_metadata(ctx.clone(), parent)
        .await
        .forward_any::<RevisionGraphError>("reading the parent's metadata")?
        .map(|(_, metadata)| metadata))
}

/// The value the mapping of `name` holds at one location, which a create claims the name from:
/// at the remote, given its revision service `service`, zero where no live branch holds the name,
/// and otherwise in the local store, read through `ctx`, the value of the name's mapping, zero
/// where the name has none or only the id derived from it names a branch
/// ([`branch::local_claim_expected`]).
///
/// In the local store, the name is claimed where the branch it names has no metadata key, as a
/// stale mapping or a derived id with only a latest revision leaves it. A branch with a metadata
/// key holds the name, whatever its metadata says, as the file-system create reads it: metadata
/// that says the branch is archived while the name maps to it reads as live.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
///
/// # Errors
///
/// A failed read or request fails the location. The refusal is `BranchAlreadyExists` for a name a
/// branch holds.
#[lore_macro::test_pub]
#[allow(clippy::manual_async_fn)]
pub(crate) fn name_free_at<'a>(
    ctx: &'a Arc<RepositoryContext>,
    service: Option<&'a Arc<dyn Revision>>,
    name: &'a str,
) -> impl Future<Output = PreCheck<Context>> + 'a {
    async move {
        let taken = || {
            RevisionGraphError::from(BranchAlreadyExists {
                branch: name.to_owned(),
            })
        };
        if let Some(service) = service {
            return match service.branch_query(None, Some(name)).await {
                Ok(record) if !record.deleted => Ok(Err(taken())),
                Ok(_) => Ok(Ok(Context::default())),
                Err(err) if err.is_not_found() => Ok(Ok(Context::default())),
                Err(err) => {
                    Err(err).forward_any::<RevisionGraphError>("querying the remote for the name")
                }
            };
        }
        match branch::local_claim_expected(ctx.clone(), name, BranchId::default()).await {
            Ok(expected) => Ok(Ok(expected)),
            Err(err) if err.is_branch_already_exists() => Ok(Err(taken())),
            Err(err) => Err(err).forward_any::<RevisionGraphError>("looking up the branch name"),
        }
    }
}

/// What the pre-checks read at one location, where neither failed.
#[lore_macro::test_pub]
#[derive(Debug)]
pub(crate) struct Checked {
    /// Where the new branch starts there, or the refusal [`parent_at`] answered.
    pub parent: Result<ParentAt, RevisionGraphError>,
    /// The value a claim of the name swaps from there, or the refusal [`name_free_at`] answered.
    pub name: Result<Context, RevisionGraphError>,
}

/// Where the new branch starts at every location it is created on, as [`settle`] takes it.
#[lore_macro::test_pub]
#[derive(Debug, PartialEq)]
pub(crate) struct Settled {
    /// The revision the new branch starts at.
    pub branch_point: Hash,
    /// The new branch's stack: its branch point on the parent, then the parent's stack.
    pub stack: Vec<BranchPoint>,
    /// The value the local store's claim of the name swaps from, zero where the call doesn't
    /// create in the local store.
    pub name_expected: Context,
}

/// Where a branch created from the revision `form` names starts, from the pre-checks of the
/// locations it is created on, `remote` and `local`, at least one of them read.
///
/// A refusal of the parent other than a missing branch or revision fails the call first, then a
/// name a branch holds at either location. A parent or revision that one location lacks fails with
/// `RevisionNotFound` where another holds it, since the branch point must resolve at every
/// location, and otherwise with the miss itself, `RevisionNotFound` before `BranchNotFound`. The
/// parent is the branch any location records for the revision. The branch points must be equal.
/// The parent's stack is the local store's, or the remote's where the local store holds no metadata
/// of the parent.
///
/// # Errors
///
/// The refusals above, `BranchNotFound` where no location records the revision's branch or holds
/// the parent's metadata, `Divergent` for branch points that differ, and `InvalidArguments` for a
/// zero branch point no location could check against the default branch.
#[lore_macro::test_pub]
pub(crate) fn settle(
    form: RevisionForm,
    remote: Option<Checked>,
    local: Option<Checked>,
) -> Result<Settled, RevisionGraphError> {
    let (remote_parent, remote_name) = remote.map(|checked| (checked.parent, checked.name)).unzip();
    let (local_parent, local_name) = local.map(|checked| (checked.parent, checked.name)).unzip();
    let mut found = [None, None];
    let mut miss: Option<RevisionGraphError> = None;
    for (slot, answer) in found.iter_mut().zip([remote_parent, local_parent]) {
        match answer {
            None => {}
            Some(Ok(at)) => *slot = Some(at),
            Some(Err(err)) if err.is_branch_not_found() || err.is_revision_not_found() => {
                if miss.as_ref().is_none_or(|kept| kept.is_branch_not_found()) {
                    miss = Some(err);
                }
            }
            Some(Err(err)) => return Err(err),
        }
    }
    if let Some(Err(taken)) = remote_name {
        return Err(taken);
    }
    let name_expected = local_name.transpose()?.unwrap_or_default();
    if let Some(miss) = miss {
        return Err(if found.iter().any(Option::is_some) {
            LocationMiss::RevisionNotFound.error(form)
        } else {
            miss
        });
    }
    let [remote_at, local_at] = found;
    let Some(first) = remote_at.as_ref().or(local_at.as_ref()) else {
        return Err(LocationMiss::BranchNotFound.error(form));
    };
    if let (Some(remote), Some(local)) = (&remote_at, &local_at)
        && remote.branch_point != local.branch_point
    {
        return Err(RevisionGraphError::from(Divergent));
    }
    let (branch_point, default_unchecked) = (first.branch_point, first.default_unchecked);
    let Some(parent) = remote_at
        .iter()
        .chain(&local_at)
        .map(|at| at.parent)
        .find(|parent| !parent.is_zero())
    else {
        return Err(RevisionGraphError::from(BranchNotFound {
            branch: branch_point.to_string(),
        }));
    };
    if default_unchecked {
        return Err(RevisionGraphError::invalid(format!(
            "the local store holds no repository metadata to tell whether the parent branch \
             {parent}, which has no latest revision, is the default branch"
        )));
    }
    let Some(mut stack) = local_at
        .and_then(|at| at.stack)
        .or_else(|| remote_at.and_then(|at| at.stack))
    else {
        return Err(RevisionGraphError::from(BranchNotFound {
            branch: parent.to_string(),
        }));
    };
    stack.insert(
        0,
        BranchPoint {
            branch: parent,
            revision: branch_point,
        },
    );
    Ok(Settled {
        branch_point,
        stack,
        name_expected,
    })
}

/// Create a branch at each location the call consults.
pub fn branch_create(
    globals: LoreGlobalArgs,
    args: LoreRevisionGraphBranchCreateArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    dispatch_call(globals, args, callback, branch_create_impl)
}

fn branch_create_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionGraphBranchCreateArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    let per_call = PerCallFlags::from_globals(&globals);
    revision_graph_call(
        globals,
        callback,
        args.store,
        args,
        branch_create,
        async move |store, args| {
            let form = validate(&args)?;
            let mut creator = call_creator(&store).await;
            check_creator(&creator)?;
            let locations = locations(&store, per_call)?;
            let name = args.name.as_str();
            let (remote, remote_checked) = match locations.remote() {
                Some(endpoint) => {
                    let reached = remote_revision_service(&store, args.repository, endpoint).await;
                    let (ctx, service) = ended_at(args.id, LoreBranchLocation::Remote, reached)?;
                    if creator.is_empty() {
                        creator = connection_identity(&ctx).await;
                        check_creator(&creator)?;
                    }
                    let checked = check_at(&ctx, Some(&service), form, name).await;
                    let checked = ended_at(args.id, LoreBranchLocation::Remote, checked)?;
                    (Some((ctx, service)), Some(checked))
                }
                None => (None, None),
            };
            let local = if locations.local() {
                let ctx = local_write_context(&store, args.repository);
                let checked = check_at(&ctx, None, form, name).await;
                let checked = ended_at(args.id, LoreBranchLocation::Local, checked)?;
                Some((ctx, checked))
            } else {
                None
            };
            let (local, local_checked) = local.unzip();
            let settled = settle(form, remote_checked, local_checked)?;
            let category = match args.category.as_str() {
                "" => branch::default_category(),
                category => category,
            };
            let mut new = NewBranch {
                id: BranchId::from(uuid::Uuid::now_v7()),
                name,
                category,
                creator: String::new(),
                created: timestamp(),
                branch_point: settled.branch_point,
                stack: settled.stack,
            };
            let status = if let Some((_, service)) = &remote {
                new.creator = creator;
                let created = create_remote(service.as_ref(), &new).await;
                let record = ended_at(args.id, LoreBranchLocation::Remote, created)?;
                new.creator.clone_from(&record.creator);
                let records = emit_branch(
                    args.id,
                    LoreBranchLocation::Remote,
                    remote_record_out(record),
                );
                emit_location_complete(
                    args.id,
                    LoreBranchLocation::Remote,
                    records,
                    &Ok::<(), RevisionGraphError>(()),
                );
                BranchLatestStatus::Convergent
            } else {
                new.creator = creator;
                BranchLatestStatus::Divergent
            };
            let Some(ctx) = local else {
                return Ok(());
            };
            let mut records = 0;
            let outcome = create_local(ctx, new, status, settled.name_expected)
                .await
                .map(|record| records = emit_branch(args.id, LoreBranchLocation::Local, record));
            emit_location_complete(args.id, LoreBranchLocation::Local, records, &outcome);
            outcome
        },
    )
}

/// The record a create writes at each location.
#[derive(Debug)]
struct NewBranch<'a> {
    id: BranchId,
    name: &'a str,
    category: &'a str,
    creator: String,
    created: u64,
    branch_point: Hash,
    stack: Vec<BranchPoint>,
}

/// The revision `args` starts the new branch from, after checking the arguments, as a create does
/// before any read.
///
/// # Errors
///
/// `InvalidArguments` for a name that is empty, longer than [`branch::MAX_NAME_LEN`] bytes or
/// reads as a branch id, a category longer than [`branch::MAX_NAME_LEN`] bytes, the limit Lore
/// Server's create applies to a category, and a malformed revision identifier.
fn validate(args: &LoreRevisionGraphBranchCreateArgs) -> Result<RevisionForm, RevisionGraphError> {
    if !branch::is_valid_name(args.name.as_str()) {
        return Err(RevisionGraphError::invalid(format!(
            "the branch name is empty, longer than {} bytes, or reads as a branch id",
            branch::MAX_NAME_LEN
        )));
    }
    if args.category.as_str().len() > branch::MAX_NAME_LEN {
        return Err(RevisionGraphError::invalid(format!(
            "the category is longer than {} bytes",
            branch::MAX_NAME_LEN
        )));
    }
    args.from.form()
}

/// Refuses a `creator` longer than [`repository::MAX_NAME_LEN`] bytes, the limit Lore Server's
/// create applies to a creator.
fn check_creator(creator: &str) -> Result<(), RevisionGraphError> {
    if creator.len() > repository::MAX_NAME_LEN {
        return Err(RevisionGraphError::invalid(format!(
            "the creator is longer than {} bytes",
            repository::MAX_NAME_LEN
        )));
    }
    Ok(())
}

/// The creator of a branch the call creates, where the call names one: its user id, else
/// `globals.identity`, else the identity the storage handle `store` was opened with. Empty where
/// none is set, so that work at the remote takes the identity its connection authenticated as
/// ([`connection_identity`]).
async fn call_creator(store: &StoreInternal) -> String {
    let context = execution_context();
    let user_id = context.user_id().await;
    if !user_id.is_empty() {
        return user_id;
    }
    match context.globals().identity() {
        Some(identity) if !identity.is_empty() => identity.to_owned(),
        _ => store.identity.clone(),
    }
}

/// The identity the remote connection of `ctx`, the remote's read context, authenticated as.
/// Empty where the context holds no connection.
async fn connection_identity(ctx: &RepositoryContext) -> String {
    ctx.remote()
        .await
        .map_or_default(|connection| connection.identity().to_owned())
}

/// The pre-checks of a create from the revision `form` names of a branch named `name` at one
/// location, through `ctx`, and at the remote given its revision service `service`, read together.
///
/// # Errors
///
/// The failure of either pre-check, the parent's first.
async fn check_at(
    ctx: &Arc<RepositoryContext>,
    service: Option<&Arc<dyn Revision>>,
    form: RevisionForm,
    name: &str,
) -> Result<Checked, RevisionGraphError> {
    let (parent, name) = tokio::join!(
        parent_at(ctx, service, form),
        name_free_at(ctx, service, name)
    );
    Ok(Checked {
        parent: parent?,
        name: name?,
    })
}

/// Creates `new` at the remote with `service`, its revision service, and answers the record the
/// remote holds. A refusal is read with a query of the name, as [`classify_create_refusal`] reads
/// it.
async fn create_remote(
    service: &dyn Revision,
    new: &NewBranch<'_>,
) -> Result<BranchRecord, RevisionGraphError> {
    let refusal = match service
        .branch_create(new.id, new.name, new.category, &new.creator, &new.stack)
        .await
    {
        Ok(record) => return Ok(record),
        Err(refusal) => refusal,
    };
    let confirming = service.branch_query(None, Some(new.name)).await.ok();
    classify_create_refusal(refusal, confirming, new.id)
}

/// What the remote's refusal `err` of the create of branch `id` means, given `confirming`, the
/// record a query of the branch's name answered after it, where the query answered one. A live
/// record of `id` means the remote created the branch and only its answer was lost, so the create
/// goes on with that record. A live record of another branch means the name is taken. A refusal of
/// the caller, an overloaded remote and a lost connection keep their codes, and any other refusal
/// can't be attributed.
///
/// # Errors
///
/// `BranchAlreadyExists`, `NotAuthorized`, `NotAuthenticated`, `SlowDown`, `Disconnected`,
/// `Maintenance`, and otherwise `Internal` with the remote's message.
#[lore_macro::test_pub]
pub(crate) fn classify_create_refusal(
    err: ProtocolError,
    confirming: Option<BranchRecord>,
    id: BranchId,
) -> Result<BranchRecord, RevisionGraphError> {
    match confirming {
        Some(record) if !record.deleted && record.id == id => return Ok(record),
        Some(record) if !record.deleted => {
            return Err(RevisionGraphError::BranchAlreadyExists(
                BranchAlreadyExists {
                    branch: record.name,
                }
                .chain_err_from(
                    err,
                    "the remote refused the create of a name another branch holds",
                ),
            ));
        }
        _ => {}
    }
    Err(RevisionGraphError::remote_refusal(
        err,
        "the remote refused the branch create",
    ))
}

/// Creates `new` in the local store through `ctx`, with its latest at `status` and its name claimed
/// from `name_expected`, and answers the record of the branch from the values it wrote.
///
/// # Errors
///
/// [`branch::create_record`]'s errors, `BranchAlreadyExists` for a claim another writer won among
/// them.
async fn create_local(
    ctx: Arc<RepositoryContext>,
    new: NewBranch<'_>,
    status: BranchLatestStatus,
    name_expected: Context,
) -> Result<BranchRecordOut, RevisionGraphError> {
    let divergent = matches!(status, BranchLatestStatus::Divergent);
    branch::create_record(
        ctx,
        new.id,
        new.name,
        new.category,
        &new.creator,
        new.created,
        new.stack.clone(),
        new.branch_point,
        status,
        name_expected,
    )
    .await
    .forward_any::<RevisionGraphError>("creating the branch in the local store")?;
    Ok(BranchRecordOut {
        branch: new.id,
        name: LoreString::from(new.name),
        category: LoreString::from(new.category),
        latest: new.branch_point,
        stack: event_stack(new.stack),
        creator: LoreString::from(new.creator),
        created: new.created,
        protect: false,
        archived: false,
        divergent,
    })
}
