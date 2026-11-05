// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod file_failure_tests;
mod operation_tests;
mod rehash_bench;
mod rehash_tests;
mod statistics_level_tests;
mod sweep_tests;
mod walk_tests;
mod weave_tests;

use lore_base::error::NotALayer;
use lore_revision::commit::*;

#[test]
fn commit_options_new_has_empty_layer_defaults() {
    let opts = CommitOptions::new("msg".into());
    assert!(opts.layer_messages.is_empty());
    assert!(opts.layer.is_none());
    assert!(opts.link_messages.is_empty());
    assert!(opts.link.is_none());
}

#[test]
fn commit_error_carries_not_a_layer() {
    let err: CommitError = NotALayer {
        path: "external/lib".into(),
    }
    .into();
    assert!(matches!(err, CommitError::NotALayer { .. }));
    assert!(err.to_string().contains("external/lib"));
}
