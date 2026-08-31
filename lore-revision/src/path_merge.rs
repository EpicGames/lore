// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use crate::change::NodeChange;
use crate::util::path::RelativePath;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PathMergeStrategy {
    #[default]
    Merge,
    KeepTarget,
    Exclude,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PathMergeRule {
    /// Repository-relative path the rule applies to. A rule matches the path
    /// itself and descendants below it.
    pub path: RelativePath,
    pub strategy: PathMergeStrategy,
}

#[derive(Clone, Copy, Debug)]
pub struct PathMergePolicy<'a> {
    rules: &'a [PathMergeRule],
}

impl Default for PathMergePolicy<'_> {
    fn default() -> Self {
        Self { rules: &[] }
    }
}

impl<'a> PathMergePolicy<'a> {
    pub fn new(rules: &'a [PathMergeRule]) -> Self {
        Self { rules }
    }

    pub fn is_empty(self) -> bool {
        self.rules.is_empty()
    }

    pub fn strategy_for_change(self, change: &NodeChange) -> PathMergeStrategy {
        self.rules
            .iter()
            .enumerate()
            .filter(|(_, rule)| path_merge_rule_matches_change(rule, change))
            .max_by_key(|(index, rule)| (rule.path.len(), *index))
            .map(|(_, rule)| rule.strategy)
            .unwrap_or_default()
    }

    pub fn strategy_for_conflict(
        self,
        source_change: &NodeChange,
        target_change: &NodeChange,
    ) -> PathMergeStrategy {
        self.rules
            .iter()
            .enumerate()
            .filter(|(_, rule)| {
                path_merge_rule_matches_change(rule, source_change)
                    || path_merge_rule_matches_change(rule, target_change)
            })
            .max_by_key(|(index, rule)| (rule.path.len(), *index))
            .map(|(_, rule)| rule.strategy)
            .unwrap_or_default()
    }

    pub fn should_merge_change(self, change: &NodeChange) -> bool {
        self.strategy_for_change(change) == PathMergeStrategy::Merge
    }

    pub fn should_merge_conflict(
        self,
        source_change: &NodeChange,
        target_change: &NodeChange,
    ) -> bool {
        self.strategy_for_conflict(source_change, target_change) == PathMergeStrategy::Merge
    }
}

fn path_merge_rule_matches_change(rule: &PathMergeRule, change: &NodeChange) -> bool {
    rule.path.overlaps(&change.path)
        || change
            .from_path
            .as_ref()
            .is_some_and(|from_path| rule.path.overlaps(from_path))
}
