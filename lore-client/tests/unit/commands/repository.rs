// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore::interface::LoreSharedStoreMode;
use lore::repository::LoreVfsType;
use lore_client::commands::repository::VfsType;
use lore_client::commands::repository::shared_store_mode;

#[test]
fn cli_vfs_fuse_maps_to_lore_fuse() {
    assert_eq!(VfsType::Fuse.to_lore(), LoreVfsType::Fuse);
}

/// `--shared-store-path` places a shared store, so it needs one: `--use-shared-store`, or a
/// `--vfs` that makes a virtual file system. With neither, an explicit `--vfs none` included, the
/// path would be ignored, so it is refused as clap refuses a missing argument.
#[test]
fn a_shared_store_path_needs_a_shared_store() {
    let refused = shared_store_mode(false, VfsType::None, true)
        .expect_err("a path with no shared store to place is refused");
    assert_eq!(
        refused.kind(),
        clap::error::ErrorKind::MissingRequiredArgument
    );
    assert_eq!(refused.exit_code(), 2);

    let accepted = [
        (true, VfsType::None, LoreSharedStoreMode::Enabled),
        (false, VfsType::Fuse, LoreSharedStoreMode::Inherit),
        (false, VfsType::Swfs, LoreSharedStoreMode::Inherit),
    ];
    for (use_shared_store, vfs, mode) in accepted {
        let given = shared_store_mode(use_shared_store, vfs, true)
            .expect("a path with a shared store to place is accepted");
        assert_eq!(
            given,
            mode,
            "--use-shared-store {use_shared_store}, {:?}",
            vfs.to_lore()
        );
    }
    assert_eq!(
        shared_store_mode(false, VfsType::None, false).expect("no path, nothing to place"),
        LoreSharedStoreMode::Inherit
    );
}
