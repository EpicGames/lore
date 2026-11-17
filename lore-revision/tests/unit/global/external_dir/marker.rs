// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures write the marker directly; what these test is how it is read.
#![allow(clippy::disallowed_methods)]

use std::num::NonZeroU64;

use lore_base::test_util::TempDir;
use lore_revision::global::external_dir::marker::*;
use lore_revision::global::external_dir::setting::RootId;

/// A saved marker reads back as it was written, and an absent one reads as none.
#[tokio::test]
async fn a_marker_round_trips_and_an_absent_one_is_none() {
    let root = TempDir::new("lore-external-marker-");
    assert_eq!(Marker::load(root.path()).expect("read"), None);

    let saved = Marker {
        id: RootId::generate(),
        generation: NonZeroU64::new(2).expect("non-zero"),
    };
    saved.save(root.path()).await.expect("save");

    assert_eq!(Marker::load(root.path()).expect("read"), Some(saved));
}

/// The marker holds `id` and `generation` as TOML, and refuses a generation of 0.
#[test]
fn a_marker_parses_from_its_toml() {
    let root = TempDir::new("lore-external-marker-toml-");
    let path = root.path().join(MARKER);
    std::fs::write(
        &path,
        "id = \"fedcba9876543210fedcba9876543210\"\ngeneration = 1\n",
    )
    .expect("write the marker");

    let marker = Marker::load(root.path()).expect("read").expect("present");
    assert_eq!(marker.id.to_string(), "fedcba9876543210fedcba9876543210");
    assert_eq!(marker.generation.get(), 1);

    std::fs::write(
        &path,
        "id = \"fedcba9876543210fedcba9876543210\"\ngeneration = 0\n",
    )
    .expect("write the marker");
    assert!(Marker::load(root.path()).is_err());
}
