// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Each test drives an operation over its mount resources and a revision, whose future is
// larger than the library's threshold; a test runs it once.
#![allow(clippy::large_futures)]
use std::sync::Arc;

use lore_revision::fs::swfs::modified_tree::Change;
use lore_revision::fs::swfs::renamed_at_risk::RenamedAtRisk;
use lore_revision::fs::swfs::view::ContentAnswer;
use lore_revision::fs::swfs::view::HoldsAnswer;
use lore_revision::state::NodeComparison;
use lore_storage::ContentHashes;

use super::fake_frozen::FakeFrozen;
use super::listing_fixture::Held;
use super::listing_fixture::address_of;
use super::listing_fixture::fixture;
use super::listing_fixture::in_context;
use super::listing_fixture::node;

async fn bytes_of(answer: ContentAnswer) -> Vec<u8> {
    match answer {
        ContentAnswer::Source(source) => source.read_all().await.expect("reading").to_vec(),
        ContentAnswer::Fallback(reason) => panic!("expected content, fell back: {reason}"),
    }
}

#[tokio::test]
async fn an_unmodified_file_is_read_from_the_store() {
    in_context(async {
        let f = fixture(
            &[("a.txt", Held::File(b"from the revision"))],
            vec![],
            FakeFrozen::default(),
            true,
        )
        .await;

        let answer = f.view.content_source(&node("a.txt")).await.unwrap();

        assert_eq!(bytes_of(answer).await, b"from the revision");
        assert_eq!(f.view.fallbacks(), 0);
    })
    .await;
}

#[tokio::test]
async fn an_unmodified_file_holds_the_content_its_own_node_addresses() {
    in_context(async {
        let content: &[u8] = b"from the revision";
        let f = fixture(
            &[("a.txt", Held::File(content))],
            vec![],
            FakeFrozen::default(),
            true,
        )
        .await;

        let answer = f
            .view
            .file_holds_content(
                f.repository.clone(),
                &node("a.txt"),
                address_of(content),
                content.len() as u64,
                &ContentHashes::default(),
            )
            .await
            .unwrap();

        assert!(matches!(
            answer,
            HoldsAnswer::Compared(NodeComparison::Matches)
        ));
        assert_eq!(f.view.fallbacks(), 0);
    })
    .await;
}

#[tokio::test]
async fn an_unmodified_file_differs_from_other_content() {
    in_context(async {
        let other: &[u8] = b"something else entirely";
        // The other content is in the store, as any content a node addressed once would be.
        let f = fixture(
            &[
                ("a.txt", Held::File(b"from the revision")),
                ("other.txt", Held::File(other)),
            ],
            vec![],
            FakeFrozen::default(),
            true,
        )
        .await;

        let answer = f
            .view
            .file_holds_content(
                f.repository.clone(),
                &node("a.txt"),
                address_of(other),
                other.len() as u64,
                &ContentHashes::default(),
            )
            .await
            .unwrap();

        assert!(matches!(
            answer,
            HoldsAnswer::Compared(NodeComparison::Differs)
        ));
    })
    .await;
}

#[tokio::test]
async fn a_changed_file_is_read_from_the_frozen_snapshot() {
    in_context(async {
        let f = fixture(
            &[("b.txt", Held::File(b"the revision's b"))],
            vec![Change::Written("b.txt".into())],
            FakeFrozen::default().with_file(r"\b.txt", b"b as changed"),
            true,
        )
        .await;

        let answer = f.view.content_source(&node("b.txt")).await.unwrap();

        assert_eq!(bytes_of(answer).await, b"b as changed");
        assert_eq!(f.view.fallbacks(), 0);
    })
    .await;
}

#[tokio::test]
async fn a_changed_file_is_compared_as_the_snapshot_holds_it() {
    in_context(async {
        let changed: &[u8] = b"b as changed";
        // The content `previous` addresses is in the store, as the revision's would be.
        let f = fixture(
            &[
                ("b.txt", Held::File(b"the revision's b")),
                ("expected.txt", Held::File(changed)),
            ],
            vec![Change::Written("b.txt".into())],
            FakeFrozen::default().with_file(r"\b.txt", changed),
            true,
        )
        .await;

        let answer = f
            .view
            .file_holds_content(
                f.repository.clone(),
                &node("b.txt"),
                address_of(changed),
                changed.len() as u64,
                &ContentHashes::default(),
            )
            .await
            .unwrap();

        assert!(matches!(
            answer,
            HoldsAnswer::Compared(NodeComparison::Matches)
        ));
    })
    .await;
}

#[tokio::test]
async fn a_frozen_file_without_bytes_of_its_own_reads_the_revisions() {
    in_context(async {
        // Changed only in name or attributes, so SWFS freezes it without bytes: they are the
        // source's, which the revision holds at the path.
        let f = fixture(
            &[("c2.txt", Held::File(b"x"))],
            vec![Change::Written("c2.txt".into())],
            FakeFrozen::default().with_unreadable_file(r"\c2.txt", 4),
            true,
        )
        .await;

        let answer = f.view.content_source(&node("c2.txt")).await.unwrap();
        assert_eq!(bytes_of(answer).await, b"x");
        assert_eq!(f.view.fallbacks(), 0);
    })
    .await;
}

#[tokio::test]
async fn a_file_renamed_over_another_the_revision_holds_reads_what_it_holds_where_it_came_from() {
    in_context(async {
        let f = fixture(
            &[("c1.txt", Held::File(b"x")), ("c2.txt", Held::File(b"y"))],
            vec![Change::Moved {
                from: "c1.txt".into(),
                to: "c2.txt".into(),
                directory: false,
            }],
            FakeFrozen::default()
                .with_unreadable_file(r"\c2.txt", 1)
                .with_move(r"\c1.txt", r"\c2.txt"),
            true,
        )
        .await;

        let answer = f.view.content_source(&node("c2.txt")).await.unwrap();
        assert_eq!(bytes_of(answer).await, b"x");
    })
    .await;
}

#[tokio::test]
async fn a_renamed_file_without_bytes_reads_what_the_revision_holds_where_it_came_from() {
    in_context(async {
        // Renamed, so SWFS freezes it without bytes: they are the source's at its old path.
        let f = fixture(
            &[("c1.txt", Held::File(b"x"))],
            vec![Change::Moved {
                from: "c1.txt".into(),
                to: "c2.txt".into(),
                directory: false,
            }],
            FakeFrozen::default()
                .with_unreadable_file(r"\c2.txt", 4)
                .with_move(r"\c1.txt", r"\c2.txt"),
            true,
        )
        .await;

        let answer = f.view.content_source(&node("c2.txt")).await.unwrap();
        assert_eq!(bytes_of(answer).await, b"x");
    })
    .await;
}

#[tokio::test]
async fn a_frozen_file_without_bytes_and_no_known_origin_cannot_be_read() {
    in_context(async {
        // A file renamed without being written, which the revision holds under another name, and
        // whose origin SWFS lists no longer, as after the user made something there: read through
        // the mount, its bytes would cost a callback that reads the store.
        let f = fixture(
            &[("c1.txt", Held::File(b"x"))],
            vec![Change::Written("c2.txt".into())],
            FakeFrozen::default().with_unreadable_file(r"\c2.txt", 4),
            true,
        )
        .await;

        assert!(matches!(
            f.view.content_source(&node("c2.txt")).await,
            Err(error) if error.is_internal()
        ));
        assert_eq!(f.view.fallbacks(), 0);
    })
    .await;
}

#[tokio::test]
async fn a_renamed_file_swfs_lists_no_origin_for_reads_the_revision_where_a_kept_move_says() {
    in_context(async {
        let f = fixture(
            &[("c1.txt", Held::File(b"x"))],
            vec![Change::Written("c2.txt".into())],
            FakeFrozen::default().with_unreadable_file(r"\c2.txt", 4),
            true,
        )
        .await;
        let kept = Arc::new(RenamedAtRisk::default());
        kept.moved("c1.txt", "c2.txt", false, true);
        let view = f.view.with_kept_moves(kept);

        let answer = view.content_source(&node("c2.txt")).await.unwrap();
        assert_eq!(bytes_of(answer).await, b"x");
    })
    .await;
}

#[tokio::test]
async fn a_changed_file_the_snapshot_lacks_is_read_from_the_revision() {
    in_context(async {
        // Changed, then replaced by a thaw, so SWFS holds no change to it while the record,
        // which keeps a path until the next mount, still does.
        let f = fixture(
            &[("old.txt", Held::File(b"x"))],
            vec![Change::Written("old.txt".into())],
            FakeFrozen::default(),
            true,
        )
        .await;

        let answer = f.view.content_source(&node("old.txt")).await.unwrap();
        assert_eq!(bytes_of(answer).await, b"x");
        assert_eq!(f.view.fallbacks(), 0);
    })
    .await;
}

#[tokio::test]
async fn a_deleted_file_has_no_content_and_cannot_be_read() {
    in_context(async {
        let f = fixture(
            &[("gone.txt", Held::File(b"was here"))],
            vec![Change::Deleted {
                path: "gone.txt".into(),
                directory: false,
            }],
            FakeFrozen::default(),
            true,
        )
        .await;

        assert!(f.view.content_source(&node("gone.txt")).await.is_err());
        let holds = f
            .view
            .file_holds_content(
                f.repository.clone(),
                &node("gone.txt"),
                address_of(b"was here"),
                8,
                &ContentHashes::default(),
            )
            .await
            .unwrap();
        assert!(matches!(
            holds,
            HoldsAnswer::Compared(NodeComparison::Unreadable)
        ));
    })
    .await;
}

#[tokio::test]
async fn a_file_swfs_froze_that_the_record_does_not_hold_is_read_from_the_snapshot() {
    in_context(async {
        // Such as an ignored file, which the record drops and the revision does not hold.
        let content = b"ignored but written";
        let f = fixture(
            &[("a.txt", Held::File(b"x"))],
            vec![],
            FakeFrozen::default().with_file(r"\scratch.tmp", content),
            true,
        )
        .await;

        let answer = f.view.content_source(&node("scratch.tmp")).await.unwrap();

        assert_eq!(bytes_of(answer).await, content);
        let holds = f
            .view
            .file_holds_content(
                f.repository.clone(),
                &node("scratch.tmp"),
                address_of(content),
                content.len() as u64,
                &ContentHashes::default(),
            )
            .await
            .unwrap();
        assert!(matches!(
            holds,
            HoldsAnswer::Compared(NodeComparison::Matches)
        ));
        assert_eq!(f.view.fallbacks(), 0);
    })
    .await;
}

#[tokio::test]
async fn a_path_nothing_holds_is_not_there() {
    in_context(async {
        let f = fixture(
            &[("a.txt", Held::File(b"x"))],
            vec![],
            FakeFrozen::default(),
            true,
        )
        .await;

        assert!(f.view.content_source(&node("nowhere.txt")).await.is_err());
        let holds = f
            .view
            .file_holds_content(
                f.repository.clone(),
                &node("nowhere.txt"),
                address_of(b"x"),
                1,
                &ContentHashes::default(),
            )
            .await
            .unwrap();
        assert!(matches!(
            holds,
            HoldsAnswer::Compared(NodeComparison::Unreadable)
        ));
        assert_eq!(f.view.fallbacks(), 0);
    })
    .await;
}

#[tokio::test]
async fn an_untrusted_record_answers_no_content_and_leaves_none_to_the_mount() {
    in_context(async {
        let f = fixture(
            &[("a.txt", Held::File(b"x"))],
            vec![],
            FakeFrozen::default(),
            false,
        )
        .await;

        assert!(f.view.content_source(&node("a.txt")).await.is_err());
        let holds = f
            .view
            .file_holds_content(
                f.repository.clone(),
                &node("a.txt"),
                address_of(b"x"),
                1,
                &ContentHashes::default(),
            )
            .await;
        assert!(holds.is_err());
        assert_eq!(f.view.fallbacks(), 0);
    })
    .await;
}

#[tokio::test]
async fn the_origin_of_a_renamed_file_is_found_only_for_a_named_destination() {
    in_context(async {
        let f = fixture(
            &[],
            vec![],
            FakeFrozen::default()
                .with_unreadable_file(r"\b.txt", 4)
                .with_move(r"\a.txt", r"\b.txt")
                .with_unreadable_file(r"\other.txt", 4)
                .with_move(r"\elsewhere.txt", r"\other.txt"),
            true,
        )
        .await;

        assert_eq!(
            f.view.origins_of_renamed_files(&[node("b.txt")]),
            vec![node("a.txt")]
        );
        // In any spelling, as the mount names it.
        assert_eq!(
            f.view.origins_of_renamed_files(&[node("B.txt")]),
            vec![node("a.txt")]
        );
        assert!(f.view.origins_of_renamed_files(&[node("c.txt")]).is_empty());
    })
    .await;
}

#[tokio::test]
async fn the_origin_of_a_renamed_directory_is_not_found() {
    in_context(async {
        // Only a file's origin is named.
        let f = fixture(
            &[],
            vec![],
            FakeFrozen::default()
                .with_directory(r"\e")
                .with_move(r"\d", r"\e"),
            true,
        )
        .await;

        assert!(f.view.origins_of_renamed_files(&[node("e")]).is_empty());
    })
    .await;
}

#[tokio::test]
async fn a_rename_swfs_froze_nothing_at_has_no_origin_found() {
    in_context(async {
        let f = fixture(
            &[],
            vec![],
            FakeFrozen::default().with_move(r"\a.txt", r"\b.txt"),
            true,
        )
        .await;

        assert!(f.view.origins_of_renamed_files(&[node("b.txt")]).is_empty());
    })
    .await;
}
