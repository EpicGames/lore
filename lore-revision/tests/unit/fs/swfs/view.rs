// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Each test drives an operation over its mount resources and a revision, whose future is
// larger than the library's threshold; a test runs it once.
#![allow(clippy::large_futures)]
use std::path::Path;
use std::sync::Arc;

use lore_revision::filter;
use lore_revision::filter::FilterInstance;
use lore_revision::filter::parse_filter;
use lore_revision::fs::swfs::frozen_files::FrozenSnapshot;
use lore_revision::fs::swfs::modified_tree::Change;
use lore_revision::fs::swfs::modified_tree::ModifiedTree;
use lore_revision::fs::swfs::served_tree::ServedTree;
use lore_revision::fs::swfs::view::FallbackReason;
use lore_revision::fs::swfs::view::ReadSource;
use lore_revision::fs::swfs::view::SwfsView;
use lore_revision::repository::RepositoryContext;

use super::fake_frozen::FakeFrozen;
use super::listing_fixture::created;
use super::listing_fixture::deleted;
use super::listing_fixture::in_context;
use super::listing_fixture::node;
use super::listing_fixture::written;
use crate::fs::filesystem_provider::test_store_create;
use crate::helper::default_repository_creation_args;

/// A trusted record holding `changes`, and a snapshot of what `frozen` lists.
async fn view_of(changes: Vec<Change>, frozen: FakeFrozen, trusted: bool) -> SwfsView {
    let (immutable_store, mutable_store, _execution) =
        test_store_create().await.expect("Making test stores");
    let repository = Arc::new(RepositoryContext::new(default_repository_creation_args(
        immutable_store,
        mutable_store,
    )));
    let record = Arc::new(ModifiedTree::new(repository.clone()));
    for change in changes {
        record.record(change).await;
    }
    record.set_trusted(trusted);
    SwfsView::new(
        record,
        FrozenSnapshot::capture(Arc::new(frozen)).expect("capturing the snapshot"),
        Arc::new(ServedTree::new(repository, None, Vec::new())),
    )
}

#[tokio::test]
async fn a_path_the_record_does_not_hold_is_answered_from_the_revision() {
    in_context(async {
        let view = view_of(
            vec![written("b.txt")],
            FakeFrozen::default().with_file(r"\b.txt", b"BBBB"),
            true,
        )
        .await;

        let source = view.classify(&node("a.txt")).await.unwrap();

        assert!(matches!(source, ReadSource::Unmodified));
        assert_eq!(view.fallbacks(), 0);
    })
    .await;
}

#[tokio::test]
async fn a_changed_path_is_answered_from_the_frozen_snapshot() {
    in_context(async {
        let view = view_of(
            vec![written("b.txt")],
            FakeFrozen::default().with_file(r"\b.txt", b"BBBB"),
            true,
        )
        .await;

        let source = view.classify(&node("b.txt")).await.unwrap();

        let ReadSource::Covered(file) = source else {
            panic!("expected the frozen file, got {source:?}");
        };
        assert_eq!(file.size, 4);
        assert_eq!(view.fallbacks(), 0);
    })
    .await;
}

#[tokio::test]
async fn a_changed_path_the_snapshot_lacks_reads_the_revision() {
    in_context(async {
        // Changed, then replaced by a thaw, so SWFS holds no change to it while the record,
        // which keeps a path until the next mount, still does.
        let view = view_of(vec![written("old.txt")], FakeFrozen::default(), true).await;

        assert!(matches!(
            view.classify(&node("old.txt")).await.unwrap(),
            ReadSource::Unmodified
        ));
        assert_eq!(view.fallbacks(), 0);
    })
    .await;
}

#[tokio::test]
async fn a_deleted_path_is_not_there_and_needs_no_read() {
    in_context(async {
        let view = view_of(
            vec![deleted("gone.txt", false), deleted("gonedir", true)],
            FakeFrozen::default(),
            true,
        )
        .await;

        assert!(matches!(
            view.classify(&node("gone.txt")).await.unwrap(),
            ReadSource::Deleted
        ));
        assert!(matches!(
            view.classify(&node("gonedir/inside.txt")).await.unwrap(),
            ReadSource::Deleted
        ));
        assert_eq!(view.fallbacks(), 0);
    })
    .await;
}

#[tokio::test]
async fn a_file_in_an_added_directory_is_answered_from_the_snapshot() {
    in_context(async {
        let view = view_of(
            vec![created("sub", true), created("sub/n.txt", false)],
            FakeFrozen::default()
                .with_directory(r"\sub")
                .with_file(r"\sub\n.txt", b"nested"),
            true,
        )
        .await;

        assert!(matches!(
            view.classify(&node("sub")).await.unwrap(),
            ReadSource::Covered(file) if file.is_directory
        ));
        assert!(matches!(
            view.classify(&node("sub/n.txt")).await.unwrap(),
            ReadSource::Covered(file) if file.size == 6
        ));
    })
    .await;
}

#[tokio::test]
async fn an_untrusted_record_answers_for_no_path_and_nothing_reads_the_mount() {
    in_context(async {
        let view = view_of(
            vec![written("b.txt")],
            FakeFrozen::default().with_file(r"\b.txt", b"BBBB"),
            false,
        )
        .await;

        for path in ["b.txt", "a.txt", "gone/x.txt"] {
            assert!(view.classify(&node(path)).await.is_err(), "{path}");
        }
        assert_eq!(view.fallbacks(), 0);
    })
    .await;
}

#[tokio::test]
async fn forcing_the_fallback_sends_every_path_to_the_mount() {
    in_context(async {
        let mut view = view_of(
            vec![written("b.txt")],
            FakeFrozen::default().with_file(r"\b.txt", b"BBBB"),
            true,
        )
        .await;
        view.force_fallback(true);

        for path in ["b.txt", "a.txt"] {
            assert!(
                matches!(
                    view.classify(&node(path)).await.unwrap(),
                    ReadSource::Fallback(FallbackReason::Forced)
                ),
                "{path}"
            );
        }

        view.force_fallback(false);
        assert!(matches!(
            view.classify(&node("a.txt")).await.unwrap(),
            ReadSource::Unmodified
        ));
    })
    .await;
}

/// The smoke tests read how many reads an operation answered without the mount, since a saving
/// that touches nothing shows up nowhere else.
#[tokio::test]
async fn reads_answered_without_the_mount_are_counted_apart_from_fallbacks() {
    in_context(async {
        let view = view_of(
            vec![
                written("b.txt"),
                written("old.txt"),
                deleted("gone.txt", false),
            ],
            FakeFrozen::default().with_file(r"\b.txt", b"BBBB"),
            true,
        )
        .await;

        // The revision's, the snapshot's and a deleted path's answers all spare the mount, as
        // does that of a changed path SWFS holds no change to, which the revision answers.
        view.classify(&node("a.txt")).await.unwrap();
        view.classify(&node("b.txt")).await.unwrap();
        view.classify(&node("gone.txt")).await.unwrap();
        view.classify(&node("old.txt")).await.unwrap();

        assert_eq!(view.answered(), 4);
        assert_eq!(view.fallbacks(), 0);

        // A forced read goes to the mount.
        let mut forced = view_of(vec![], FakeFrozen::default(), true).await;
        forced.force_fallback(true);
        forced.classify(&node("a.txt")).await.unwrap();
        assert_eq!(forced.answered(), 0);
        assert_eq!(forced.fallbacks(), 1);
    })
    .await;
}

#[tokio::test]
async fn a_file_swfs_froze_that_the_record_holds_nothing_of_is_answered_from_the_snapshot() {
    in_context(async {
        // Such as an edited file an ignore rule matches, which the record leaves out.
        let view = view_of(
            vec![],
            FakeFrozen::default().with_file(r"\ignored.tmp", b"edited"),
            true,
        )
        .await;

        let source = view.classify(&node("ignored.tmp")).await.unwrap();

        assert!(matches!(source, ReadSource::Covered(_)));
        assert_eq!(view.fallbacks(), 0);
    })
    .await;
}

#[tokio::test]
async fn an_ignored_path_is_answered_from_what_swfs_froze_or_as_unchanged_never_by_the_mount() {
    in_context(async {
        let (immutable_store, mutable_store, _execution) =
            test_store_create().await.expect("Making test stores");
        let repository = Arc::new(RepositoryContext::new(default_repository_creation_args(
            immutable_store,
            mutable_store,
        )));
        let record = Arc::new(ModifiedTree::new(repository.clone()));
        let ignore = parse_filter(b"*.tmp\n", Path::new(".loreignore")).expect("parsing rules");
        record.set_ignore(Arc::new(
            filter::from_instances(ignore, FilterInstance::default()).expect("building filter"),
        ));
        record.set_trusted(true);
        let frozen = FakeFrozen::default().with_file(r"\edited.tmp", b"edited");
        let view = SwfsView::new(
            record,
            FrozenSnapshot::capture(Arc::new(frozen)).expect("capturing the snapshot"),
            Arc::new(ServedTree::new(repository, None, Vec::new())),
        );

        // SWFS freezes every file it holds changed, ignored or not, so one it did not freeze is
        // unchanged: the revision answers for it, here with nothing.
        assert!(matches!(
            view.classify(&node("edited.tmp")).await.unwrap(),
            ReadSource::Covered(_)
        ));
        assert!(matches!(
            view.classify(&node("untouched.tmp")).await.unwrap(),
            ReadSource::Unmodified
        ));
        assert_eq!(view.fallbacks(), 0);
    })
    .await;
}
