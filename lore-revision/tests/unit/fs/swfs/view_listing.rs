// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Each test drives an operation over its mount resources and a revision, whose future is
// larger than the library's threshold; a test runs it once.
#![allow(clippy::large_futures)]
// The fixtures write and remove the files a mount holds directly, as a user would.
#![allow(clippy::disallowed_methods)]
use std::collections::BTreeMap;

use lore_revision::fs::filesystem_provider::DirectoryListing;
use lore_revision::fs::filesystem_provider::FileInfo;
use lore_revision::fs::filesystem_provider::InstanceOperation;
use lore_revision::fs::os::OsFilesystem;
use lore_revision::fs::swfs::modified_tree::Change;
use lore_revision::fs::swfs::view::ListingAnswer;
use lore_revision::fs::swfs::view::SwfsView;

use super::fake_frozen::FakeFrozen;
use super::listing_fixture::Held;
use super::listing_fixture::created;
use super::listing_fixture::deleted;
use super::listing_fixture::fixture;
use super::listing_fixture::in_context;
use super::listing_fixture::node;
use super::listing_fixture::written;

/// What a listing holds, by lowercased name: whether each is a directory, and a file's size.
async fn described(mut listing: DirectoryListing) -> BTreeMap<String, (bool, u64)> {
    let mut entries = BTreeMap::new();
    while let Some(entry) = listing.next().await {
        let entry = entry.expect("a readable entry");
        let held = match entry.info {
            FileInfo::Directory => (true, 0),
            FileInfo::File { size, .. } => (false, size),
            FileInfo::NotExist => panic!("a listing holds no entry for what is not there"),
        };
        assert_eq!(
            entry.name_hash,
            lore_storage::hash::hash_string(&entry.name),
            "the hash of {}",
            entry.name
        );
        let previous = entries.insert(entry.name.to_lowercase(), held);
        assert!(previous.is_none(), "{} listed twice", entry.name);
    }
    entries
}

async fn listing_of(view: &SwfsView, path: &str) -> BTreeMap<String, (bool, u64)> {
    match view.read_directory(&node(path)).await.expect("listing") {
        ListingAnswer::Listing(listing) => described(*listing).await,
        ListingAnswer::Fallback(reason) => panic!("expected a listing, fell back: {reason}"),
    }
}

/// What the OS lists in `path` of a tree holding `files` (a name ending in `/` is a directory).
async fn os_listing_of(files: &[(&str, &[u8])], path: &str) -> BTreeMap<String, (bool, u64)> {
    let directory = lore_base::test_util::TempDir::new("lore-swfs-listing-");
    for (name, content) in files {
        let target = std::path::PathBuf::from(directory.as_ref()).join(name.trim_end_matches('/'));
        if name.ends_with('/') {
            std::fs::create_dir_all(&target).unwrap();
        } else {
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(&target, content).unwrap();
        }
    }
    let operation = OsFilesystem::new(directory.as_ref()).begin_operation();
    described(
        operation
            .read_directory(&node(path))
            .await
            .expect("the OS listing"),
    )
    .await
}

#[tokio::test]
async fn an_unmodified_directory_lists_the_revisions_children_as_the_os_would() {
    in_context(async {
        let f = fixture(
            &[
                ("a.txt", Held::File(b"aaaa")),
                ("b.txt", Held::File(b"bb")),
                ("sub/c.txt", Held::File(b"ccc")),
                ("empty", Held::Directory),
            ],
            vec![],
            FakeFrozen::default(),
            true,
        )
        .await;

        let from_view = listing_of(&f.view, "").await;

        let from_os = os_listing_of(
            &[
                ("a.txt", b"aaaa"),
                ("b.txt", b"bb"),
                ("sub/c.txt", b"ccc"),
                ("empty/", b""),
            ],
            "",
        )
        .await;
        assert_eq!(from_view, from_os);
        assert_eq!(f.view.fallbacks(), 0);
    })
    .await;
}

#[tokio::test]
async fn a_subdirectory_lists_only_its_own_children() {
    in_context(async {
        let f = fixture(
            &[
                ("a.txt", Held::File(b"aaaa")),
                ("sub/c.txt", Held::File(b"ccc")),
                ("sub/deep/d.txt", Held::File(b"d")),
            ],
            vec![],
            FakeFrozen::default(),
            true,
        )
        .await;

        let from_view = listing_of(&f.view, "sub").await;

        let from_os =
            os_listing_of(&[("sub/c.txt", b"ccc"), ("sub/deep/d.txt", b"d")], "sub").await;
        assert_eq!(from_view, from_os);
    })
    .await;
}

#[tokio::test]
async fn changes_in_a_directory_are_applied_to_what_the_revision_holds() {
    in_context(async {
        let f = fixture(
            &[
                ("kept.txt", Held::File(b"kept")),
                ("edited.txt", Held::File(b"old")),
                ("removed.txt", Held::File(b"gone soon")),
                ("moved.txt", Held::File(b"moved")),
            ],
            vec![
                written("edited.txt"),
                deleted("removed.txt", false),
                Change::Moved {
                    from: "moved.txt".into(),
                    to: "renamed.txt".into(),
                    directory: false,
                },
                created("added.txt", false),
                created("newdir", true),
            ],
            FakeFrozen::default()
                .with_file(r"\edited.txt", b"edited, longer")
                .with_file(r"\renamed.txt", b"moved")
                .with_file(r"\added.txt", b"brand new")
                .with_directory(r"\newdir"),
            true,
        )
        .await;

        let from_view = listing_of(&f.view, "").await;

        let from_os = os_listing_of(
            &[
                ("kept.txt", b"kept"),
                ("edited.txt", b"edited, longer"),
                ("renamed.txt", b"moved"),
                ("added.txt", b"brand new"),
                ("newdir/", b""),
            ],
            "",
        )
        .await;
        assert_eq!(from_view, from_os);
        assert_eq!(f.view.fallbacks(), 0);
    })
    .await;
}

#[tokio::test]
async fn a_changed_name_is_listed_as_the_snapshot_spells_it() {
    in_context(async {
        // Renamed only in case: the record holds one node for both spellings.
        let f = fixture(
            &[("File.txt", Held::File(b"x"))],
            vec![deleted("File.txt", false), created("file.txt", false)],
            FakeFrozen::default().with_file(r"\file.txt", b"x"),
            true,
        )
        .await;

        let mut listing = match f.view.read_directory(&node("")).await.unwrap() {
            ListingAnswer::Listing(listing) => *listing,
            ListingAnswer::Fallback(reason) => panic!("fell back: {reason}"),
        };
        let mut names = Vec::new();
        while let Some(entry) = listing.next().await {
            names.push(entry.unwrap().name);
        }

        assert_eq!(names, vec!["file.txt".to_string()]);
    })
    .await;
}

#[tokio::test]
async fn files_in_an_added_directory_are_listed_from_the_snapshot() {
    in_context(async {
        let f = fixture(
            &[],
            vec![created("sub", true), created("sub/n.txt", false)],
            FakeFrozen::default()
                .with_directory(r"\sub")
                .with_file(r"\sub\n.txt", b"nested"),
            true,
        )
        .await;

        let from_view = listing_of(&f.view, "sub").await;

        assert_eq!(
            from_view,
            os_listing_of(&[("sub/n.txt", b"nested")], "sub").await
        );
    })
    .await;
}

#[tokio::test]
async fn a_link_in_the_revision_is_left_out_of_a_listing() {
    in_context(async {
        let f = fixture(
            &[("a.txt", Held::File(b"a")), ("linked", Held::Link)],
            vec![],
            FakeFrozen::default(),
            true,
        )
        .await;

        let from_view = listing_of(&f.view, "").await;

        assert_eq!(from_view, os_listing_of(&[("a.txt", b"a")], "").await);
    })
    .await;
}

#[tokio::test]
async fn a_deleted_directory_has_no_listing() {
    in_context(async {
        let f = fixture(
            &[("gone/x.txt", Held::File(b"x"))],
            vec![deleted("gone", true)],
            FakeFrozen::default(),
            true,
        )
        .await;

        assert!(f.view.read_directory(&node("gone")).await.is_err());
        assert!(f.view.read_directory(&node("gone/inside")).await.is_err());
    })
    .await;
}

#[tokio::test]
async fn a_changed_child_the_snapshot_lacks_is_listed_as_the_revision_holds_it() {
    in_context(async {
        // Changed, then replaced by a thaw, so SWFS holds no change to it while the record,
        // which keeps a path until the next mount, still does.
        let f = fixture(
            &[("old.txt", Held::File(b"x"))],
            vec![written("old.txt")],
            FakeFrozen::default(),
            true,
        )
        .await;

        assert_eq!(
            listing_of(&f.view, "").await,
            BTreeMap::from([("old.txt".to_string(), (false, 1))])
        );
        assert_eq!(f.view.fallbacks(), 0);
    })
    .await;
}

#[tokio::test]
async fn an_untrusted_record_answers_no_listing_and_leaves_none_to_the_mount() {
    in_context(async {
        let f = fixture(
            &[("a.txt", Held::File(b"a"))],
            vec![],
            FakeFrozen::default(),
            false,
        )
        .await;

        assert!(f.view.read_directory(&node("")).await.is_err());
        assert_eq!(f.view.fallbacks(), 0);
    })
    .await;
}

#[tokio::test]
async fn a_directory_nothing_holds_has_no_listing() {
    in_context(async {
        let f = fixture(
            &[("a.txt", Held::File(b"a"))],
            vec![],
            FakeFrozen::default(),
            true,
        )
        .await;

        assert!(f.view.read_directory(&node("nowhere")).await.is_err());
        assert_eq!(f.view.fallbacks(), 0);
    })
    .await;
}

#[tokio::test]
async fn files_swfs_froze_that_the_record_does_not_hold_are_listed() {
    in_context(async {
        // Such as an ignored file, which the record drops and the revision does not hold.
        let f = fixture(
            &[
                ("a.txt", Held::File(b"aaaa")),
                ("sub/b.txt", Held::File(b"b")),
            ],
            vec![],
            FakeFrozen::default()
                .with_file(r"\scratch.tmp", b"scratch")
                .with_file(r"\sub\inner.tmp", b"inner"),
            true,
        )
        .await;

        let root = listing_of(&f.view, "").await;
        let sub = listing_of(&f.view, "sub").await;

        assert_eq!(
            root,
            os_listing_of(
                &[
                    ("a.txt", b"aaaa"),
                    ("scratch.tmp", b"scratch"),
                    ("sub/b.txt", b"b")
                ],
                ""
            )
            .await
        );
        assert_eq!(
            sub,
            os_listing_of(&[("sub/b.txt", b"b"), ("sub/inner.tmp", b"inner")], "sub").await
        );
    })
    .await;
}

#[tokio::test]
async fn a_directory_only_the_snapshot_holds_is_listed() {
    in_context(async {
        let f = fixture(
            &[("a.txt", Held::File(b"a"))],
            vec![],
            FakeFrozen::default().with_file(r"\scratch\x.tmp", b"x"),
            true,
        )
        .await;

        let from_view = listing_of(&f.view, "scratch").await;

        assert_eq!(
            from_view,
            os_listing_of(&[("scratch/x.tmp", b"x")], "scratch").await
        );
    })
    .await;
}

#[tokio::test]
async fn a_file_the_revision_holds_that_swfs_froze_without_the_record_is_listed_as_frozen() {
    in_context(async {
        // Such as an edited file an ignore rule matches, which the record drops.
        let f = fixture(
            &[
                ("a.txt", Held::File(b"aaaa")),
                ("b.txt", Held::File(b"bbbb")),
            ],
            vec![],
            FakeFrozen::default().with_file(r"\a.txt", b"edited locally"),
            true,
        )
        .await;

        let root = listing_of(&f.view, "").await;

        assert_eq!(
            root,
            os_listing_of(&[("a.txt", b"edited locally"), ("b.txt", b"bbbb")], "").await
        );
    })
    .await;
}
