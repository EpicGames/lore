// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures write external `.lore` configs directly; what these test is how staleness reads them.
#![allow(clippy::disallowed_methods)]

use std::path::Path;
use std::sync::Arc;

use lore_base::env::CallEnvironment;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_revision::global::external_dir::external_lore_dir;
use lore_revision::instance::*;
use lore_revision::interface::ExecutionContext;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository::CONFIG;
use lore_revision::repository::SALT_LORE;
use lore_storage::store_types::KeyType;
use zerocopy::IntoBytes;

#[test]
fn from_hex_roundtrips_text_encoding() {
    let id = InstanceId::generate();
    let hex = id.text_encoding();
    assert_eq!(InstanceId::from_hex(&hex), Some(id));
    assert_eq!(InstanceId::from_hex("not hex"), None);
    assert_eq!(
        InstanceId::from_hex("ab"),
        None,
        "too short to be an InstanceId"
    );
}

#[test]
fn instance_key_is_deterministic() {
    let id = InstanceId::generate();
    let (key1, typ1) = instance_key(SALT_LORE, id);
    let (key2, typ2) = instance_key(SALT_LORE, id);
    assert_eq!(key1, key2);
    assert_eq!(typ1, typ2);
    assert_eq!(typ1, KeyType::Instance);
}

#[test]
fn instance_key_differs_for_different_ids() {
    let a = InstanceId::generate();
    let b = InstanceId::generate();
    let (key_a, _) = instance_key(SALT_LORE, a);
    let (key_b, _) = instance_key(SALT_LORE, b);
    assert_ne!(key_a, key_b);
}

#[test]
fn anchor_keys_differ_for_current_vs_staged() {
    let id = InstanceId::generate();
    let (current, typ_c) = anchor_key(SALT_LORE, ANCHOR_CURRENT, id);
    let (staged, typ_s) = anchor_key(SALT_LORE, ANCHOR_STAGED, id);
    assert_ne!(current, staged);
    assert_eq!(typ_c, KeyType::Untyped);
    assert_eq!(typ_s, KeyType::Untyped);
}

#[test]
fn generate_produces_nonzero_unique_values() {
    let a = InstanceId::generate();
    let b = InstanceId::generate();
    assert!(!a.is_zero());
    assert!(!b.is_zero());
    assert_ne!(a, b);
}

#[test]
fn default_is_zero() {
    let id = InstanceId::default();
    assert!(id.is_zero());
}

#[test]
fn roundtrip_bytes() {
    let id = InstanceId::generate();
    let bytes = id.as_bytes().to_vec();
    assert_eq!(bytes.len(), 16);
    let mut restored = InstanceId::default();
    restored.as_mut_bytes().copy_from_slice(&bytes);
    assert_eq!(id, restored);
}

/// The minimal config of an SWFS-backed instance.
const SWFS_CONFIG: &[u8] = b"[vfs]\nvfs_type = \"Swfs\"\n";

/// The minimal config of a FUSE-backed instance.
const FUSE_CONFIG: &[u8] = b"[vfs]\nvfs_type = \"Fuse\"\n";

/// What an instance's external `.lore` directory holds.
#[derive(Clone, Copy, Debug)]
enum External {
    /// There is no external directory.
    Absent,
    /// The directory, with no config in it.
    NoConfig,
    /// The directory, with a config of these bytes.
    Config(&'static [u8]),
    /// The directory, with a directory where its config would be, so the config cannot be read.
    Unreadable,
}

/// A call whose global data directory is `global`, so no test touches the real one.
fn execution_in(global: &Path) -> Arc<ExecutionContext> {
    let environment = CallEnvironment {
        values: [Some(global.display().to_string()), None],
    };
    Arc::new(
        ExecutionContext::new_client_with_user_id(
            Default::default(),
            EventDispatcher::no_dispatch(),
            "test-user".to_string(),
        )
        .with_environment(environment),
    )
}

/// The staleness of the registration of a fresh instance whose external `.lore` directory holds
/// `external`, at a path that is gone where `path_gone`, else an empty directory: a checkout with
/// no instance file.
async fn staleness_with(external: External, path_gone: bool) -> InstanceStaleness {
    let global = TempDir::new("lore-instance-staleness-global-");
    let parent = TempDir::new("lore-instance-staleness-path-");
    let path = parent.path().join("instance");
    if !path_gone {
        std::fs::create_dir(&path).expect("make the instance path");
    }
    LORE_CONTEXT
        .scope(execution_in(global.path()), async {
            let instance_id = InstanceId::generate();
            let dot_lore = external_lore_dir(instance_id).expect("global data directory");
            match external {
                External::Absent => {}
                External::NoConfig => {
                    std::fs::create_dir_all(&dot_lore).expect("make the external directory");
                }
                External::Config(config) => {
                    std::fs::create_dir_all(&dot_lore).expect("make the external directory");
                    std::fs::write(dot_lore.join(CONFIG), config).expect("write the config");
                }
                External::Unreadable => {
                    std::fs::create_dir_all(dot_lore.join(CONFIG)).expect("occupy the config path");
                }
            }
            instance_staleness(&path.display().to_string(), instance_id).await
        })
        .await
}

/// Only positive evidence makes a registration stale. An external `.lore` naming SWFS keeps one
/// whose path or checkout is gone, as an SWFS instance cannot be removed by hand. One naming FUSE
/// keeps one whose checkout is gone, as an unmounted mount point is empty, but not one whose
/// mount point is gone.
#[tokio::test]
async fn an_external_config_naming_swfs_or_fuse_keeps_what_its_path_does_not() {
    for (config, path_gone, checkout_gone) in [
        (
            SWFS_CONFIG,
            InstanceStaleness::Active,
            InstanceStaleness::Active,
        ),
        (
            FUSE_CONFIG,
            InstanceStaleness::PathMissing,
            InstanceStaleness::Active,
        ),
    ] {
        let label = String::from_utf8_lossy(config);
        assert_eq!(
            staleness_with(External::Config(config), true).await,
            path_gone,
            "path gone, {label}"
        );
        assert_eq!(
            staleness_with(External::Config(config), false).await,
            checkout_gone,
            "checkout gone, {label}"
        );
    }
}

/// An external `.lore` whose config is there but cannot be read or parsed is no evidence the
/// instance is gone, whichever VFS it was, so it keeps the registration.
#[tokio::test]
async fn an_external_config_that_cannot_be_read_keeps_the_registration() {
    for external in [
        External::Config(b"this is not toml = = ="),
        External::Unreadable,
    ] {
        for path_gone in [true, false] {
            assert_eq!(
                staleness_with(external, path_gone).await,
                InstanceStaleness::Active,
                "{external:?}, path gone {path_gone}"
            );
        }
    }
}

/// An external `.lore` that names no VFS, holds no config or is not there is no evidence either
/// way, and leaves the registration as its path says.
#[tokio::test]
async fn an_external_directory_naming_no_vfs_leaves_the_registration_as_its_path_says() {
    for external in [
        External::Config(b"[vfs]\nvfs_type = \"None\"\n"),
        External::Config(b"remote_url = \"lore://host\"\n"),
        External::NoConfig,
        External::Absent,
    ] {
        assert_eq!(
            staleness_with(external, true).await,
            InstanceStaleness::PathMissing,
            "{external:?}"
        );
        assert_eq!(
            staleness_with(external, false).await,
            InstanceStaleness::CheckoutMissing,
            "{external:?}"
        );
    }
}

#[test]
fn display_is_hex() {
    let id = InstanceId::generate();
    let s = id.to_string();
    assert_eq!(s.len(), 32); // 16 bytes = 32 hex chars
    assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
}

const PATH: &str = "/checkout";

#[test]
fn an_swfs_instance_on_its_own_mount_can_be_acted_on() {
    let id = InstanceId::generate();
    check_mount(id, true, true, Some(id), PATH, "delete").expect("its own mount");
}

#[test]
fn a_regular_instance_on_no_mount_can_be_acted_on() {
    let id = InstanceId::generate();
    check_mount(id, false, true, None, PATH, "delete").expect("in the service");
    check_mount(id, false, false, None, PATH, "delete").expect("outside it");
}

#[test]
fn an_instance_file_naming_another_instance_than_the_mount_is_refused() {
    let named = InstanceId::generate();
    let mounted = InstanceId::generate();
    for is_swfs in [true, false] {
        let error = check_mount(named, is_swfs, true, Some(mounted), PATH, "move")
            .expect_err("another instance's mount")
            .to_string();
        assert!(error.contains(&mounted.to_string()), "{error}");
        assert!(error.contains(&named.to_string()), "{error}");
    }
}

#[test]
fn an_swfs_instance_that_is_not_mounted_at_its_path_is_refused() {
    let id = InstanceId::generate();
    let error = check_mount(id, true, true, None, PATH, "delete")
        .expect_err("not mounted")
        .to_string();
    assert!(error.contains("not mounted"), "{error}");
}

#[test]
fn an_swfs_instance_outside_the_service_is_refused() {
    let id = InstanceId::generate();
    let error = check_mount(id, true, false, None, PATH, "move")
        .expect_err("no service")
        .to_string();
    assert!(error.contains("only the service"), "{error}");
}

#[test]
fn a_mount_of_an_instance_with_no_external_directory_is_refused() {
    let id = InstanceId::generate();
    let error = check_mount(id, false, true, Some(id), PATH, "delete")
        .expect_err("a mount it cannot unmount")
        .to_string();
    assert!(error.contains("no external directory"), "{error}");
}
