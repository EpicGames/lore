// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ffi::OsString;

use clap::Parser;
use lore::interface::AuthMode;
use lore_client::cli::AuthModeArg;
use lore_client::cli::LoreCli;
use lore_client::cli::LoreCliError;
use lore_client::cli::resolve_auth_mode;

fn env(value: &str) -> Option<OsString> {
    Some(OsString::from(value))
}

/// The flag, then the variable, then `auto`: each source sets the mode when
/// the one before it is silent, and is overridden by it when it is not.
#[test]
fn the_flag_outranks_the_variable_which_outranks_auto() {
    assert_eq!(
        resolve_auth_mode(Some(AuthModeArg::Grpc), env("oidc")),
        Ok(AuthMode::Grpc)
    );
    assert_eq!(resolve_auth_mode(None, env("oidc")), Ok(AuthMode::Oidc));
    assert_eq!(resolve_auth_mode(None, None), Ok(AuthMode::Auto));
}

/// A blank variable is the shell's way of unsetting one without removing it,
/// and reads as unset.
#[test]
fn a_blank_variable_reads_as_unset() {
    assert_eq!(resolve_auth_mode(None, env("  ")), Ok(AuthMode::Auto));
    assert_eq!(resolve_auth_mode(None, env("")), Ok(AuthMode::Auto));
}

#[test]
fn the_variable_is_read_whatever_its_case() {
    assert_eq!(resolve_auth_mode(None, env("OIDC")), Ok(AuthMode::Oidc));
    assert_eq!(resolve_auth_mode(None, env(" grpc ")), Ok(AuthMode::Grpc));
}

/// A tester who mistypes the mode is opting in to a path; keeping them
/// silently on the other one would make the opt-in look broken.
#[test]
fn a_variable_naming_no_mode_is_an_error_not_auto() {
    assert_eq!(
        resolve_auth_mode(None, env("odic")),
        Err(LoreCliError::ParseAuthMode("odic".to_string()))
    );
    let message = LoreCliError::ParseAuthMode("odic".to_string()).to_string();
    assert!(message.contains("LORE_AUTH_MODE"), "{message}");
    assert!(message.contains("oidc"), "{message}");
}

#[test]
fn the_flag_is_global_and_takes_the_three_names() {
    for (name, expected) in [
        ("auto", AuthModeArg::Auto),
        ("grpc", AuthModeArg::Grpc),
        ("oidc", AuthModeArg::Oidc),
    ] {
        let cli = LoreCli::try_parse_from(["lore", "status", "--auth-mode", name])
            .expect("the flag must parse after the command");
        assert_eq!(cli.auth_mode, Some(expected));
        assert_eq!(expected.to_lore().name(), name);
    }
    assert!(
        LoreCli::try_parse_from(["lore", "--auth-mode", "odic", "status"]).is_err(),
        "a name that is not a mode must be refused by the parser"
    );
    let cli = LoreCli::try_parse_from(["lore", "status"]).expect("parses");
    assert_eq!(cli.auth_mode, None);
}
