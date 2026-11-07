// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

mod clone;
mod clone_hold;
mod dry_run_store_tests;
mod name_validation_tests;
mod path_optional_tests;
mod prefetch;
mod remote_state_tests;
mod repository_lock_tests;
mod root_text_tests;
mod status;
mod store_config_tests;
pub(crate) mod test_helpers;
mod write_token_tests;

use lore_revision::repository::RepositoryConfig;
use lore_revision::repository::VfsConfig;
use lore_revision::repository::VfsType;

#[test]
fn vfs_type_fuse_roundtrips_serde() {
    let json = serde_json::to_string(&VfsType::Fuse).expect("VfsType::Fuse serializes");
    let restored: VfsType =
        serde_json::from_str(&json).expect("VfsType::Fuse round-trips through serde");
    assert_eq!(restored, VfsType::Fuse);
}

#[test]
fn vfs_config_base_time_is_optional_and_absent_by_default() {
    let config = VfsConfig {
        vfs_type: VfsType::Fuse,
        base_time: None,
    };
    let json = serde_json::to_string(&config).expect("VfsConfig serializes");
    assert!(
        !json.contains("base_time"),
        "absent base_time must be skipped, got: {json}"
    );
    let restored: VfsConfig =
        serde_json::from_str(&json).expect("VfsConfig without base_time deserializes");
    assert!(restored.base_time.is_none());
}

#[test]
fn repository_config_names_its_vfs_type() {
    let config = RepositoryConfig {
        vfs: Some(VfsConfig {
            vfs_type: VfsType::Fuse,
            base_time: None,
        }),
        ..Default::default()
    };
    assert_eq!(config.vfs_type(), VfsType::Fuse);
    assert!(config.vfs_type().is_virtual());
    assert_eq!(
        RepositoryConfig::default().vfs_type(),
        VfsType::None,
        "a config without a [vfs] section names no VFS"
    );
}
