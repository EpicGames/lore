// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures write the leftover list directly; what these test is how it is read.
#![allow(clippy::disallowed_methods)]

use lore_base::test_util::TempDir;
use lore_revision::global::external_dir::leftovers::*;

/// A saved list reads back as it was written, and an absent one reads as empty.
#[tokio::test]
async fn a_list_round_trips_and_an_absent_one_is_empty() {
    let global = TempDir::new("lore-external-leftovers-");
    assert_eq!(
        Leftovers::load(global.path()).expect("read"),
        Leftovers::default()
    );

    let saved = Leftovers {
        paths: vec!["external.aside-1a2b".to_owned(), "/mnt/old/root".to_owned()],
        marked: vec!["/mnt/gone/root".to_owned()],
    };
    saved.save(global.path()).await.expect("save");

    assert_eq!(Leftovers::load(global.path()).expect("read"), saved);
}

/// The list holds `paths` and `marked` as arrays of strings, either of which can be left out.
#[test]
fn a_list_parses_from_its_toml() {
    let global = TempDir::new("lore-external-leftovers-toml-");
    std::fs::write(global.path().join(LEFTOVERS), "marked = [\"/mnt/gone\"]\n")
        .expect("write the list");

    let list = Leftovers::load(global.path()).expect("read");

    assert!(list.paths.is_empty());
    assert_eq!(list.marked, ["/mnt/gone"]);
}
