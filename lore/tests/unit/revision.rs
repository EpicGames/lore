// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore::revision::*;
use lore_revision::interface::LoreGlobalArgs;

/// A command runs in its handler's future, with no future around it holding the arguments
/// again.
#[test]
fn a_command_runs_in_its_handlers_future() {
    let handler = commit_local(
        LoreGlobalArgs::default(),
        LoreRevisionCommitArgs::default(),
        None,
    );
    let command = lore::args::InvokableLoreArgs::invoke_local(
        LoreRevisionCommitArgs::default(),
        LoreGlobalArgs::default(),
        None,
    );

    assert_eq!(size_of_val(&command), size_of_val(&handler));
}
