// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod weighting;

use std::path::PathBuf;

use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;
use tracing::info;

use crate::lore::state::LoreState;
use crate::operations::CommitOperation;
use crate::operations::FileChangeOperation;
use crate::operations::FileMergeOperation;
use crate::operations::RepoOperationKind;
use crate::probability::weighting::ProbabilityWeighting;

pub struct ProbabilityEngine {
    #[allow(unused)]
    seed: u64,
    /// Rng for generating Operation values and the Rng seed for values gotten during that Operation.
    operation_rng: StdRng,
    /// Rng for generating values for the current ongoing operation.
    current_operation_rng: StdRng,
    /// Rng for generating values at runtime needed for cases where something unexpected happens.
    runtime_rng: StdRng,
    weights: ProbabilityWeighting,
}

pub enum ProbabilityType {
    Picking,
    Runtime,
}

impl ProbabilityEngine {
    pub fn new(weights: ProbabilityWeighting, seed: Option<u64>) -> Self {
        info!("Probability using seed {seed:?}");
        let seed = seed.unwrap_or_else(|| rand::rng().random());
        let mut root_rng = StdRng::seed_from_u64(seed);
        let operation_rng = StdRng::seed_from_u64(root_rng.random());
        let current_operation_rng = StdRng::seed_from_u64(root_rng.random());
        let runtime_rng = StdRng::seed_from_u64(root_rng.random());
        Self {
            seed,
            operation_rng,
            current_operation_rng,
            runtime_rng,
            weights,
        }
    }

    pub fn pick_repo_operation_kind(&mut self) -> RepoOperationKind {
        let operation = self
            .weights
            .repo_operation
            .generate(&mut self.operation_rng);
        self.current_operation_rng = StdRng::seed_from_u64(self.operation_rng.random());
        operation
    }

    pub fn pick_file_operations(
        &mut self,
        branch_files: Vec<PathBuf>,
    ) -> (CommitOperation, Vec<PathBuf>) {
        let proportion_to_modify = self.weights.file_operation_weights.proportion_to_modify;
        // Adjusted so that P(not modified) * P(adjusted_delete) = P(delete)
        let adjusted_proportion_to_delete =
            self.weights.file_operation_weights.proportion_to_delete / (1.0 - proportion_to_modify);

        let mut result_operation = CommitOperation {
            changes: Vec::new(),
            description: self.make_name(),
        };
        let mut result_branch_files = Vec::new();
        for file in branch_files {
            if self.random_probability() < proportion_to_modify {
                result_operation
                    .changes
                    .push((file.clone(), FileChangeOperation::Modify));
                result_branch_files.push(file);
            } else if self.random_probability() < adjusted_proportion_to_delete {
                result_operation
                    .changes
                    .push((file, FileChangeOperation::Delete));
            } else {
                result_branch_files.push(file);
            }
        }

        let num_new_files = self
            .current_operation_rng
            .random_range(0..=self.weights.file_operation_weights.max_number_to_add);
        for _ in 0..num_new_files {
            let new_name = PathBuf::from(self.make_file_name(&result_branch_files));
            result_branch_files.push(new_name.clone());
            result_operation
                .changes
                .push((new_name, FileChangeOperation::Add));
        }
        (result_operation, result_branch_files)
    }

    pub fn pick_branch(&mut self, state: &LoreState) -> usize {
        self.current_operation_rng
            .random_range(0..state.branches.len())
    }

    pub fn pick_other_branch(&mut self, state: &LoreState) -> Option<usize> {
        if state.branches.len() < 2 {
            None
        } else {
            let mut index = self
                .current_operation_rng
                .random_range(0..state.branches.len() - 1);
            if index >= state.current_branch {
                index += 1;
            }
            Some(index)
        }
    }

    pub fn pick_merge_resolutions(
        &mut self,
        target_files: Vec<PathBuf>,
        source_files: &[PathBuf],
    ) -> (Vec<(PathBuf, FileMergeOperation)>, Vec<PathBuf>) {
        let mut merged_files = Vec::new();
        let mut result_files = target_files;
        for merged_file in source_files.iter() {
            if result_files.contains(merged_file) {
                merged_files.push((
                    merged_file.clone(),
                    self.pick_file_merge_operation(ProbabilityType::Picking),
                ));
            } else {
                result_files.push(merged_file.clone());
            }
        }
        (merged_files, result_files)
    }

    pub fn pick_file_merge_operation(
        &mut self,
        probability_type: ProbabilityType,
    ) -> FileMergeOperation {
        FileMergeOperation::from_kind(
            self.weights
                .file_merge_weights
                .generate(match probability_type {
                    ProbabilityType::Picking => &mut self.current_operation_rng,
                    ProbabilityType::Runtime => &mut self.runtime_rng,
                }),
            || self.make_file_contents(),
        )
    }

    pub fn make_branch_name(&mut self, state: &LoreState) -> String {
        let is_allowed = |name: &str| !state.branches.iter().any(|branch| branch.name == name);
        let mut generated_name: Option<String> = None;
        while generated_name.is_none() || !is_allowed(generated_name.as_ref().unwrap()) {
            generated_name = Some(self.make_name());
        }
        generated_name.unwrap()
    }

    pub fn make_file_name(&mut self, current_files: &[PathBuf]) -> String {
        // TODO: Add files that are in directories
        let is_allowed = |name: &str| {
            !current_files
                .iter()
                .any(|file| file.to_str().unwrap() == name)
        };
        let mut generated_name: Option<String> = None;
        while generated_name.is_none() || !is_allowed(generated_name.as_ref().unwrap()) {
            generated_name = Some(self.make_name());
        }
        generated_name.unwrap()
    }

    pub fn make_file_contents(&mut self) -> String {
        self.make_file_name(&[])
    }

    pub fn make_bool(&mut self) -> bool {
        self.current_operation_rng.random()
    }
}

impl ProbabilityEngine {
    /// Generates an 8 character name that can be a file name or a branch name.
    fn make_name(&mut self) -> String {
        let alpha = self.current_operation_rng.sample(rand::distr::Alphabetic) as char;
        let mut alphanumeric =
            || self.current_operation_rng.sample(rand::distr::Alphanumeric) as char;
        format!(
            "{}{}{}{}{}{}{}{}",
            alpha,
            alphanumeric(),
            alphanumeric(),
            alphanumeric(),
            alphanumeric(),
            alphanumeric(),
            alphanumeric(),
            alphanumeric()
        )
        .to_lowercase()
    }

    fn random_probability(&mut self) -> f32 {
        self.current_operation_rng.random_range(0.0..1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    pub fn merge_states() {
        let mut engine = ProbabilityEngine::new(Default::default(), Some(1000));
        let source_files: Vec<PathBuf> = ["a", "b", "c", "d"].map(PathBuf::from).to_vec();
        let target_files: Vec<PathBuf> = ["b", "c", "d", "e"].map(PathBuf::from).to_vec();
        let (resolutions, mut all_files) =
            engine.pick_merge_resolutions(target_files.clone(), &source_files);

        for resolution in resolutions {
            assert!(target_files.contains(&resolution.0));
            assert!(source_files.contains(&resolution.0));
        }

        let mut expected_files = ["a", "b", "c", "d", "e"].map(PathBuf::from).to_vec();
        expected_files.sort();
        all_files.sort();
        assert_eq!(expected_files, all_files);
    }
}
