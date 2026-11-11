// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use clap::Parser;
use lore_client::cli::LoreCli;
use lore_client::cli::LoreCommands;
use lore_client::client_main::runs_the_service;

fn command_of(args: &[&str]) -> LoreCommands {
    LoreCli::try_parse_from(args)
        .expect("the arguments must parse")
        .command
        .expect("the arguments must name a command")
}

/// `service run` is the service. Sizing it for relaying would leave the
/// process that does all the work with the pools of one that does none.
#[test]
fn the_command_that_runs_the_service_is_not_sized_for_relaying() {
    assert!(runs_the_service(&command_of(&["lore", "service", "run"])));
}

/// Every other `service` command is a client of the service, including the
/// two that ask for one to start and stop.
#[test]
fn the_commands_that_act_on_the_service_are_sized_for_relaying() {
    for args in [
        vec!["lore", "service", "start"],
        vec!["lore", "service", "stop"],
        vec!["lore", "service", "set-executable", "/opt/lore/bin/lore"],
        vec!["lore", "service", "set-use-automatically", "true"],
        vec!["lore", "status"],
    ] {
        assert!(
            !runs_the_service(&command_of(&args)),
            "{} is a client of the service",
            args.join(" ")
        );
    }
}

/// `--trust-remote` reaches the command's globals, and `--fast-flush` the options of its flushes,
/// and each is off without its flag.
#[test]
fn trusting_the_remote_and_fast_flush_reach_the_globals() {
    for (args, trusted, fast_flush) in [
        (vec!["lore", "status"], 0, false),
        (vec!["lore", "status", "--trust-remote"], 1, false),
        (vec!["lore", "status", "--fast-flush"], 0, true),
    ] {
        let cli = LoreCli::try_parse_from(&args).expect("the arguments must parse");
        let globals =
            lore_client::cli::lore_globals_from_args(&cli).expect("the globals must resolve");
        assert_eq!(globals.trust_remote, trusted, "{}", args.join(" "));
        assert_eq!(
            globals.flush_options().fast_flush,
            fast_flush,
            "{}",
            args.join(" ")
        );
    }
}

/// The `[auth]` settings are set for the variables the environment leaves unset or blank, and
/// left alone for those it sets.
#[test]
fn the_auth_settings_fill_only_unset_or_blank_variables() {
    use std::ffi::OsString;

    use lore_client::client_main::auth_variables_to_set;

    let settings = lore::AuthSettings {
        store: None,
        path: Some("/tokens".into()),
    };
    let filled = vec![("LORE_AUTH_PATH", OsString::from("/tokens"))];
    assert_eq!(auth_variables_to_set(&settings, |_| None), filled, "unset");
    assert_eq!(
        auth_variables_to_set(&settings, |_| Some(OsString::new())),
        filled,
        "blank"
    );
    assert!(
        auth_variables_to_set(&settings, |_| Some(OsString::from("/mine"))).is_empty(),
        "set"
    );
}
