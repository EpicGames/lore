// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod io_pool;
mod protocol;
mod resource;
mod trace;

use lore_server::telemetry::*;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::registry::Registry;

fn max_level(directives: &str) -> Option<LevelFilter> {
    <EnvFilter as Layer<Registry>>::max_level_hint(&log_filter(directives))
}

/// A standalone server start sets no `RUST_LOG`, and an operator running
/// one wants to see the warnings it emits.
#[test]
fn no_directives_enable_warnings() {
    assert_eq!(max_level(""), Some(LevelFilter::WARN));
}

#[test]
fn a_directive_overrides_the_default() {
    assert_eq!(max_level("info"), Some(LevelFilter::INFO));
}

/// A large deployment cuts the output back down to errors alone.
#[test]
fn errors_only_remains_available() {
    assert_eq!(max_level("error"), Some(LevelFilter::ERROR));
}

#[test]
fn an_unparsable_directive_leaves_the_default_in_force() {
    assert_eq!(max_level("=!=not a directive=!="), Some(LevelFilter::WARN));
}
