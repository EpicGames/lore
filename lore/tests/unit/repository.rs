// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::repository::VfsType;

#[cfg(any(target_os = "linux", target_family = "windows"))]
#[test]
fn lore_vfs_type_default_maps_to_fuse_on_linux_and_swfs_on_windows() {
    let config = lore::repository::LoreVfsType::Default.to_config();
    #[cfg(target_os = "linux")]
    assert!(
        config.vfs_type == VfsType::Fuse,
        "Default should map to Fuse on Linux"
    );
    #[cfg(target_family = "windows")]
    assert!(
        config.vfs_type == VfsType::Swfs,
        "Default should map to Swfs on Windows"
    );
}

#[test]
fn lore_vfs_type_fuse_maps_to_config_fuse() {
    let config = lore::repository::LoreVfsType::Fuse.to_config();
    assert_eq!(config.vfs_type, VfsType::Fuse);
}
