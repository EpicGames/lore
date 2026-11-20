// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `branch_push` refusals that need server setup: a protected branch, linked revisions the remote
//! lacks or the caller can't read, and branch names other remote branches hold.

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use lore::revision_graph::branch_push::LoreRevisionGraphBranchPushOptions;
use lore_base::error::BranchAlreadyExists;
use lore_base::error::NotAuthorized;
use lore_base::error::RevisionNotFound;
use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_base::types::RepositoryId;
use lore_revision::branch;
use lore_server::authnz::repository_authorizer::Action;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::VerifiedToken;
use tonic::Status;

use super::support::*;

/// A repository authorizer that answers as `AllowAllRepositoryAuthorizer`, except that it refuses
/// every action on the repository it holds with `PermissionDenied`, once a test sets one.
struct DenyRepository(Arc<Mutex<Option<RepositoryId>>>);

impl DenyRepository {
    fn refuses(&self, repository: RepositoryId) -> bool {
        *self.0.lock().unwrap() == Some(repository)
    }
}

#[async_trait]
impl RepositoryAuthorizer for DenyRepository {
    async fn check_repository_access(
        &self,
        token: Option<&VerifiedToken<'_>>,
        repository: RepositoryId,
        action: Action,
    ) -> Result<(), Status> {
        if self.refuses(repository) {
            return Err(Status::permission_denied("the test refuses the repository"));
        }
        AllowAllRepositoryAuthorizer
            .check_repository_access(token, repository, action)
            .await
    }
}

/// The status of a no-flag push of `branch` on `f`'s handle with `options`.
async fn push_status(
    f: &Fixture,
    branch: BranchId,
    options: LoreRevisionGraphBranchPushOptions,
) -> i32 {
    push_branch_call(f, f.store, graph_globals(), branch, options)
        .await
        .0
}

/// A forced push to a branch the remote protects fails with `NotAuthorized` for an identity
/// without the push-protected permission, and the remote latest stays.
#[tokio::test]
async fn force_on_a_protected_branch_fails_not_authorized() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    on_server(&f, async |context| {
        branch::protect(context, f.default_branch).await
    })
    .await
    .expect("protects the branch on the server");
    commit_on(f.store, f.repository, f.default_branch, seeded).await;

    let status = push_status(
        &f,
        f.default_branch,
        LoreRevisionGraphBranchPushOptions {
            force: 1,
            ..Default::default()
        },
    )
    .await;

    assert_eq!(status, NotAuthorized::FFI_CODE);
    assert_eq!(server_latest(&f, f.default_branch).await, seeded);
    f.close().await;
}

/// A revision linking a revision of another repository that the remote lacks fails the push with
/// `RevisionNotFound` before anything is pushed: the remote latest stays, and the remote holds no
/// state of the linking revision.
#[tokio::test]
async fn a_linked_revision_the_remote_lacks_fails_revision_not_found_before_anything_is_pushed() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let linking = commit_linking_an_unpublished_revision(&f, seeded).await;

    let status = push_status(&f, f.default_branch, Default::default()).await;

    assert_eq!(status, RevisionNotFound::FFI_CODE);
    assert_eq!(
        (
            server_latest(&f, f.default_branch).await,
            server_holds_revision(&f, f.repository, linking).await
        ),
        (seeded, false)
    );
    f.close().await;
}

/// A revision linking a revision of a repository the caller can't read fails the push with
/// `NotAuthorized`, and the remote latest stays.
#[tokio::test]
async fn a_linked_repository_the_caller_cannot_read_fails_not_authorized() {
    let denied = Arc::new(Mutex::new(None));
    let f = fixture_with_authorizer(Arc::new(DenyRepository(denied.clone()))).await;
    let seeded = seed_main(&f).await;
    let linked = Partition::from(uuid::Uuid::now_v7());
    let linked_revision = commit_on(
        f.store,
        linked,
        BranchId::from(uuid::Uuid::now_v7()),
        Hash::default(),
    )
    .await;
    commit_with_link(&f, f.default_branch, seeded, linked, linked_revision).await;
    *denied.lock().unwrap() = Some(linked);

    let status = push_status(&f, f.default_branch, Default::default()).await;

    assert_eq!(status, NotAuthorized::FFI_CODE);
    assert_eq!(server_latest(&f, f.default_branch).await, seeded);
    f.close().await;
}

/// A push of a branch the remote archived restores it under its id and publishes, and fails with
/// `BranchAlreadyExists` where another remote branch took its name meanwhile; that pushed event
/// names the latest the archived branch kept.
#[tokio::test]
async fn an_archived_remote_branch_is_restored_or_fails_branch_already_exists_when_its_name_is_taken()
 {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let restored = create_branch_at_both_locations(
        &f,
        "restored",
        branch::default_category(),
        f.default_branch,
        CREATOR,
    )
    .await;
    let committed = commit_on(f.store, f.repository, restored, seeded).await;
    archive_on_server(&f, restored).await;
    let taken = create_branch_at_both_locations(
        &f,
        "taken",
        branch::default_category(),
        f.default_branch,
        CREATOR,
    )
    .await;
    let taken_latest = commit_on(f.store, f.repository, taken, seeded).await;
    archive_on_server(&f, taken).await;
    let holder = create_remote_branch(&f, "taken", f.default_branch, CREATOR).await;

    let (restoring, restoring_events) =
        push_branch_call(&f, f.store, graph_globals(), restored, Default::default()).await;
    let (refused, refused_events) =
        push_branch_call(&f, f.store, graph_globals(), taken, Default::default()).await;

    let record = server_record(&f, restored).await;
    assert_eq!(
        (restoring, record.deleted, record.latest, refused),
        (0, false, committed, BranchAlreadyExists::FFI_CODE)
    );
    assert_eq!(pushed(&restoring_events), [published(committed, 2)]);
    assert_eq!(
        pushed(&refused_events),
        [Pushed {
            revision_hash: Hash::default(),
            revision_number: 0,
            fast_forward_merged: false,
            remote_latest: seeded,
            local_latest: taken_latest,
        }]
    );
    assert_eq!(server_latest(&f, holder).await, seeded);
    f.close().await;
}

/// The first push of a branch whose name another remote branch holds fails with
/// `BranchAlreadyExists`, and that branch stays as it was. The pushed event names no remote latest,
/// since the remote holds no branch of the pushed one's id.
#[tokio::test]
async fn a_first_push_whose_name_another_remote_branch_holds_fails_branch_already_exists() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let local = create_local_branch(&f, "x", f.default_branch, CREATOR).await;
    let remote = create_remote_branch(&f, "x", f.default_branch, CREATOR).await;
    let committed = commit_on(f.store, f.repository, local, seeded).await;

    let (status, events) =
        push_branch_call(&f, f.store, graph_globals(), local, Default::default()).await;

    assert_eq!(status, BranchAlreadyExists::FFI_CODE);
    assert_eq!(
        pushed(&events),
        [Pushed {
            revision_hash: Hash::default(),
            revision_number: 0,
            fast_forward_merged: false,
            remote_latest: Hash::default(),
            local_latest: committed,
        }]
    );
    assert_eq!(server_latest(&f, remote).await, seeded);
    f.close().await;
}
