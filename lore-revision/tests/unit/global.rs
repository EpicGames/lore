// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use lore_base::env::AuthStore;
use lore_base::test_util::TempDir;
use lore_revision::global::*;
use lore_revision::util::config::SaveableConfig;

/// A config as a newer version of Lore writes it, with keys and tables this version doesn't know at every
/// level, among the settings it does know.
const CONFIG_WITH_UNKNOWN_KEYS: &str = r#"
future_setting = "kept"
use_shared_store_automatically = true

[future_table]
when = 2026-11-11T12:00:00Z
values = [1, 2, 3]

[default_shared_stores.example]
path_to_store = "/srv/shared"
future_store_setting = 7

[[future_list]]
name = "a"

[[future_list]]
name = "b"

[service]
executable = "/opt/lore/bin/lore"
future_service_setting = { nested = true }

[[service.future_hooks]]
command = "notify"

[auth]
path = "tokens"
future_auth_setting = "kept"
"#;

/// Writes `contents` as the config in a fresh directory, returning the directory and the path.
// A test fixture's write, outside any repository and its write token.
#[allow(clippy::disallowed_methods)]
fn config_file(contents: &str) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new("lore-global-config-");
    let path = dir.child(CONFIG);
    std::fs::write(&path, contents).expect("the config must be writable");
    (dir, path)
}

/// The config at `path` as plain TOML, every key included.
fn saved_table(path: &Path) -> toml::Table {
    let saved = std::fs::read_to_string(path).expect("the saved config must be readable");
    toml::from_str(&saved).expect("the saved config must be TOML")
}

/// A save changes the setting it was asked to and writes back the keys it doesn't know.
#[tokio::test]
async fn keys_it_does_not_know_survive_a_save() {
    let (_dir, path) = config_file(CONFIG_WITH_UNKNOWN_KEYS);

    let (mut config, lock) = GlobalConfig::load_locked_from_path(&path)
        .await
        .expect("the config must load");
    config.service.executable = Some("/opt/lore/2.0/bin/lore".to_string());
    config
        .save_at_path(lock, &path)
        .await
        .expect("the config must save");

    let expected: toml::Table = toml::from_str(
        &CONFIG_WITH_UNKNOWN_KEYS.replace("/opt/lore/bin/lore", "/opt/lore/2.0/bin/lore"),
    )
    .expect("the expected config must be TOML");
    assert_eq!(saved_table(&path), expected);
}

/// A new default path for a remote that already has one keeps that entry's other keys.
#[tokio::test]
async fn a_new_default_shared_store_path_keeps_the_keys_it_does_not_know() {
    let (_dir, path) = config_file(CONFIG_WITH_UNKNOWN_KEYS);

    let (mut config, lock) = GlobalConfig::load_locked_from_path(&path)
        .await
        .expect("the config must load");
    config
        .set_default_path_for_remote_url("lore://example", "/srv/other")
        .expect("the shared store path must be settable");
    config
        .save_at_path(lock, &path)
        .await
        .expect("the config must save");

    let saved = saved_table(&path);
    let entry = &saved["default_shared_stores"]["example"];
    assert_eq!(entry["path_to_store"].as_str(), Some("/srv/other"));
    assert_eq!(entry["future_store_setting"].as_integer(), Some(7));
}

/// A key read under an old name is a known key, so a save writes it under its current name only.
#[tokio::test]
async fn a_key_read_under_an_old_name_is_saved_under_its_current_name() {
    let (_dir, path) = config_file(
        "use_global_store_automatically = true\n\n\
         [default_global_stores.example]\npath_to_store = \"/srv/shared\"\n",
    );

    let (config, lock) = GlobalConfig::load_locked_from_path(&path)
        .await
        .expect("the config must load");
    config
        .save_at_path(lock, &path)
        .await
        .expect("the config must save");

    let saved = saved_table(&path);
    assert!(saved.contains_key("default_shared_stores"));
    assert!(saved.contains_key("use_shared_store_automatically"));
    assert!(!saved.contains_key("default_global_stores"));
    assert!(!saved.contains_key("use_global_store_automatically"));
}

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
    config.auth.store = Some(AuthStore::Fallback);
    config.auth.path = Some("tokens".to_string());

    let written = toml::to_string(&config).expect("the config must be writable as TOML");
    let read: GlobalConfig = toml::from_str(&written).expect("and readable back");

    assert_eq!(read.service_executable(), Some("/opt/lore/1.9/bin/lore"));
    assert_eq!(
        read.service_idle_connection_timeout(),
        Duration::from_secs(42)
    );
    assert!(read.use_shared_store_automatically());
    assert_eq!(read.all_default_shared_stores().count(), 1);
    assert_eq!(read.auth.store, Some(AuthStore::Fallback));
    assert_eq!(read.auth.path.as_deref(), Some("tokens"));
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

/// Each store reads by its name, and any other value fails the parse rather than selecting the
/// secure store.
#[test]
fn the_auth_store_is_secure_or_fallback() {
    for (name, store) in [
        ("secure", AuthStore::Secure),
        ("fallback", AuthStore::Fallback),
    ] {
        let config: GlobalConfig =
            toml::from_str(&format!("[auth]\nstore = \"{name}\"\n")).expect("readable");
        assert_eq!(config.auth.store, Some(store), "{name}");
    }
    for name in ["file", "Fallback", "fallbak", ""] {
        assert!(
            toml::from_str::<GlobalConfig>(&format!("[auth]\nstore = \"{name}\"\n")).is_err(),
            "{name:?} must not parse"
        );
    }
}

/// A relative `[auth] path` is relative to the configuration file's directory, whatever the
/// current directory, and an absolute one is kept.
#[test]
#[allow(clippy::disallowed_methods)] // A test fixture in its own temporary directory.
fn a_relative_auth_path_resolves_against_the_config_directory() {
    let config_dir = TempDir::new("auth-path-");
    let file = config_dir.child("config.toml");
    std::fs::write(&file, "[auth]\npath = \"tokens\"\n").expect("the config must be writable");

    let settings = load_auth_settings_from(&file).expect("the config must load");
    assert_eq!(settings.path, Some(config_dir.child("tokens")));

    let absolute = config_dir.child("elsewhere");
    let config: GlobalConfig = toml::from_str(&format!(
        "[auth]\npath = {:?}\n",
        absolute.display().to_string()
    ))
    .expect("readable");
    assert_eq!(
        AuthSettings::from_config(config.auth, Path::new("/unrelated")).path,
        Some(absolute)
    );
}

/// A missing configuration file has no settings, and one that does not parse is an error.
#[test]
#[allow(clippy::disallowed_methods)] // A test fixture in its own temporary directory.
fn a_config_file_that_does_not_parse_is_reported() {
    let config_dir = TempDir::new("auth-parse-");
    let file = config_dir.child("config.toml");
    assert_eq!(
        load_auth_settings_from(&file).expect("a missing file must load"),
        AuthSettings::default()
    );

    std::fs::write(&file, "[auth]\nstore = \"file\"\n").expect("the config must be writable");
    assert!(load_auth_settings_from(&file).is_err());
}

/// Either store is carried in `LORE_AUTH_STORE`, so that a call relayed to the service carries
/// the choice, and the path in `LORE_AUTH_PATH`.
#[test]
fn both_stores_are_carried_in_the_environment() {
    for store in [AuthStore::Secure, AuthStore::Fallback] {
        let settings = AuthSettings {
            store: Some(store),
            path: Some("/tokens".into()),
        };
        assert_eq!(
            settings.variables(),
            vec![
                ("LORE_AUTH_STORE", OsString::from(store.as_var())),
                ("LORE_AUTH_PATH", OsString::from("/tokens")),
            ]
        );
    }
    assert!(AuthSettings::default().variables().is_empty());
}

/// Only the `[auth]` table is read for the auth settings: a value another table can't take
/// doesn't stop them, while a file that isn't TOML does.
#[test]
#[allow(clippy::disallowed_methods)] // A test fixture in its own temporary directory.
fn the_auth_settings_read_only_the_auth_table() {
    let config_dir = TempDir::new("auth-only-");
    let file = config_dir.child("config.toml");
    std::fs::write(
        &file,
        "[service]\nidle_connection_timeout = \"600\"\n\n[auth]\nstore = \"fallback\"\n",
    )
    .expect("the config must be writable");
    assert_eq!(
        load_auth_settings_from(&file)
            .expect("another table's value doesn't stop the auth settings")
            .store,
        Some(AuthStore::Fallback)
    );

    std::fs::write(&file, "[auth\n").expect("the config must be writable");
    assert!(load_auth_settings_from(&file).is_err());
}

/// `LORE_AUTH_STORE` names a store or is blank or unset; any other value is refused rather than
/// read as the secure store.
#[test]
fn the_auth_store_variable_names_a_store_or_nothing() {
    for value in [None, Some(""), Some("secure"), Some("fallback")] {
        assert!(check_auth_store_value(value).is_ok(), "{value:?}");
    }
    for value in ["file", "Fallback", "fallbak"] {
        assert!(check_auth_store_value(Some(value)).is_err(), "{value:?}");
    }
}

/// `[auth] store = "fallback"` needs a token store directory of its own, from `[auth] path` or
/// `LORE_AUTH_PATH`, unless `LORE_AUTH_STORE` makes the choice instead.
#[test]
fn a_fallback_store_needs_a_path_of_its_own() {
    let fallback = AuthSettings {
        store: Some(AuthStore::Fallback),
        path: None,
    };
    assert!(check_fallback_has_a_path(&fallback, None, None, None).is_err());
    assert!(check_fallback_has_a_path(&fallback, Some(""), Some(""), None).is_err());
    assert!(check_fallback_has_a_path(&fallback, None, Some("/tokens"), None).is_ok());
    assert!(check_fallback_has_a_path(&fallback, Some("secure"), None, None).is_ok());
    let with_path = AuthSettings {
        path: Some("/tokens".into()),
        ..fallback
    };
    assert!(check_fallback_has_a_path(&with_path, None, None, None).is_ok());
    assert!(
        check_fallback_has_a_path(&with_path, None, None, Some(Path::new("/tokens/."))).is_err(),
        "a path naming the default directory isn't one of its own"
    );
    assert!(check_fallback_has_a_path(&AuthSettings::default(), None, None, None).is_ok());
}
