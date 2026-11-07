// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Scans the handler modules for any `send_error` call on a terminal arm,
// so a regression that re-emits a mid-stream `Error` event fails the build.

use lore_revision::repository::VfsType;

const MIGRATED_SOURCES: &[(&str, &str)] = &[
    ("repository.rs", include_str!("../../src/repository.rs")),
    ("auth.rs", include_str!("../../src/auth.rs")),
];

#[test]
fn migrated_terminal_arms_have_no_send_error_call() {
    // Build the needle from parts so this scanning test does not match its
    // own source when it scans `repository.rs`.
    let needle = format!(".{}(", "send_error");
    for (name, source) in MIGRATED_SOURCES {
        assert!(
            !source.contains(&needle),
            "{name} still calls the dispatcher error sink on a terminal arm; \
                 the migrated handler must route the error through `complete` \
                 instead of emitting a mid-stream `Error` event"
        );
    }
}

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
