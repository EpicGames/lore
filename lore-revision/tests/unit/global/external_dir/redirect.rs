// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures plant what the default location holds directly; what these test is how the redirect
// step treats it.
#![allow(clippy::disallowed_methods)]

use std::num::NonZeroU64;
use std::path::Path;
use std::path::PathBuf;

use lore_base::test_util::TempDir;
use lore_revision::global::external_dir::record::MoveKind;
use lore_revision::global::external_dir::record::MovePhase;
use lore_revision::global::external_dir::record::MoveRecord;
use lore_revision::global::external_dir::redirect::*;
use lore_revision::global::external_dir::setting::RootId;
use lore_revision::global::external_dir::setting::Setting;

/// A global data directory, and a directory beside it for configured roots.
struct Fixture {
    global: TempDir,
    volume: TempDir,
}

impl Fixture {
    fn new() -> Fixture {
        Fixture {
            global: TempDir::new("lore-external-redirect-global-"),
            volume: TempDir::new("lore-external-redirect-volume-"),
        }
    }

    fn default_location(&self) -> PathBuf {
        self.global.path().join("external")
    }

    /// A root directory named `name` beside the global data directory, created.
    fn root(&self, name: &str) -> PathBuf {
        let root = self.volume.path().join(name);
        std::fs::create_dir_all(&root).expect("create the root");
        root
    }

    /// What the global data directory holds, by name, sorted.
    fn global_entries(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.global.path())
            .expect("list the global data directory")
            .map(|entry| {
                entry
                    .expect("read an entry")
                    .file_name()
                    .into_string()
                    .expect("a UTF-8 name")
            })
            .collect();
        names.sort();
        names
    }
}

/// A setting that names `root`.
fn setting(root: &Path) -> Setting {
    Setting {
        path: root.to_str().expect("a UTF-8 root").to_owned(),
        id: RootId::generate(),
        generation: NonZeroU64::MIN,
    }
}

/// A move record from `from` to `to`, empty for the default location.
fn move_record(from: &str, to: &str) -> MoveRecord {
    MoveRecord {
        from: from.to_owned(),
        to: to.to_owned(),
        kind: MoveKind::Rename,
        phase: MovePhase::Redirect,
    }
}

/// The redirect's target, read as a link or a junction.
fn target_of(path: &Path) -> PathBuf {
    assert!(
        std::fs::symlink_metadata(path)
            .expect("stat the redirect")
            .file_type()
            .is_symlink(),
        "{} is no redirect",
        path.display()
    );
    std::fs::read_link(path).expect("read the redirect")
}

/// A free default location takes the redirect, and nothing else is left in the global data
/// directory.
#[tokio::test]
async fn a_free_path_takes_the_redirect() {
    let fixture = Fixture::new();
    let root = fixture.root("root");

    let placed = redirect_to(
        fixture.global.path(),
        Some(&root),
        Some(&setting(&root)),
        None,
    )
    .await
    .expect("make the redirect");

    assert_eq!(placed, Redirect::Matched);
    assert_eq!(target_of(&fixture.default_location()), root);
    assert_eq!(fixture.global_entries(), ["external"]);
}

/// A redirect to the root the setting names is replaced with one to the new root: a link on Unix,
/// a junction on Windows. A redirect left at the temporary name is replaced as well.
#[tokio::test]
async fn a_redirect_the_command_made_is_replaced() {
    let fixture = Fixture::new();
    let (old, new) = (fixture.root("old"), fixture.root("new"));
    let old_setting = setting(&old);
    redirect_to(fixture.global.path(), Some(&old), Some(&old_setting), None)
        .await
        .expect("make the first redirect");
    super::redirect(&fixture.global.path().join(TEMPORARY_REDIRECT), &old);

    let placed = redirect_to(fixture.global.path(), Some(&new), Some(&old_setting), None)
        .await
        .expect("replace the redirect");

    assert_eq!(placed, Redirect::Matched);
    assert_eq!(target_of(&fixture.default_location()), new);
    assert!(old.is_dir(), "the old root stays where it is");
    assert_eq!(fixture.global_entries(), ["external"]);
}

/// A redirect to either root of the move record is the command's, and one the record makes the
/// path match is replaced.
#[tokio::test]
async fn a_redirect_to_a_root_of_the_record_is_replaced() {
    let fixture = Fixture::new();
    let (old, new) = (fixture.root("old"), fixture.root("new"));
    super::redirect(&fixture.default_location(), &old);
    let record = move_record(old.to_str().expect("UTF-8"), new.to_str().expect("UTF-8"));

    let placed = redirect_to(fixture.global.path(), Some(&new), None, Some(&record))
        .await
        .expect("replace the redirect");

    assert_eq!(placed, Redirect::Matched);
    assert_eq!(target_of(&fixture.default_location()), new);
}

/// Only a target that the setting or the move record names is a redirect the command made, and the
/// default location, which both write as an empty path, names no target.
#[test]
fn a_redirect_the_command_made_is_told_apart() {
    let fixture = Fixture::new();
    let (named, from, to, other) = (
        fixture.root("named"),
        fixture.root("from"),
        fixture.root("to"),
        fixture.root("other"),
    );
    let setting = setting(&named);
    let record = move_record(from.to_str().expect("UTF-8"), to.to_str().expect("UTF-8"));

    for made in [&named, &from, &to] {
        assert!(made_by_command(made, Some(&setting), Some(&record)));
    }
    assert!(made_by_command(&named, Some(&setting), None));
    assert!(made_by_command(&to, None, Some(&record)));
    assert!(!made_by_command(&other, Some(&setting), Some(&record)));
    assert!(!made_by_command(&named, None, Some(&record)));
    assert!(!made_by_command(&from, Some(&setting), None));
    assert!(!made_by_command(
        Path::new(""),
        Some(&setting),
        Some(&record)
    ));

    let default = Setting {
        path: String::new(),
        ..setting.clone()
    };
    assert!(!made_by_command(
        Path::new(""),
        Some(&default),
        Some(&move_record("", ""))
    ));
}

/// A redirect the command didn't make is kept and reported, with its target.
#[tokio::test]
async fn a_redirect_the_command_did_not_make_is_kept_and_reported() {
    let fixture = Fixture::new();
    let (other, root) = (fixture.root("other"), fixture.root("root"));
    super::redirect(&fixture.default_location(), &other);

    for wanted in [Some(root.as_path()), None] {
        let kept = redirect_to(fixture.global.path(), wanted, Some(&setting(&root)), None)
            .await
            .expect("examine the default location");

        assert_eq!(kept, Redirect::Kept(Occupant::OtherRedirect(other.clone())));
        assert_eq!(target_of(&fixture.default_location()), other);
    }
}

/// A directory with entries at the default location is kept and reported, with its entries.
#[tokio::test]
async fn a_directory_with_entries_is_kept_and_reported() {
    let fixture = Fixture::new();
    let root = fixture.root("root");
    let entry = fixture.default_location().join("0123456789abcdef");
    std::fs::create_dir_all(&entry).expect("plant an external directory");

    for wanted in [Some(root.as_path()), None] {
        let kept = redirect_to(fixture.global.path(), wanted, Some(&setting(&root)), None)
            .await
            .expect("examine the default location");

        assert_eq!(kept, Redirect::Kept(Occupant::Entries));
        assert!(entry.is_dir(), "the entry stays");
    }
    assert_eq!(fixture.global_entries(), ["external"]);
}

/// A file at the default location is kept and reported.
#[tokio::test]
async fn a_file_is_kept_and_reported() {
    let fixture = Fixture::new();
    let root = fixture.root("root");
    std::fs::write(fixture.default_location(), b"kept").expect("plant a file");

    let kept = redirect_to(
        fixture.global.path(),
        Some(&root),
        Some(&setting(&root)),
        None,
    )
    .await
    .expect("examine the default location");

    assert_eq!(kept, Redirect::Kept(Occupant::Other));
    assert_eq!(
        std::fs::read(fixture.default_location()).expect("read the file"),
        b"kept"
    );
}

/// An empty directory at the default location holds nothing to keep, and takes the redirect.
#[tokio::test]
async fn an_empty_directory_takes_the_redirect() {
    let fixture = Fixture::new();
    let root = fixture.root("root");
    std::fs::create_dir(fixture.default_location()).expect("plant an empty directory");

    let placed = redirect_to(
        fixture.global.path(),
        Some(&root),
        Some(&setting(&root)),
        None,
    )
    .await
    .expect("make the redirect");

    assert_eq!(placed, Redirect::Matched);
    assert_eq!(target_of(&fixture.default_location()), root);
}

/// With no root to lead to, a redirect the command made is removed, and the path left free for
/// the root to take; a free path stays free.
#[tokio::test]
async fn a_redirect_the_command_made_is_removed() {
    let fixture = Fixture::new();
    let root = fixture.root("root");
    let root_setting = setting(&root);
    redirect_to(
        fixture.global.path(),
        Some(&root),
        Some(&root_setting),
        None,
    )
    .await
    .expect("make the redirect");

    for _ in 0..2 {
        let removed = redirect_to(fixture.global.path(), None, Some(&root_setting), None)
            .await
            .expect("remove the redirect");

        assert_eq!(removed, Redirect::Matched);
        assert!(
            std::fs::symlink_metadata(fixture.default_location()).is_err(),
            "the default location is free"
        );
        assert!(root.is_dir(), "the root stays where it is");
    }
    assert!(fixture.global_entries().is_empty());
}
