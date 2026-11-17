// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The external root's files and its resolution.

mod leftovers;
mod marker;
mod record;
pub(crate) mod resolve;
mod setting;

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
