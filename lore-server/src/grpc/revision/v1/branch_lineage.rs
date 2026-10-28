// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::branch::CachedRevisionItem;
use lore_revision::lore::BranchId;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_telemetry::tracing::fields::BRANCH_ID;
use tokio::sync::OnceCell;
use tonic::Status;
use tracing::warn;

use crate::grpc::FilterSlowDownExt;

struct AncestorPoint {
    branch: BranchId,
    /// Revision this level was branched from, on `branch`.
    revision: Hash,
    /// Number of `revision`, read on first use. `None` when it cannot be read.
    number: OnceCell<Option<u64>>,
}

impl AncestorPoint {
    async fn number(&self, repository: &Arc<RepositoryContext>) -> Result<Option<u64>, Status> {
        self.number
            .get_or_try_init(|| async {
                if self.revision.is_zero() {
                    return Ok(Some(0));
                }
                match State::deserialize(repository.clone(), self.revision)
                    .await
                    .filter_slow_down()?
                {
                    Ok(state) => Ok(Some(state.revision_number())),
                    Err(err) => {
                        warn!(
                            {BRANCH_ID} = %self.branch, revision = %self.revision, ?err,
                            "Failed to read a branch point on an ancestor of the requested branch",
                        );
                        Ok(None)
                    }
                }
            })
            .await
            .copied()
    }
}

/// The branch whose acceleration data covers a revision number, and how much
/// of that branch's history the requested branch shares.
pub(crate) struct HistoryOwner {
    pub branch: BranchId,
    /// Highest number on `branch` that the requested branch's history
    /// includes. `None` when `branch` is the requested branch itself.
    limit: Option<u64>,
}

impl HistoryOwner {
    pub fn requested(branch: BranchId) -> Self {
        Self {
            branch,
            limit: None,
        }
    }

    /// The part of a newest-first cached segment that belongs to the requested
    /// branch's history.
    pub fn visible<'a>(&self, items: &'a [CachedRevisionItem]) -> &'a [CachedRevisionItem] {
        match self.limit {
            Some(limit) => &items[items.partition_point(|item| item.number > limit)..],
            None => items,
        }
    }
}

/// The ancestry of a requested branch, which decides whose acceleration data
/// describes a given revision number.
///
/// A revision belongs to the branch it was created on, and the step keys and
/// list cache describing it are keyed by that branch. A branch created from
/// another shares its parent's history up to the branch point, so a number at
/// or below that point is described by the ancestor and not by the branch.
///
/// Nothing is read until the first question about ownership, so a request the
/// requested branch's own data can answer never pays for it.
pub(crate) struct BranchLineage {
    branch: BranchId,
    /// Nearest ancestor first, matching the order of the branch stack. `None`
    /// when the branch's metadata cannot be read, which leaves lookups to
    /// behave as they would without a lineage.
    ancestors: OnceCell<Option<Vec<AncestorPoint>>>,
}

impl BranchLineage {
    pub fn new(branch: BranchId) -> Self {
        Self {
            branch,
            ancestors: OnceCell::new(),
        }
    }

    pub fn branch(&self) -> BranchId {
        self.branch
    }

    async fn load_ancestors(
        &self,
        repository: &Arc<RepositoryContext>,
    ) -> Result<Option<Vec<AncestorPoint>>, Status> {
        let metadata = match branch::metadata_local(repository.clone(), self.branch)
            .await
            .filter_slow_down()?
        {
            Ok(metadata) => metadata,
            Err(err) => {
                // A branch that does not exist is reported by the lookup itself.
                if !err.is_branch_not_found() {
                    warn!(
                        {BRANCH_ID} = %self.branch, ?err,
                        "Failed to read the metadata of the requested branch",
                    );
                }
                return Ok(None);
            }
        };

        Ok(Some(
            branch::stack(&metadata)
                .into_iter()
                .map(|point| AncestorPoint {
                    branch: point.branch,
                    revision: point.revision,
                    number: OnceCell::new(),
                })
                .collect(),
        ))
    }

    /// The branch that holds `number` in the requested branch's history, or
    /// `None` when the ancestry needed to decide cannot be read.
    pub async fn owner_of(
        &self,
        repository: &Arc<RepositoryContext>,
        number: u64,
    ) -> Result<Option<HistoryOwner>, Status> {
        let Some(ancestors) = self
            .ancestors
            .get_or_try_init(|| self.load_ancestors(repository))
            .await?
        else {
            return Ok(None);
        };

        let mut owner = HistoryOwner::requested(self.branch);
        for ancestor in ancestors {
            // Levels are read in order and only while the number is still at
            // or below the previous branch point.
            let Some(point_number) = ancestor.number(repository).await? else {
                return Ok(None);
            };
            if number > point_number {
                break;
            }
            owner = HistoryOwner {
                branch: ancestor.branch,
                limit: Some(point_number),
            };
        }
        Ok(Some(owner))
    }
}
