// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::num::NonZeroU64;

use clap::Parser;
use lore::interface::LoreSharedStoreMode;
use lore::repository::LoreVfsType;
use lore_client::cli::LoreCli;
use lore_client::cli::LoreCommands;
use lore_client::commands::repository::RepositoryCloneArgs;
use lore_client::commands::repository::VfsType;
use lore_client::commands::repository::shared_store_mode;

/// The clone arguments `args` parse to.
fn clone_args_of(args: &[&str]) -> RepositoryCloneArgs {
    match crate::client_main::command_of(args) {
        LoreCommands::Clone(clone) => clone,
        _ => panic!("{} is a clone", args.join(" ")),
    }
}

/// `--max-capacity` and `--max-size` reach the clone's arguments, each is absent without its flag,
/// and 0 is refused, as `shared-store create` refuses it.
#[test]
fn clone_parses_the_store_limits() {
    let given = clone_args_of(&[
        "lore",
        "clone",
        "lores://host/repo",
        "--max-capacity",
        "50000000",
        "--max-size",
        "1099511627776",
    ]);
    assert_eq!(given.max_capacity.map(NonZeroU64::get), Some(50_000_000));
    assert_eq!(given.max_size.map(NonZeroU64::get), Some(1 << 40));

    let absent = clone_args_of(&["lore", "clone", "lores://host/repo"]);
    assert_eq!(absent.max_capacity, None);
    assert_eq!(absent.max_size, None);

    for flag in ["--max-capacity", "--max-size"] {
        assert!(
            LoreCli::try_parse_from(["lore", "clone", "lores://host/repo", flag, "0"]).is_err(),
            "{flag} 0 is refused"
        );
    }
}

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
