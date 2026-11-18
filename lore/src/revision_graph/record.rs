// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The records a revision graph call reports, and the events that report them.
//!
//! A builder reads one branch at one location into a [`BranchRecordOut`], which holds the branch's
//! text and stack as the branch record event carries them, so that [`emit_branch`] moves them into
//! the event rather than copying them. Each emitter sends its events from the calling task. Each
//! record emitter returns the number of record events it sent, which the caller adds to the
//! location's count for [`emit_location_complete`].

use std::sync::Arc;

use futures::FutureExt;
use lore_base::lore_warn;
use lore_base::types::BranchId;
use lore_base::types::BranchPoint;
use lore_base::types::Hash;
use lore_error_set::prelude::*;
use lore_revision::branch;
use lore_revision::event::LoreErrorDetail;
use lore_revision::event::LoreEvent;
use lore_revision::event::LoreMetadataEventData;
use lore_revision::event::revision_graph::LoreRevisionGraphBranchEventData;
use lore_revision::event::revision_graph::LoreRevisionGraphBranchMetadataEventData;
use lore_revision::event::revision_graph::LoreRevisionGraphInstanceEventData;
use lore_revision::event::revision_graph::LoreRevisionGraphLocationCompleteEventData;
use lore_revision::event::revision_graph::LoreRevisionGraphRevisionEventData;
use lore_revision::instance::InstanceRecord;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::interface::LoreBranchPoint;
use lore_revision::interface::LoreString;
use lore_revision::metadata::Metadata;
use lore_revision::metadata::MetadataType;
use lore_revision::repository::RepositoryContext;
use lore_transport::BranchRecord;

use crate::revision_graph::RevisionGraphError;

/// One branch as one location holds it, with its text and stack in the form its branch record
/// event carries them.
#[derive(Debug)]
pub struct BranchRecordOut {
    /// Branch identifier.
    pub branch: BranchId,
    /// Branch name.
    pub name: LoreString,
    /// Branch category.
    pub category: LoreString,
    /// The branch's latest revision at the location.
    pub latest: Hash,
    /// The branch's branch point, then those of the branches it descends from, nearest first.
    pub stack: LoreArray<LoreBranchPoint>,
    /// Identifier of the user who created the branch.
    pub creator: LoreString,
    /// Time the branch was created, in milliseconds since the Unix epoch.
    pub created: u64,
    /// Whether the branch's metadata marks it protected.
    pub protect: bool,
    /// Whether the branch is archived at the location.
    pub archived: bool,
    /// Whether the local latest isn't known to be on the remote's history line for the branch, the
    /// remote latest or a revision it reaches along first parents, as after a commit the remote
    /// doesn't have yet. False on a remote record.
    pub divergent: bool,
}

/// The record of `branch` in the local store, read through `ctx`, as [`live_branch_record`] reads
/// it without a creator filter. `None` where the branch has no metadata key, or the local store
/// lacks the blob it names.
///
/// Returns the builder's future with the record mapped onto its output, without a future of its
/// own.
///
/// # Errors
///
/// A failed or corrupt read, and a blob without a name.
#[inline]
pub fn local_branch_record(
    ctx: Arc<RepositoryContext>,
    branch: BranchId,
) -> impl Future<Output = Result<Option<BranchRecordOut>, RevisionGraphError>> {
    live_branch_record(ctx, branch, None)
        .map(|read| read.map(|found| found.and_then(|(_, record)| record)))
}

/// The value of the metadata key of `branch` in the local store, read through `ctx` with
/// [`branch::local_metadata`], and the record [`branch_record_from_metadata`] builds from the blob
/// it names where `filter` keeps the blob's creator. `None` where the branch has no metadata key,
/// or the local store lacks the blob it names.
///
/// Whether the branch is archived is for the caller to tell, from its name's mapping.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
///
/// # Errors
///
/// A failed or corrupt read, and a blob without a name that `filter` keeps.
#[allow(clippy::manual_async_fn)]
pub(crate) fn live_branch_record(
    ctx: Arc<RepositoryContext>,
    branch: BranchId,
    filter: Option<Arc<str>>,
) -> impl Future<Output = Result<Option<(Hash, Option<BranchRecordOut>)>, RevisionGraphError>> {
    async move {
        let Some((metadata_hash, metadata)) =
            branch::local_metadata(ctx.clone(), branch)
                .await
                .forward_any::<RevisionGraphError>("reading the branch metadata")?
        else {
            return Ok(None);
        };
        if !branch::metadata_creator_matches(filter.as_deref(), &metadata) {
            return Ok(Some((metadata_hash, None)));
        }
        let record = branch_record_from_metadata(ctx, branch, metadata).await?;
        Ok(Some((metadata_hash, Some(record))))
    }
}

/// The record of `branch` in the local store from `metadata`, the blob its metadata key names: the
/// blob's fields, and the latest and the latest status ([`local_latest`]). A branch
/// without a latest has a zero one, and a blob without a category or a creator, as older clients
/// wrote, an empty one.
///
/// The record reports the branch live: its metadata key outlives an archive, so whether the
/// branch is archived is for the caller to tell.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
///
/// # Errors
///
/// A failed read, and a blob without a name.
#[allow(clippy::manual_async_fn)]
pub(crate) fn branch_record_from_metadata(
    ctx: Arc<RepositoryContext>,
    branch: BranchId,
    metadata: Metadata,
) -> impl Future<Output = Result<BranchRecordOut, RevisionGraphError>> {
    async move {
        let LocalLatest { latest, divergent } = local_latest(ctx, branch).await?;
        Ok(BranchRecordOut {
            branch,
            name: LoreString::from(
                branch::name(&metadata)
                    .forward_any::<RevisionGraphError>("reading the branch name")?,
            ),
            category: LoreString::from(branch::category(&metadata).unwrap_or_default()),
            latest,
            stack: event_stack(branch::stack(&metadata)),
            creator: LoreString::from(branch::creator(&metadata).unwrap_or_default()),
            created: branch::created(&metadata),
            protect: branch::protected(&metadata),
            archived: false,
            divergent,
        })
    }
}

/// The latest of a branch in the local store, zero for none, and whether its latest status is
/// divergent.
#[derive(Clone, Copy)]
pub(crate) struct LocalLatest {
    /// The local latest.
    pub latest: Hash,
    /// Whether the local latest isn't known to be on the remote's history line for the branch, the
    /// remote latest or a revision it reaches along first parents, as after a commit the remote
    /// doesn't have yet.
    pub divergent: bool,
}

/// The local latest of `branch` and its latest status, read through `ctx` together.
///
/// Returns the two reads' joined future with the answer mapped onto its output, without a future
/// of its own.
///
/// # Errors
///
/// A failed read, the latest's first.
#[inline]
pub(crate) fn local_latest(
    ctx: Arc<RepositoryContext>,
    branch: BranchId,
) -> impl Future<Output = Result<LocalLatest, RevisionGraphError>> {
    futures::future::join(
        branch::mutable_value(ctx.clone(), branch::LATEST, branch),
        branch::mutable_value(ctx, branch::LATEST_STATUS, branch),
    )
    .map(|(latest, status)| {
        Ok(LocalLatest {
            latest: latest
                .forward_any::<RevisionGraphError>("loading the latest revision")?
                .unwrap_or_default(),
            divergent: status
                .forward_any::<RevisionGraphError>("loading the latest status")?
                .is_some(),
        })
    })
}

/// The record of the branch the remote's `record` describes: [`remote_record_out`], with `protect`
/// from the metadata blob `record` names, read through `ctx`.
///
/// Returns the blob read's future with `protect` mapped onto the record, which is built before
/// the read, so that the future holds the record in its event's form rather than `record`.
///
/// # Errors
///
/// The blob read's error.
#[inline]
pub fn remote_branch_record(
    ctx: Arc<RepositoryContext>,
    record: BranchRecord,
) -> impl Future<Output = Result<BranchRecordOut, RevisionGraphError>> {
    let metadata = record.metadata;
    let mut out = remote_record_out(record);
    Metadata::deserialize(ctx, metadata).map(move |read| {
        let metadata = read.forward_any::<RevisionGraphError>("reading the branch metadata")?;
        out.protect = branch::protected(&metadata);
        Ok(out)
    })
}

/// The record of the branch the remote's `record` describes, as the remote sent its fields, with
/// `archived` from `deleted`, and `protect` and `divergent` false.
pub(crate) fn remote_record_out(record: BranchRecord) -> BranchRecordOut {
    BranchRecordOut {
        branch: record.id,
        name: LoreString::from(record.name.as_str()),
        category: LoreString::from(record.category.as_str()),
        latest: record.latest,
        stack: event_stack(record.stack),
        creator: LoreString::from(record.creator.as_str()),
        created: record.created,
        protect: false,
        archived: record.deleted,
        divergent: false,
    }
}

/// `stack` as the branch record event carries it. Collected from the vector itself, the conversion
/// keeps the vector's allocation: the standard library collects in place into an element type of
/// the same size and alignment. The assertion checks both, since `BranchPoint` has no `repr(C)` to
/// fix them.
pub(crate) fn event_stack(stack: Vec<BranchPoint>) -> LoreArray<LoreBranchPoint> {
    const {
        assert!(
            size_of::<BranchPoint>() == size_of::<LoreBranchPoint>()
                && align_of::<BranchPoint>() == align_of::<LoreBranchPoint>()
        );
    }
    LoreArray::from_vec(
        stack
            .into_iter()
            .map(|point| LoreBranchPoint::from(&point))
            .collect(),
    )
}

/// Sends `record` as the branch record event of call `id` at `location`, moving its text and stack
/// into the event. Returns 1, the number of record events sent.
pub fn emit_branch(id: u64, location: LoreBranchLocation, record: BranchRecordOut) -> u64 {
    LoreEvent::RevisionGraphBranch(LoreRevisionGraphBranchEventData {
        id,
        location,
        branch: record.branch,
        name: record.name,
        category: record.category,
        latest: record.latest,
        stack: record.stack,
        creator: record.creator,
        created: record.created,
        protect: u8::from(record.protect),
        archived: u8::from(record.archived),
        divergent: u8::from(record.divergent),
    })
    .send();
    1
}

/// Sends `record`, the record of a branch the local store holds, as the branch record event of
/// call `id`, then the instance records of `on_branch`, the registered instances on that branch.
/// Returns the number of record events sent.
pub fn emit_local_branch(id: u64, record: BranchRecordOut, on_branch: Vec<InstanceRecord>) -> u64 {
    let branch = record.branch;
    emit_branch(id, LoreBranchLocation::Local, record) + emit_instances(id, branch, on_branch)
}

/// Sends an instance record event of call `id` for each of `records`, the registered instances on
/// `branch`, in the local store, moving each record's path into its event. Returns the number sent.
pub fn emit_instances(id: u64, branch: BranchId, records: Vec<InstanceRecord>) -> u64 {
    let sent = records.len() as u64;
    for record in records {
        LoreEvent::RevisionGraphInstance(LoreRevisionGraphInstanceEventData {
            id,
            location: LoreBranchLocation::Local,
            instance_id: record.instance_id,
            path: record.path,
            branch,
            revision: record.revision,
            stale: record.staleness.as_event_flag(),
        })
        .send();
    }
    sent
}

/// Sends the revision record event of call `id` at `location`: the revision with
/// `revision_hash` and `revision_number`, created on `branch`, zero where the location doesn't
/// record it. Returns 1, the number of record events sent.
pub fn emit_revision(
    id: u64,
    location: LoreBranchLocation,
    revision_hash: Hash,
    revision_number: u64,
    branch: BranchId,
) -> u64 {
    LoreEvent::RevisionGraphRevision(LoreRevisionGraphRevisionEventData {
        id,
        location,
        revision_hash,
        revision_number,
        branch,
    })
    .send();
    1
}

/// Sends a branch metadata record event of call `id` at `location` for each entry of `metadata`,
/// the metadata of `branch`, in stored order, or only for the entries whose key is `key` where it's
/// `Some`. Returns the number of record events sent.
///
/// Passes over an entry no record can carry: one whose key isn't UTF-8, whose type tag
/// [`MetadataType`] doesn't define, or whose value doesn't decode as its type. Where it passes over
/// any, it logs one warning that names `location` and how many.
pub fn emit_branch_metadata(
    id: u64,
    location: LoreBranchLocation,
    branch: BranchId,
    metadata: &Metadata,
    key: Option<&str>,
) -> u64 {
    let (mut records, mut skipped) = (0, 0);
    metadata.walk_tags(|stored_key, value, tag| {
        if key.is_some_and(|key| key.as_bytes() != stored_key) {
            return;
        }
        let Some(entry) = MetadataType::try_from(tag).ok().and_then(|value_type| {
            LoreMetadataEventData::from_stored(stored_key, value, value_type)
        }) else {
            skipped += 1;
            return;
        };
        LoreEvent::RevisionGraphBranchMetadata(LoreRevisionGraphBranchMetadataEventData {
            id,
            location,
            branch,
            key: entry.key,
            value: entry.value,
        })
        .send();
        records += 1;
    });
    if skipped > 0 {
        lore_warn!(
            "Left out {skipped} {location} metadata entries of branch {branch} whose key isn't \
             UTF-8, whose type this build doesn't define, or whose value doesn't decode as its \
             type"
        );
    }
    records
}

/// Sends the location complete event of call `id` at `location`, which follows the `records`
/// record events the call sent for it and carries the detail of `outcome`: code 0 where the work
/// at the location succeeded, a miss included, and the error's code, message and trace where it
/// failed.
pub fn emit_location_complete<T>(
    id: u64,
    location: LoreBranchLocation,
    records: u64,
    outcome: &Result<T, RevisionGraphError>,
) {
    LoreEvent::RevisionGraphLocationComplete(LoreRevisionGraphLocationCompleteEventData {
        id,
        location,
        records,
        error: LoreErrorDetail::from_result(outcome),
    })
    .send();
}
