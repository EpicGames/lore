// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Each test drives an operation over its mount resources and a revision, whose future is
// larger than the library's threshold; a test runs it once.
#![allow(clippy::large_futures)]
// The fixtures write and remove the files a mount holds directly, as a user would.
#![allow(clippy::disallowed_methods)]
use lore_revision::fs::filesystem_provider::InstanceOperation;
use lore_revision::fs::os::OsFilesystem;

use super::fake_frozen::FakeFrozen;
use super::listing_fixture::Held;
use super::listing_fixture::created;
use super::listing_fixture::deleted;
use super::listing_fixture::fixture;
use super::listing_fixture::in_context;
use super::listing_fixture::node;

fn held(answer: Option<bool>) -> bool {
    answer.expect("expected an answer, fell back")
}

#[tokio::test]
async fn a_name_is_held_only_in_the_spelling_the_revision_gives_it() {
    in_context(async {
        let f = fixture(
            &[
                ("File.txt", Held::File(b"x")),
                ("sub/Inner.txt", Held::File(b"y")),
            ],
            vec![],
            FakeFrozen::default(),
            true,
        )
        .await;

        assert!(held(
            f.view.holds_name_exactly(&node("File.txt")).await.unwrap()
        ));
        assert!(!held(
            f.view.holds_name_exactly(&node("file.txt")).await.unwrap()
        ));
        assert!(!held(
            f.view.holds_name_exactly(&node("FILE.TXT")).await.unwrap()
        ));
        assert!(!held(
            f.view.holds_name_exactly(&node("nope.txt")).await.unwrap()
        ));
        assert!(held(
            f.view
                .holds_name_exactly(&node("sub/Inner.txt"))
                .await
                .unwrap()
        ));
        assert!(!held(
            f.view
                .holds_name_exactly(&node("sub/inner.txt"))
                .await
                .unwrap()
        ));
        assert_eq!(f.view.fallbacks(), 0);
    })
    .await;
}

/// The same names answered the same way as the OS answers them for a tree holding them, where the
/// OS can say.
#[tokio::test]
async fn the_answers_are_the_os_answers_for_the_same_tree() {
    in_context(async {
        let f = fixture(
            &[
                ("File.txt", Held::File(b"x")),
                ("Dir/Inner.txt", Held::File(b"y")),
            ],
            vec![],
            FakeFrozen::default(),
            true,
        )
        .await;
        let directory = lore_base::test_util::TempDir::new("lore-swfs-names-");
        std::fs::write(
            std::path::PathBuf::from(directory.as_ref()).join("File.txt"),
            b"x",
        )
        .unwrap();
        std::fs::create_dir_all(std::path::PathBuf::from(directory.as_ref()).join("Dir")).unwrap();
        std::fs::write(
            std::path::PathBuf::from(directory.as_ref()).join("Dir/Inner.txt"),
            b"y",
        )
        .unwrap();
        let os = OsFilesystem::new(directory.as_ref()).begin_operation();

        for path in [
            "File.txt",
            "file.txt",
            "Dir",
            "dir",
            "Dir/Inner.txt",
            "Dir/inner.txt",
            "missing.txt",
            "missing/x.txt",
        ] {
            let Some(from_view) = f.view.holds_name_exactly(&node(path)).await.unwrap() else {
                continue;
            };
            // macOS cannot say whether a name is held in its exact spelling, and answers `None`.
            let Some(from_os) = os.holds_name_exactly(&node(path)).await else {
                continue;
            };
            assert_eq!(from_view, from_os, "{path}");
        }
    })
    .await;
}

#[tokio::test]
async fn a_name_the_changes_renamed_is_held_in_its_new_spelling() {
    in_context(async {
        let f = fixture(
            &[("File.txt", Held::File(b"x"))],
            vec![deleted("File.txt", false), created("file.txt", false)],
            FakeFrozen::default().with_file(r"\file.txt", b"x"),
            true,
        )
        .await;

        assert!(held(
            f.view.holds_name_exactly(&node("file.txt")).await.unwrap()
        ));
        assert!(!held(
            f.view.holds_name_exactly(&node("File.txt")).await.unwrap()
        ));
    })
    .await;
}

#[tokio::test]
async fn a_name_under_a_deleted_directory_is_not_held() {
    in_context(async {
        let f = fixture(
            &[("gone/x.txt", Held::File(b"x"))],
            vec![deleted("gone", true)],
            FakeFrozen::default(),
            true,
        )
        .await;

        assert!(!held(
            f.view
                .holds_name_exactly(&node("gone/x.txt"))
                .await
                .unwrap()
        ));
        assert!(!held(
            f.view.holds_name_exactly(&node("gone")).await.unwrap()
        ));
    })
    .await;
}

#[tokio::test]
async fn a_name_asked_of_an_untrusted_record_is_an_error_not_a_read_of_the_mount() {
    in_context(async {
        let f = fixture(
            &[("a.txt", Held::File(b"a"))],
            vec![],
            FakeFrozen::default(),
            true,
        )
        .await;
        let untrusted = fixture(
            &[("a.txt", Held::File(b"a"))],
            vec![],
            FakeFrozen::default(),
            false,
        )
        .await;

        // Beside a directory nothing holds, nothing holds the name.
        assert_eq!(
            f.view
                .holds_name_exactly(&node("elsewhere/a.txt.mine"))
                .await
                .unwrap(),
            Some(false)
        );
        assert_eq!(f.view.fallbacks(), 0);
        assert!(
            untrusted
                .view
                .holds_name_exactly(&node("a.txt"))
                .await
                .is_err()
        );
        assert_eq!(untrusted.view.fallbacks(), 0);
    })
    .await;
}
