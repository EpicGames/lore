// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The external root's files and its resolution.

mod access;
mod at_start;
mod check;
mod copying;
mod crash;
mod leftovers;
mod locks;
mod marker;
pub(crate) mod moving;
mod record;
mod recovery;
mod redirect;
mod resolve;
mod setting;
pub(crate) mod takeover;

/// An instance's external directory is made in a root a resolution finds available. With no root
/// ID, the first one makes the missing default location. A configured root that is missing makes
/// the creation fail, and neither it, nor its parent, nor anything at the default location is made.
#[tokio::test]
async fn an_external_directory_is_made_only_in_an_available_root() {
    use std::num::NonZeroU64;

    use lore_base::test_util::TempDir;
    use lore_revision::global::external_dir::create_external_dir;
    use lore_revision::global::external_dir::resolve::resolve_in;
    use lore_revision::global::external_dir::setting::RootId;
    use lore_revision::global::external_dir::setting::Setting;
    use lore_revision::instance::InstanceId;

    let global = TempDir::new("lore-external-create-");
    let instance_id = InstanceId::generate();
    let made = create_external_dir(&resolve_in(global.path().to_path_buf()), instance_id)
        .expect("the default root is made with the first instance");
    assert_eq!(
        made,
        global
            .path()
            .join("external")
            .join(instance_id.text_encoding())
    );
    assert!(made.is_dir());

    let configured = TempDir::new("lore-external-create-configured-");
    let volume = configured.path().join("volume");
    Setting {
        path: volume.join("root").display().to_string(),
        id: RootId::generate(),
        generation: NonZeroU64::MIN,
    }
    .save(configured.path())
    .await
    .expect("the setting is written");

    let refused = create_external_dir(&resolve_in(configured.path().to_path_buf()), instance_id);

    assert!(refused.is_err());
    assert!(!volume.exists(), "the root and its parent aren't made");
    assert!(
        !configured.path().join("external").exists(),
        "nothing is made at the default location"
    );
}

/// A status reports the root a resolution finds available, else the path the setting names, else
/// the default location, and an empty path where the setting can't be read.
#[tokio::test]
#[allow(clippy::disallowed_methods)] // A fixture in a global data directory of its own.
async fn a_status_reports_the_available_root_else_the_one_the_setting_names() {
    use std::num::NonZeroU64;

    use lore_base::test_util::TempDir;
    use lore_revision::global::external_dir::reported_root;
    use lore_revision::global::external_dir::resolve::resolve_in;
    use lore_revision::global::external_dir::setting::RootId;
    use lore_revision::global::external_dir::setting::Setting;

    let global = TempDir::new("lore-external-reported-");
    let default = global.path().join("external");
    assert_eq!(
        reported_root(&resolve_in(global.path().to_path_buf())),
        default,
        "with no setting, the default location"
    );

    let missing = global.path().join("missing");
    let mut setting = Setting {
        path: missing.display().to_string(),
        id: RootId::generate(),
        generation: NonZeroU64::MIN,
    };
    setting
        .save(global.path())
        .await
        .expect("write the setting");
    assert_eq!(
        reported_root(&resolve_in(global.path().to_path_buf())),
        missing
    );

    setting.path = String::new();
    setting
        .save(global.path())
        .await
        .expect("write the setting");
    assert_eq!(
        reported_root(&resolve_in(global.path().to_path_buf())),
        default,
        "an empty path names the default location"
    );

    std::fs::write(global.path().join("external-root"), b"not toml = [").expect("corrupt it");
    assert_eq!(
        reported_root(&resolve_in(global.path().to_path_buf())),
        std::path::PathBuf::new()
    );
}

/// Places a redirect at `link` to `target`: a symbolic link on Unix, a junction on Windows.
pub(crate) fn redirect(link: &std::path::Path, target: &std::path::Path) {
    #[cfg(target_family = "unix")]
    std::os::unix::fs::symlink(target, link).expect("make the link");
    #[cfg(target_family = "windows")]
    {
        let created = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .expect("run mklink");
        assert!(created.status.success(), "mklink /J failed: {created:?}");
    }
}
