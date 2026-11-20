// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The push core as a revision graph push runs it: the branch alone, its linked revisions checked
//! on the remote, run on the context the graph push takes.

use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_base::types::Partition;

use super::support::*;

/// A revision linking a revision of another repository that the server doesn't hold fails the
/// push with `RevisionNotFound` before anything is pushed: the server's latest stays, the push
/// publishes nothing, and the server holds no state of the linking revision.
#[tokio::test]
async fn a_linked_revision_the_remote_lacks_fails_the_push_before_anything_is_pushed() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let linking = commit_linking_an_unpublished_revision(&f, seeded).await;

    let (pushed, report) = push_branch_alone(&f, f.default_branch, linking).await;

    let Err(err) = pushed else {
        panic!("a push linking a revision the remote lacks fails");
    };
    assert!(err.is_revision_not_found(), "{err:?}");
    assert_eq!(
        (
            server_latest(&f, f.default_branch).await,
            report.published,
            server_holds_revision(&f, f.repository, linking).await
        ),
        (seeded, Hash::default(), false)
    );
    f.close().await;
}

/// A revision linking a revision of another repository that the server holds is published, and
/// the local latest moves to the revision published.
#[tokio::test]
async fn a_linked_revision_the_remote_holds_lets_the_push_publish() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let linked = Partition::from(uuid::Uuid::now_v7());
    let linked_revision = commit_on(
        f.store,
        linked,
        BranchId::from(uuid::Uuid::now_v7()),
        Hash::default(),
    )
    .await;
    let linking = commit_with_link(&f, f.default_branch, seeded, linked, linked_revision).await;

    let (pushed, report) = push_branch_alone(&f, f.default_branch, linking).await;

    pushed.expect("the push publishes");
    let published = server_latest(&f, f.default_branch).await;
    assert_eq!(
        (
            report.published,
            report.local_latest,
            local_latest(f.store, f.repository, f.default_branch).await
        ),
        (published, published, published)
    );
    f.close().await;
}

/// A tree commit that moves the local latest while the push runs stays in place: the push writes
/// the published revision over the local latest it pushed, misses, succeeds, and reports the
/// commit as the local latest it left.
#[tokio::test]
async fn a_local_write_that_misses_leaves_the_commit_and_succeeds() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let pushed_revision = commit_on(f.store, f.repository, f.default_branch, seeded).await;
    let committed = commit_on(f.store, f.repository, f.default_branch, pushed_revision).await;

    let (pushed, report) = push_branch_alone(&f, f.default_branch, pushed_revision).await;

    pushed.expect("the push publishes");
    assert_eq!(
        (
            server_latest(&f, f.default_branch).await,
            local_latest(f.store, f.repository, f.default_branch).await,
            report.local_latest
        ),
        (pushed_revision, committed, committed)
    );
    f.close().await;
}
