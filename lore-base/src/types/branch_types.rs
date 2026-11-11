// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

use super::BranchId;
use super::Hash;

/// A branch paired with one revision on that branch.
#[derive(Clone, Debug, Default, PartialEq, FromBytes, IntoBytes, Immutable)]
pub struct BranchPoint {
    /// Branch identifier.
    pub branch: BranchId,
    /// Revision hash on the branch.
    pub revision: Hash,
}

/// Descriptive information about a branch.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BranchMetadata {
    /// Branch identifier.
    pub id: BranchId,
    /// Branch name.
    pub name: String,
    /// Category the branch belongs to.
    pub category: String,
    /// Hash of the latest revision on the branch.
    pub latest: Hash,
    /// Name of the user who created the branch.
    pub creator: String,
    /// Creation timestamp.
    pub created: u64,
    /// Ordered list of branch points the branch is built on.
    pub stack: Vec<BranchPoint>,
}

impl BranchMetadata {
    /// The branch point the branch was created at, where it was created from another branch.
    pub fn branch_point(&self) -> Option<&BranchPoint> {
        self.stack.first()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: BranchId,
        name: String,
        category: String,
        latest: Hash,
        creator: String,
        created: u64,
        stack: Vec<BranchPoint>,
    ) -> Self {
        Self {
            id,
            name,
            category,
            latest,
            creator,
            created,
            stack,
        }
    }
}
