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

use lore_base::test_util::TempDir;
use lore_revision::repository::CONFIG;
use lore_revision::repository::DOT_LORE;
use lore_revision::repository::RepositoryConfig;
use lore_revision::repository::VfsConfig;
use lore_revision::repository::VfsType;
use lore_revision::repository::force_may_replace;

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

/// `--force` replaces an ordinary repository in the path, and one whose config is missing or
/// unreadable there, but never a virtual clone: neither one whose config names a VFS, nor one
/// whose Lore directory lies outside the path, whatever its config holds.
#[test]
#[allow(clippy::disallowed_methods)]
fn force_replaces_only_an_ordinary_repository() {
    const ORDINARY: &[u8] = b"[vfs]\nvfs_type = \"None\"\n";
    const UNREADABLE: &[u8] = b"[vfs\n";
    let cases: [(&str, bool, Option<&[u8]>, bool); 8] = [
        ("no config", true, None, true),
        ("an ordinary repository", true, Some(ORDINARY), true),
        ("an unreadable config", true, Some(UNREADABLE), true),
        (
            "a FUSE config",
            true,
            Some(b"[vfs]\nvfs_type = \"Fuse\"\n"),
            false,
        ),
        (
            "an SWFS config",
            true,
            Some(b"[vfs]\nvfs_type = \"Swfs\"\n"),
            false,
        ),
        ("no config outside the path", false, None, false),
        (
            "an ordinary config outside the path",
            false,
            Some(ORDINARY),
            false,
        ),
        (
            "an unreadable config outside the path",
            false,
            Some(UNREADABLE),
            false,
        ),
    ];
    for (name, inside, config, replaceable) in cases {
        let path = TempDir::new("force-replaces-only-an-ordinary-repository-");
        let external = TempDir::new("force-replaces-only-an-ordinary-repository-external-");
        let dot_lore = if inside { &path } else { &external }.child(DOT_LORE);
        std::fs::create_dir(&dot_lore).expect("making the Lore directory");
        if let Some(config) = config {
            std::fs::write(dot_lore.join(CONFIG), config).expect("writing the config");
        }
        assert_eq!(
            force_may_replace(path.path(), &dot_lore),
            replaceable,
            "{name}"
        );
    }
}

/// A virtual VFS always uses a shared store: one that inherits the machine's setting gets one,
/// and one that disables it is refused. Without a VFS the mode is kept as given.
#[test]
fn a_virtual_vfs_always_uses_a_shared_store() {
    use lore_revision::repository::LoreSharedStoreMode;

    for vfs in [VfsType::Fuse, VfsType::Swfs] {
        for mode in [LoreSharedStoreMode::Inherit, LoreSharedStoreMode::Enabled] {
            assert_eq!(
                mode.for_vfs(vfs).expect("a shared store is allowed"),
                LoreSharedStoreMode::Enabled,
                "{mode:?} with {vfs}"
            );
        }
        let refusal = LoreSharedStoreMode::Disabled
            .for_vfs(vfs)
            .expect_err("a virtual clone without a shared store is refused");
        assert!(
            refusal.reason.contains("always uses a shared store"),
            "{}",
            refusal.reason
        );
    }
    for mode in [
        LoreSharedStoreMode::Inherit,
        LoreSharedStoreMode::Enabled,
        LoreSharedStoreMode::Disabled,
    ] {
        assert_eq!(
            mode.for_vfs(VfsType::None).expect("no VFS keeps the mode"),
            mode
        );
    }
}

/// A config with a virtual VFS and no shared store fails to validate, as the instance could not
/// be found or served.
#[test]
fn a_virtual_vfs_without_a_shared_store_fails_to_validate() {
    use lore_revision::repository::SharedStoreToUseConfig;

    for vfs_type in [VfsType::Fuse, VfsType::Swfs] {
        let vfs = Some(VfsConfig {
            vfs_type,
            base_time: None,
        });
        let without = RepositoryConfig {
            vfs: vfs.clone(),
            ..Default::default()
        };
        assert!(
            without.validate().is_err(),
            "{vfs_type} without a shared store"
        );
        let with = RepositoryConfig {
            vfs,
            shared_store_to_use: Some(SharedStoreToUseConfig {
                use_shared_store: Some(true),
                shared_store_path: None,
            }),
            ..Default::default()
        };
        assert!(with.validate().is_ok(), "{vfs_type} with a shared store");
    }
    assert!(RepositoryConfig::default().validate().is_ok());
}
