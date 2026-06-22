// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::cmp::max;
use std::cmp::min;
use std::path::PathBuf;

use crate::probability::ProbabilityEngine;

#[derive(Debug, Clone)]
pub struct LoreState {
    pub current_branch: usize,
    pub branches: Vec<BranchState>,
}

#[derive(Debug, Clone)]
pub struct BranchState {
    pub name: String,
    pub files: Vec<PathBuf>,
}

impl Default for LoreState {
    fn default() -> Self {
        Self {
            branches: vec![BranchState {
                name: "main".to_string(),
                files: Vec::new(),
            }],
            current_branch: 0,
        }
    }
}

impl LoreState {
    pub fn add_new_current_branch(&mut self, branch_name: String) {
        let files = self.mut_current_branch().files.clone();
        self.branches.push(BranchState {
            name: branch_name,
            files,
        });
        self.current_branch = self.branches.len() - 1;
    }

    pub fn mut_current_branch(&mut self) -> &mut BranchState {
        &mut self.branches[self.current_branch]
    }

    pub fn mut_current_and_other_branch(
        &'_ mut self,
        probability: &mut ProbabilityEngine,
    ) -> Option<MutTwoBranches<'_>> {
        let other_branch_index = probability.pick_other_branch(self)?;
        let min_index = min(other_branch_index, self.current_branch);
        let max_index = max(other_branch_index, self.current_branch);
        let (half1, half2) = self.branches.split_at_mut(max_index);
        let min_branch = half1.get_mut(min_index).unwrap();
        let max_branch = half2.get_mut(0).unwrap();
        if self.current_branch < other_branch_index {
            Some(MutTwoBranches {
                current: min_branch,
                other: max_branch,
            })
        } else {
            Some(MutTwoBranches {
                current: max_branch,
                other: min_branch,
            })
        }
    }
}

pub struct MutTwoBranches<'a> {
    pub current: &'a mut BranchState,
    pub other: &'a mut BranchState,
}
