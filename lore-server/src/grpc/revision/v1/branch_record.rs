// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::types::Hash;
use lore_proto::lore::model::v1 as model_v1;
use lore_revision::branch;
use lore_revision::lore::BranchId;
use lore_revision::metadata::Metadata;

/// Build a v1 `Branch` response record from already-loaded values.
///
/// Callers pass `metadata`, `metadata_hash` and `latest` they have in scope so
/// the helper fits both pre-load paths (e.g. delete preserves metadata for the
/// deleted response) and post-mutation paths. Reading `latest` is left to the
/// caller because how an unreadable latest should be handled depends on whether
/// the caller has already committed a mutation. Missing metadata fields
/// (legacy / partial blobs) fall back to defaults rather than erroring.
pub(super) fn build_branch(
    branch_id: BranchId,
    metadata: &Metadata,
    metadata_hash: Hash,
    deleted: bool,
    latest: Hash,
) -> model_v1::Branch {
    let name = branch::name(metadata).unwrap_or_default().to_string();
    let creator = branch::creator(metadata).unwrap_or_default().to_string();
    let category = branch::category(metadata).unwrap_or_default().to_string();
    let created = branch::created(metadata);
    let stack: Vec<model_v1::BranchPoint> = branch::stack(metadata)
        .iter()
        .map(model_v1::BranchPoint::from)
        .collect();
    model_v1::Branch {
        id: branch_id.into(),
        name,
        creator,
        category,
        created,
        latest: latest.into(),
        deleted,
        metadata: metadata_hash.into(),
        stack,
    }
}
