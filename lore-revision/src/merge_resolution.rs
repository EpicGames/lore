// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Stable merge-conflict identities and whole-side conflict resolution.
//!
//! This module is deliberately independent of gRPC. Callers can compute a
//! [`MergePlan`] from Lore's normal three-way [`DiffResult`], present its
//! stable conflict ids through any UI, then resolve each conflict to the
//! target or source side before applying the returned changes to the target
//! state.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use lore_base::types::Hash;

use crate::change::Flags;
use crate::change::NodeChange;
use crate::revision::DiffResult;

/// Complete side to retain for a conflict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictSide {
    /// Preserve the target state. Because merge changes are applied onto the
    /// target tree, this requires no tree edit.
    Target,
    /// Apply the source-side change to the target tree.
    Source,
}

/// One conflict in a three-way merge plan.
#[derive(Clone, Debug)]
pub struct MergeConflict {
    pub id: Hash,
    pub source: NodeChange,
    pub target: NodeChange,
}

/// A reusable, immutable description of a three-way merge.
#[derive(Debug)]
pub struct MergePlan {
    pub base: Hash,
    pub source: Hash,
    pub target: Hash,
    pub changes: Vec<NodeChange>,
    pub conflicts: Vec<MergeConflict>,
}

/// Result of applying a partial set of whole-side decisions to a plan.
#[derive(Debug)]
pub struct ResolvedMerge {
    /// Conflict-free edits to apply to the target tree.
    pub changes: Vec<NodeChange>,
    /// Conflicts that still require a decision.
    pub unresolved: Vec<MergeConflict>,
    /// Supplied ids that do not occur in this merge plan.
    pub unknown_resolution_ids: Vec<Hash>,
}

impl MergePlan {
    /// Attach stable ids to a three-way diff result.
    pub fn new(result: DiffResult) -> Self {
        // revision::diff3 also emits target-only changes so presentation
        // callers can describe the complete joined diff. A merge is applied
        // onto that target already, so only source-side edits belong in the
        // executable plan.
        let changes = result
            .changes
            .into_iter()
            .filter(|change| change.to.state.revision() == result.source)
            .collect();
        let conflicts = result
            .conflicts
            .into_iter()
            .map(|(source, target)| MergeConflict {
                id: conflict_id(result.base, result.source, result.target, &source, &target),
                source,
                target,
            })
            .collect();
        Self {
            base: result.base,
            source: result.source,
            target: result.target,
            changes,
            conflicts,
        }
    }

    /// Apply any supplied target/source choices. A target choice is omitted
    /// from `changes`, because the edits are applied to the target state. A
    /// source choice contributes the source-side change with merge-conflict
    /// flags removed.
    pub fn resolve(&self, resolutions: &BTreeMap<Hash, ConflictSide>) -> ResolvedMerge {
        let known: BTreeSet<Hash> = self.conflicts.iter().map(|conflict| conflict.id).collect();
        let mut changes = self.changes.clone();
        let mut unresolved = Vec::new();

        for conflict in &self.conflicts {
            match resolutions.get(&conflict.id) {
                Some(ConflictSide::Target) => {}
                Some(ConflictSide::Source) => {
                    let mut source = conflict.source.clone();
                    source.flags = if source.flags.contains(Flags::Modify) {
                        Flags::Modify
                    } else {
                        Flags::None
                    };
                    changes.push(source);
                }
                None => unresolved.push(conflict.clone()),
            }
        }

        let unknown_resolution_ids = resolutions
            .keys()
            .filter(|id| !known.contains(id))
            .copied()
            .collect();
        ResolvedMerge {
            changes,
            unresolved,
            unknown_resolution_ids,
        }
    }
}

/// Compute a stable id for a conflict under pinned base/source/target tips.
/// Stream-local partition indices and node ids are intentionally excluded.
pub fn conflict_id(
    base: Hash,
    source_revision: Hash,
    target_revision: Hash,
    source: &NodeChange,
    target: &NodeChange,
) -> Hash {
    let mut bytes = Vec::with_capacity(256);
    bytes.extend_from_slice(b"lore.merge-conflict.v1\0");
    bytes.extend_from_slice(base.data());
    bytes.extend_from_slice(source_revision.data());
    bytes.extend_from_slice(target_revision.data());
    append_change(&mut bytes, source);
    append_change(&mut bytes, target);
    Hash::hash_buffer(&bytes)
}

fn append_change(bytes: &mut Vec<u8>, change: &NodeChange) {
    bytes.extend_from_slice(&(change.action as u16).to_le_bytes());
    append_string(bytes, change.path.as_str());
    match change.from_path.as_ref() {
        Some(path) => {
            bytes.push(1);
            append_string(bytes, path.as_str());
        }
        None => bytes.push(0),
    }
    append_state(bytes, &change.from);
    append_state(bytes, &change.to);
}

fn append_state(bytes: &mut Vec<u8>, state: &crate::change::NodeChangeState) {
    bytes.extend_from_slice(state.repository.id.data());
    bytes.extend_from_slice(state.address.hash.data());
    bytes.extend_from_slice(state.address.context.data());
    bytes.extend_from_slice(&state.flags.bits().to_le_bytes());
}

fn append_string(bytes: &mut Vec<u8>, value: &str) {
    bytes.extend_from_slice(&(value.len() as u64).to_le_bytes());
    bytes.extend_from_slice(value.as_bytes());
}
