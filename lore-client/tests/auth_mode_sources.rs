// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Resolves the auth mode through the real `LORE_AUTH_MODE` variable, which is
//! process-wide state, so this runs in a process of its own.
use clap::Parser;
use lore::interface::AuthMode;
use lore_client::cli::LoreCli;
use lore_client::cli::lore_globals_from_args;

fn resolved_mode(args: &[&str]) -> AuthMode {
    let cli = LoreCli::try_parse_from(args).expect("the arguments must parse");
    lore_globals_from_args(&cli)
        .expect("the globals must resolve")
        .auth_mode
}

/// Each source sets the mode on its own, and the flag overrides the variable.
#[test]
fn each_source_sets_the_mode_in_order_of_precedence() {
    // safety: the only test in this binary, so nothing else reads the environment
    unsafe { std::env::remove_var("LORE_AUTH_MODE") };
    assert_eq!(
        resolved_mode(&["lore", "status"]),
        AuthMode::Auto,
        "nothing set is auto"
    );

    unsafe { std::env::set_var("LORE_AUTH_MODE", "oidc") };
    assert_eq!(
        resolved_mode(&["lore", "status"]),
        AuthMode::Oidc,
        "the variable sets the mode"
    );

    assert_eq!(
        resolved_mode(&["lore", "status", "--auth-mode", "grpc"]),
        AuthMode::Grpc,
        "the flag outranks the variable"
    );

    unsafe { std::env::set_var("LORE_AUTH_MODE", "odic") };
    let cli = LoreCli::try_parse_from(["lore", "status"]).expect("parses");
    assert!(
        lore_globals_from_args(&cli).is_err(),
        "a variable naming no mode fails the command"
    );
}
