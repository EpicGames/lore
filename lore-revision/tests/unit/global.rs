// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::time::Duration;

use lore_revision::global::*;

/// Every field written and read back through TOML, so that a field added
/// to the config is known to survive being saved and loaded rather than
/// only being readable from a file someone wrote by hand.
#[test]
fn a_fully_populated_config_survives_a_round_trip() {
    let mut config = GlobalConfig {
        use_shared_store_automatically: Some(true),
        ..GlobalConfig::default()
    };
    config
        .set_default_path_for_remote_url("lore://example", "/srv/shared")
        .expect("the shared store path must be settable");
    config.service.executable = Some("/opt/lore/1.9/bin/lore".to_string());
    config.service.idle_connection_timeout = Some(42);

    let written = toml::to_string(&config).expect("the config must be writable as TOML");
    let read: GlobalConfig = toml::from_str(&written).expect("and readable back");

    assert_eq!(read.service_executable(), Some("/opt/lore/1.9/bin/lore"));
    assert_eq!(
        read.service_idle_connection_timeout(),
        Duration::from_secs(42)
    );
    assert!(read.use_shared_store_automatically());
    assert_eq!(read.all_default_shared_stores().count(), 1);
}

#[test]
fn no_executable_is_named_by_default() {
    assert_eq!(GlobalConfig::default().service_executable(), None);
}

/// Blanking the field is how a pin is removed, so it reads as unset rather
/// than as an executable with no name.
#[test]
fn an_empty_executable_reads_as_unset() {
    let config: GlobalConfig = toml::from_str("[service]\nexecutable = \"\"\n").expect("readable");
    assert_eq!(config.service_executable(), None);
}
