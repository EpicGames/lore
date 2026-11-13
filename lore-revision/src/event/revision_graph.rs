// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Event data for the `lore_revision_graph_*` verbs: record events, each reporting one object as
//! one location holds it, and the location complete event, which reports a call's outcome at one
//! location.

use lore_base::types::BranchId;
use lore_base::types::Hash;
use serde::Serialize;

use crate::event::LoreErrorDetail;
use crate::instance::InstanceId;
use crate::interface::LoreArray;
use crate::interface::LoreBranchLocation;
use crate::interface::LoreBranchPoint;
use crate::interface::LoreString;
use crate::util::serde::u8_as_bool;

/// One branch, as one location holds it.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
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
    /// Set when the local latest may hold revisions the remote lacks. Zero on a remote record.
    #[serde(with = "u8_as_bool")]
    pub divergent: u8,
}

/// One repository instance on a branch, as the local store records it.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
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
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
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

/// The call's outcome at one location, after that location's record events.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
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
