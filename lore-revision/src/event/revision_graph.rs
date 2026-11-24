// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Event data for the `lore_revision_graph_*` verbs: record events, each reporting one object as
//! one location holds it, the location complete event, which reports a call's outcome at one
//! location, and the synced event, which reports a branch sync's outcome.

use lore_base::types::BranchId;
use lore_base::types::Hash;
use serde::Serialize;

use crate::event::LoreErrorDetail;
use crate::instance::InstanceId;
use crate::interface::LoreArray;
use crate::interface::LoreBranchLocation;
use crate::interface::LoreBranchPoint;
use crate::interface::LoreMetadata;
use crate::interface::LoreString;
use crate::util::serde::u8_as_bool;

/// One branch, as one location holds it.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionGraphBranchEventData {
    /// The `id` from the call's arguments.
    pub id: u64,
    /// The location that holds the branch.
    pub location: LoreBranchLocation,
    /// Branch identifier.
    pub branch: BranchId,
    /// Branch name.
    pub name: LoreString,
    /// Branch category.
    pub category: LoreString,
    /// The branch's latest revision at this location.
    pub latest: Hash,
    /// The branch's branch point, then those of the branches it descends from, nearest first.
    pub stack: LoreArray<LoreBranchPoint>,
    /// Identifier of the user who created the branch.
    pub creator: LoreString,
    /// Time the branch was created, in milliseconds since the Unix epoch.
    pub created: u64,
    /// Set when the branch is protected.
    #[serde(with = "u8_as_bool")]
    pub protect: u8,
    /// Set when the branch is archived.
    #[serde(with = "u8_as_bool")]
    pub archived: u8,
    /// Set when the local latest isn't known to be on the remote's history line for the branch, the
    /// remote latest or a revision it reaches along first parents, as after a commit the remote
    /// doesn't have yet. Zero on a remote record.
    #[serde(with = "u8_as_bool")]
    pub divergent: u8,
}

/// One repository instance on a branch, as the local store records it.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionGraphInstanceEventData {
    /// The `id` from the call's arguments.
    pub id: u64,
    /// Always `LORE_BRANCH_LOCATION_LOCAL`, since the local store records instances.
    pub location: LoreBranchLocation,
    /// The instance's id from its registration.
    pub instance_id: InstanceId,
    /// The instance's working directory, as registered.
    pub path: LoreString,
    /// The instance's current branch.
    pub branch: BranchId,
    /// The instance's current revision.
    pub revision: Hash,
    /// 0 when the instance is live, 1 when the registered path no longer exists, 2 when the path
    /// holds a different instance, which superseded this one, and 3 when the path holds no
    /// readable checkout.
    pub stale: u8,
}

/// One revision, as one location resolves it.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionGraphRevisionEventData {
    /// The `id` from the call's arguments.
    pub id: u64,
    /// The location that resolved the revision.
    pub location: LoreBranchLocation,
    /// The revision's hash.
    pub revision_hash: Hash,
    /// The revision's number.
    pub revision_number: u64,
    /// The branch the revision was created on. Zero when the location doesn't record it.
    pub branch: BranchId,
}

/// One metadata entry of a branch, as one location holds it.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionGraphBranchMetadataEventData {
    /// The `id` from the call's arguments.
    pub id: u64,
    /// The location that holds the entry.
    pub location: LoreBranchLocation,
    /// Branch identifier.
    pub branch: BranchId,
    /// The entry's key.
    pub key: LoreString,
    /// The entry's value, with the type it's stored with.
    pub value: LoreMetadata,
}

/// The call's outcome at one location, after that location's record events.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionGraphLocationCompleteEventData {
    /// The `id` from the call's arguments.
    pub id: u64,
    /// The location this outcome is for.
    pub location: LoreBranchLocation,
    /// The number of record events the call emitted for this location, instance records included.
    pub records: u64,
    /// The location's outcome. Code 0 on success, including when the location doesn't hold the
    /// object.
    pub error: LoreErrorDetail,
}

/// A branch sync's outcome: the local latest before the call and after it.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionGraphBranchSyncedEventData {
    /// The `id` from the call's arguments.
    pub id: u64,
    /// The branch the call syncs.
    pub branch: BranchId,
    /// The local latest the call read. Where the call failed, the local latest it last observed.
    pub previous_local_latest: Hash,
    /// The local latest when the call finished. Where the call failed, the local latest it last
    /// observed, as `previous_local_latest` holds it.
    pub local_latest: Hash,
}

/// A branch push's outcome: the last revision the call published, the remote latest and the local
/// latest when the call finished.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionGraphBranchPushedEventData {
    /// The `id` from the call's arguments.
    pub id: u64,
    /// The branch the call pushes.
    pub branch: BranchId,
    /// The last revision the call published. Where it published none, the local latest when the
    /// call succeeds, and zero when it fails.
    pub revision_hash: Hash,
    /// The revision number of `revision_hash` on the remote, zero where `revision_hash` is zero.
    pub revision_number: u64,
    /// Set where the remote published `revision_hash` as a fast-forward merge onto a remote latest
    /// that had moved.
    #[serde(with = "u8_as_bool")]
    pub fast_forward_merged: u8,
    /// The remote latest when the call finished.
    pub remote_latest: Hash,
    /// The local latest when the call finished.
    pub local_latest: Hash,
}
