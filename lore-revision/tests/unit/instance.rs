// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures write external `.lore` configs directly; what these test is how staleness reads them.
#![allow(clippy::disallowed_methods)]

use lore_revision::instance::*;
use lore_revision::repository::SALT_LORE;
use lore_storage::store_types::KeyType;
use zerocopy::IntoBytes;

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

fn external_dot_lore(config: Option<&[u8]>) -> lore_base::test_util::TempDir {
    let dir = lore_base::test_util::TempDir::new("lore-instance-external-");
    if let Some(config) = config {
        std::fs::write(dir.path().join(lore_revision::repository::CONFIG), config)
            .expect("write config");
    }
    dir
}

#[tokio::test]
async fn an_swfs_config_names_swfs() {
    let dir = external_dot_lore(Some(SWFS_CONFIG));
    assert!(external_dot_lore_names_swfs(dir.path()).await);
}

#[tokio::test]
async fn a_config_without_swfs_does_not_name_swfs() {
    for config in [
        &b"[vfs]\nvfs_type = \"None\"\n"[..],
        b"remote_url = \"lore://host\"\n",
    ] {
        let dir = external_dot_lore(Some(config));
        assert!(
            !external_dot_lore_names_swfs(dir.path()).await,
            "{}",
            String::from_utf8_lossy(config)
        );
    }
}

#[tokio::test]
async fn a_missing_config_does_not_name_swfs() {
    let dir = external_dot_lore(None);
    assert!(!external_dot_lore_names_swfs(dir.path()).await);
    assert!(
        !external_dot_lore_names_swfs(&dir.path().join("absent")).await,
        "a missing external directory holds no config"
    );
}

/// Only positive evidence makes a registration stale, so a config that is
/// there but cannot be understood counts as an SWFS instance.
#[tokio::test]
async fn an_unparseable_config_names_swfs() {
    let dir = external_dot_lore(Some(b"this is not toml = = ="));
    assert!(external_dot_lore_names_swfs(dir.path()).await);
}

#[tokio::test]
async fn an_unreadable_config_names_swfs() {
    let dir = external_dot_lore(None);
    std::fs::create_dir(dir.path().join(lore_revision::repository::CONFIG))
        .expect("occupy the config path");
    assert!(external_dot_lore_names_swfs(dir.path()).await);
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
