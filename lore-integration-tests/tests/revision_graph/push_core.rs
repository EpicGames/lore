// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The push core as a revision graph push runs it: the branch alone, its linked revisions checked
//! on the remote, run on the context the graph push takes.

use lore::revision_graph::locations::full_context;
use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_revision::branch::push::MountPolicy;
use lore_revision::branch::push::PushError;
use lore_revision::branch::push::PushReport;
use lore_revision::branch::push::PushRun;
use lore_revision::branch::push::push_branch;

use super::support::*;
use crate::common::net_common::close_handle;

/// Pushes the local latest `latest` of `branch` of `f`'s repository with the push core, as a
/// revision graph push runs it, on a context over `f`'s storage handle connected to the server:
/// the branch alone, with its linked revisions checked on the remote, and the local latest
/// written over `latest`. Returns the push's outcome and report.
async fn push_branch_alone(
    f: &Fixture,
    branch: BranchId,
    latest: Hash,
) -> (Result<(), PushError>, PushReport) {
    let store = handle_state(f);
    let remote = store.remote.clone().expect("the handle has a remote");
    in_client(async {
        let context = full_context(&store, f.repository, Some(&remote))
            .await
            .expect("connects to the server");
        let token = context
            .try_write_token()
            .expect("a write context carries a write token");
        let run = PushRun {
            mounts: MountPolicy::RequireOnRemote,
            expected_local_latest: Some(latest),
            ..PushRun::default()
        };
        let mut report = PushReport::default();
        let pushed = push_branch(context.clone(), token, &run, branch, latest, &mut report).await;
        (pushed, report)
    })
    .await
}

/// A revision linking a revision of another repository that the server doesn't hold fails the
/// push with `RevisionNotFound` before anything is pushed: the server's latest stays, the push
/// publishes nothing, and the server holds no state of the linking revision.
#[tokio::test]
async fn a_linked_revision_the_remote_lacks_fails_the_push_before_anything_is_pushed() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let linked = Partition::from(uuid::Uuid::now_v7());
    let local_only = handle_over_local_stores(&f, None);
    let linked_revision = commit_on(
        local_only,
        linked,
        BranchId::from(uuid::Uuid::now_v7()),
        Hash::default(),
    )
    .await;
    close_handle(local_only).await;
    let linking = commit_with_link(&f, f.default_branch, seeded, linked, linked_revision).await;

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
