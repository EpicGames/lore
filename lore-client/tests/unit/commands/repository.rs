// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore::repository::LoreVfsType;
use lore_client::commands::repository::VfsType;

#[test]
fn cli_vfs_fuse_maps_to_lore_fuse() {
    assert_eq!(VfsType::Fuse.to_lore(), LoreVfsType::Fuse);
}
