// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use rand::Rng;
use rand_distr::Distribution;
use rand_distr::weighted::WeightedAliasIndex;

use crate::operations::FileMergeOperationKind;
use crate::operations::RepoOperationKind;

pub struct Weighting<T: Clone> {
    values: Vec<T>,
    table: WeightedAliasIndex<f32>,
}

impl<T: Clone> Weighting<T> {
    pub fn new(weighted_values: Vec<(f32, T)>) -> Self {
        let weights: Vec<f32> = weighted_values.iter().map(|&(weight, _)| weight).collect();
        let values = weighted_values.into_iter().map(|(_, t)| t).collect();
        let table = WeightedAliasIndex::new(weights).unwrap();
        Self { values, table }
    }

    pub fn generate(&mut self, rng: &mut impl Rng) -> T {
        self.values[self.table.sample(rng)].clone()
    }
}

pub struct ProbabilityWeighting {
    pub repo_operation: RepoOperationWeighting,
    pub file_operation_weights: FileOperationWeights,
    pub file_merge_weights: FileMergeWeights,
}

impl ProbabilityWeighting {
    pub fn read_only() -> Self {
        Self {
            repo_operation: RepoOperationWeighting::new(
                RepoOperationKind::ALL_READ_ONLY
                    .iter()
                    .map(|kind| (1.0, *kind))
                    .collect(),
            ),
            ..Default::default()
        }
    }
}

impl Default for ProbabilityWeighting {
    fn default() -> Self {
        Self {
            repo_operation: RepoOperationWeighting::new(
                RepoOperationKind::ALL
                    .iter()
                    .map(|kind| (1.0, *kind))
                    .collect(),
            ),
            file_operation_weights: Default::default(),
            file_merge_weights: FileMergeWeights::new(
                FileMergeOperationKind::ALL
                    .iter()
                    .map(|kind| (1.0, *kind))
                    .collect(),
            ),
        }
    }
}

pub type RepoOperationWeighting = Weighting<RepoOperationKind>;
pub type FileMergeWeights = Weighting<FileMergeOperationKind>;

pub struct FileOperationWeights {
    pub proportion_to_modify: f32,
    pub proportion_to_delete: f32,
    pub max_number_to_add: usize,
}

impl Default for FileOperationWeights {
    fn default() -> Self {
        Self {
            proportion_to_modify: 0.25,
            // Deletes are currently defaulted to off because tracking them across merges is difficult.
            proportion_to_delete: 0.0,
            max_number_to_add: 10,
        }
    }
}
