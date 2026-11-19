// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures write the setting's file directly; what these test is how it is read.
#![allow(clippy::disallowed_methods)]

use std::num::NonZeroU64;

use lore_base::test_util::TempDir;
use lore_revision::global::external_dir::setting::*;

fn setting(path: &str) -> Setting {
    Setting {
        path: path.to_owned(),
        id: RootId::generate(),
        generation: NonZeroU64::MIN,
    }
}

/// A saved setting reads back as it was written, and an absent one reads as none.
#[tokio::test]
async fn a_setting_round_trips_and_an_absent_one_is_none() {
    let global = TempDir::new("lore-external-setting-");
    assert_eq!(Setting::load(global.path()).expect("read"), None);

    let saved = setting("/mnt/work/lore-external");
    saved.save(global.path()).await.expect("save");

    assert_eq!(Setting::load(global.path()).expect("read"), Some(saved));
}

/// The file holds `path`, `id` as 32 lower-case hex digits, and `generation`, as TOML.
#[test]
fn a_setting_parses_from_its_toml() {
    let global = TempDir::new("lore-external-setting-toml-");
    std::fs::write(
        global.path().join(EXTERNAL_ROOT),
        "path = \"\"\nid = \"0123456789abcdef0123456789abcdef\"\ngeneration = 3\n",
    )
    .expect("write the setting");

    let setting = Setting::load(global.path())
        .expect("read")
        .expect("present");

    assert_eq!(setting.path, "");
    assert_eq!(setting.id.to_string(), "0123456789abcdef0123456789abcdef");
    assert_eq!(setting.generation.get(), 3);
}

/// An ID that isn't 32 lower-case hex digits, a generation of 0, and a missing field are errors,
/// not a setting.
#[test]
fn a_malformed_setting_is_an_error() {
    for text in [
        "path = \"\"\nid = \"0123456789ABCDEF0123456789ABCDEF\"\ngeneration = 1\n",
        "path = \"\"\nid = \"0123456789abcdef\"\ngeneration = 1\n",
        "path = \"\"\nid = \"0123456789abcdef0123456789abcdef\"\ngeneration = 0\n",
        "path = \"\"\nid = \"0123456789abcdef0123456789abcdef\"\n",
        "",
    ] {
        let global = TempDir::new("lore-external-setting-malformed-");
        std::fs::write(global.path().join(EXTERNAL_ROOT), text).expect("write the setting");

        assert!(Setting::load(global.path()).is_err(), "{text:?}");
    }
}

/// Two generated root IDs differ, and each reads back from its text.
#[test]
fn a_root_id_is_random_and_reads_back_from_its_text() {
    let (first, second) = (RootId::generate(), RootId::generate());

    assert_ne!(first, second);
    assert_eq!(first.to_string().parse::<RootId>(), Ok(first));
}

/// A root ID aligns no wider than a pointer, so the futures that hold a setting or a marker,
/// among them every command's dispatch, keep the alignment they have without one.
#[test]
fn a_root_id_aligns_no_wider_than_a_pointer() {
    use lore_revision::global::external_dir::marker::Marker;
    use lore_revision::global::external_dir::setting::RootId;
    use lore_revision::global::external_dir::setting::Setting;

    assert!(align_of::<RootId>() <= align_of::<usize>());
    assert!(align_of::<Setting>() <= align_of::<usize>());
    assert!(align_of::<Marker>() <= align_of::<usize>());
}
