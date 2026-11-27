// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The scope records of a repository: their lifecycle writes, reads and refusals, and the
//! default scope.

mod races;

use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::branch::record;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_revision::scope;
use lore_revision::scope::Refused;
use lore_revision::scope::Scope;
use lore_revision::scope::ScopeError;
use lore_revision::scope::ScopeId;
use lore_revision::scope::Scopes;
use lore_storage::StoreError;

use crate::branch::hooked_repository;
use crate::branch::null_repository;
use crate::helper::setup_test_execution;

/// The scope id made of `byte`.
fn scope_id(byte: u8) -> ScopeId {
    ScopeId::from([byte; 16])
}

/// Runs `body` with the execution context a server request has.
async fn scoped<F: Future>(body: F) -> F::Output {
    Box::pin(LORE_CONTEXT.scope(setup_test_execution(), body)).await
}

/// Creates scope `byte` named `name` in `repository`, by `creator` at 7. A create the scope's
/// state refuses panics.
async fn create(
    repository: &Arc<RepositoryContext>,
    byte: u8,
    name: &str,
) -> Result<Scope, ScopeError> {
    scope::create(repository.clone(), scope_id(byte), name, "creator", 7)
        .await
        .map(|created| created.expect("the create is not refused"))
}

/// The live scope `name` maps to in `repository`.
async fn by_name(repository: &Arc<RepositoryContext>, name: &str) -> Option<Scope> {
    scope::get_by_name(repository.clone(), name)
        .await
        .expect("reading a scope by name")
}

/// Scope `byte` of `repository`.
async fn by_id(repository: &Arc<RepositoryContext>, byte: u8) -> Option<Scope> {
    scope::get(repository.clone(), scope_id(byte))
        .await
        .expect("reading a scope by id")
}

/// Whether scope `byte` of `repository` is live.
async fn is_live(repository: &Arc<RepositoryContext>, byte: u8) -> bool {
    scope::is_live(repository.clone(), scope_id(byte))
        .await
        .expect("reading whether a scope is live")
}

/// The live scope `byte` named `name`, as a create by `creator` at 7 makes it.
fn live(byte: u8, name: &str) -> Scope {
    Scope {
        id: scope_id(byte),
        name: name.to_string(),
        creator: "creator".to_string(),
        created: 7,
        archived: false,
    }
}

/// [`live`] scope `byte`, archived.
fn archived(byte: u8) -> Scope {
    Scope {
        name: String::new(),
        archived: true,
        ..live(byte, "")
    }
}

#[tokio::test]
async fn a_created_scope_reads_by_id_and_by_name_regardless_of_case() {
    scoped(async {
        let repository = null_repository().await;
        assert_eq!(
            create(&repository, 1, "Release").await.unwrap(),
            live(1, "Release")
        );
        assert_eq!(by_id(&repository, 1).await, Some(live(1, "Release")));
        assert_eq!(
            by_name(&repository, "release").await,
            Some(live(1, "Release"))
        );
        assert_eq!(by_name(&repository, "other").await, None);
        assert_eq!(by_id(&repository, 2).await, None);
    })
    .await;
}

#[tokio::test]
async fn a_create_repeated_with_the_same_request_answers_the_scope() {
    scoped(async {
        let repository = null_repository().await;
        create(&repository, 1, "release").await.unwrap();
        assert_eq!(
            create(&repository, 1, "release").await.unwrap(),
            live(1, "release")
        );
        assert_eq!(
            by_name(&repository, "release").await,
            Some(live(1, "release"))
        );
    })
    .await;
}

#[tokio::test]
async fn a_create_under_an_existing_id_with_another_request_is_refused() {
    scoped(async {
        let repository = null_repository().await;
        create(&repository, 1, "release").await.unwrap();
        for name in ["other", "Release"] {
            let err = create(&repository, 1, name).await.unwrap_err();
            assert!(err.is_already_exists(), "{name}: {err:?}");
        }
        assert_eq!(by_id(&repository, 1).await, Some(live(1, "release")));
    })
    .await;
}

/// A create of an archived scope's id is refused as archived under any name, its old one
/// included, and writes nothing.
#[tokio::test]
async fn a_create_of_an_archived_scope_is_refused_and_writes_nothing() {
    scoped(async {
        let repository = null_repository().await;
        create(&repository, 1, "release").await.unwrap();
        scope::archive(repository.clone(), scope_id(1))
            .await
            .unwrap()
            .unwrap();
        let metadata = record::metadata_key::<Scopes>(repository.clone(), scope_id(1))
            .await
            .unwrap();
        for name in ["release", "other"] {
            assert_eq!(
                scope::create(repository.clone(), scope_id(1), name, "creator", 7)
                    .await
                    .unwrap(),
                Err(Refused::Archived),
                "{name}"
            );
            assert!(
                record::name_mapping::<Scopes>(repository.clone(), name)
                    .await
                    .unwrap()
                    .is_zero(),
                "{name}"
            );
        }
        assert_eq!(
            record::metadata_key::<Scopes>(repository.clone(), scope_id(1))
                .await
                .unwrap(),
            metadata
        );
    })
    .await;
}

#[tokio::test]
async fn a_create_of_a_taken_name_is_refused_in_any_case() {
    scoped(async {
        let repository = null_repository().await;
        create(&repository, 1, "release").await.unwrap();
        let err = create(&repository, 2, "RELEASE").await.unwrap_err();
        assert!(err.is_already_exists(), "{err:?}");
        assert_eq!(by_id(&repository, 2).await, None);
    })
    .await;
}

#[tokio::test]
async fn names_follow_the_branch_name_rules_and_reserve_default() {
    scoped(async {
        let repository = null_repository().await;
        let id_like = scope_id(9).to_string();
        let too_long = "n".repeat(lore_revision::branch::MAX_NAME_LEN + 1);
        for name in ["", "default", "Default", "DEFAULT", &id_like, &too_long] {
            let err = create(&repository, 1, name).await.unwrap_err();
            assert!(err.is_invalid_arguments(), "{name:?}: {err:?}");
        }
        create(&repository, 1, "fine").await.unwrap();
        for name in ["", "default", "DeFault", &id_like] {
            let err = scope::rename(repository.clone(), scope_id(1), name)
                .await
                .unwrap_err();
            assert!(err.is_invalid_arguments(), "{name:?}: {err:?}");
        }
        scope::archive(repository.clone(), scope_id(1))
            .await
            .unwrap()
            .unwrap();
        let err = scope::restore(repository.clone(), scope_id(1), "default")
            .await
            .unwrap_err();
        assert!(err.is_invalid_arguments(), "{err:?}");
    })
    .await;
}

#[tokio::test]
async fn a_zero_id_or_the_default_scopes_id_is_not_created() {
    scoped(async {
        let repository = null_repository().await;
        for id in [ScopeId::default(), scope::default_scope_id(repository.id)] {
            let err = scope::create(repository.clone(), id, "release", "creator", 7)
                .await
                .unwrap_err();
            assert!(err.is_invalid_arguments(), "{id}: {err:?}");
        }
    })
    .await;
}

/// The default scope is answered by its id and by its name, in any case, from a store that fails
/// every call: it has no stored state.
#[tokio::test]
async fn the_default_scope_is_answered_without_a_store_read() {
    scoped(async {
        let repository =
            hooked_repository(|_, _, _| Err(StoreError::internal("the store is not read"))).await;
        let default = scope::default_scope(repository.id);
        assert_eq!(
            scope::get(repository.clone(), default.id).await.unwrap(),
            Some(default.clone())
        );
        assert!(
            scope::is_live(repository.clone(), default.id)
                .await
                .unwrap()
        );
        for name in ["default", "DEFAULT"] {
            assert_eq!(
                scope::get_by_name(repository.clone(), name).await.unwrap(),
                Some(default.clone()),
                "{name}"
            );
        }
        assert!(
            scope::get(repository.clone(), scope_id(1)).await.is_err(),
            "a stored scope is read from the store"
        );
    })
    .await;
}

#[tokio::test]
async fn repositories_have_default_scopes_of_their_own() {
    assert_ne!(
        scope::default_scope_id(RepositoryId::from([3; 16])),
        scope::default_scope_id(RepositoryId::from([4; 16]))
    );
}

#[tokio::test]
async fn the_default_scope_takes_no_lifecycle_write() {
    scoped(async {
        let repository = null_repository().await;
        let default = scope::default_scope_id(repository.id);
        assert_eq!(
            scope::rename(repository.clone(), default, "other")
                .await
                .unwrap(),
            Err(Refused::Default)
        );
        assert_eq!(
            scope::archive(repository.clone(), default).await.unwrap(),
            Err(Refused::Default)
        );
        assert_eq!(
            scope::restore(repository.clone(), default, "other")
                .await
                .unwrap(),
            Err(Refused::Default)
        );
        assert_eq!(by_name(&repository, "other").await, None);
    })
    .await;
}

#[tokio::test]
async fn a_rename_moves_the_name_and_frees_the_old_one() {
    scoped(async {
        let repository = null_repository().await;
        create(&repository, 1, "release").await.unwrap();
        assert_eq!(
            scope::rename(repository.clone(), scope_id(1), "shipped")
                .await
                .unwrap(),
            Ok(live(1, "shipped"))
        );
        assert_eq!(by_id(&repository, 1).await, Some(live(1, "shipped")));
        assert_eq!(
            by_name(&repository, "shipped").await,
            Some(live(1, "shipped"))
        );
        assert_eq!(by_name(&repository, "release").await, None);
        assert_eq!(
            create(&repository, 2, "release").await.unwrap(),
            live(2, "release")
        );
    })
    .await;
}

#[tokio::test]
async fn a_rename_to_the_same_name_or_its_case_keeps_the_mapping() {
    scoped(async {
        let repository = null_repository().await;
        create(&repository, 1, "release").await.unwrap();
        assert_eq!(
            scope::rename(repository.clone(), scope_id(1), "release")
                .await
                .unwrap(),
            Ok(live(1, "release"))
        );
        assert_eq!(
            scope::rename(repository.clone(), scope_id(1), "Release")
                .await
                .unwrap(),
            Ok(live(1, "Release"))
        );
        assert_eq!(by_id(&repository, 1).await, Some(live(1, "Release")));
        assert_eq!(
            by_name(&repository, "RELEASE").await,
            Some(live(1, "Release"))
        );
    })
    .await;
}

#[tokio::test]
async fn a_rename_to_a_taken_name_is_refused_and_changes_nothing() {
    scoped(async {
        let repository = null_repository().await;
        create(&repository, 1, "release").await.unwrap();
        create(&repository, 2, "hotfix").await.unwrap();
        let err = scope::rename(repository.clone(), scope_id(1), "HOTFIX")
            .await
            .unwrap_err();
        assert!(err.is_already_exists(), "{err:?}");
        assert_eq!(by_id(&repository, 1).await, Some(live(1, "release")));
        assert_eq!(
            by_name(&repository, "release").await,
            Some(live(1, "release"))
        );
        assert_eq!(
            by_name(&repository, "hotfix").await,
            Some(live(2, "hotfix"))
        );
    })
    .await;
}

#[tokio::test]
async fn an_archive_removes_the_name_and_keeps_the_record() {
    scoped(async {
        let repository = null_repository().await;
        create(&repository, 1, "release").await.unwrap();
        assert_eq!(
            scope::archive(repository.clone(), scope_id(1))
                .await
                .unwrap(),
            Ok(archived(1))
        );
        assert_eq!(by_id(&repository, 1).await, Some(archived(1)));
        assert_eq!(by_name(&repository, "release").await, None);
        assert_eq!(
            scope::archive(repository.clone(), scope_id(1))
                .await
                .unwrap(),
            Ok(archived(1))
        );
        assert_eq!(
            create(&repository, 2, "release").await.unwrap(),
            live(2, "release")
        );
    })
    .await;
}

/// A scope is live from its create until its archive, and a scope without metadata isn't.
#[tokio::test]
async fn a_scope_is_live_until_it_is_archived() {
    scoped(async {
        let repository = null_repository().await;
        assert!(!is_live(&repository, 1).await, "a scope without metadata");
        create(&repository, 1, "release").await.unwrap();
        assert!(is_live(&repository, 1).await, "a created scope");
        scope::archive(repository.clone(), scope_id(1))
            .await
            .unwrap()
            .unwrap();
        assert!(!is_live(&repository, 1).await, "an archived scope");
    })
    .await;
}

#[tokio::test]
async fn an_archived_scope_is_restored_under_a_name_and_not_renamed() {
    scoped(async {
        let repository = null_repository().await;
        create(&repository, 1, "release").await.unwrap();
        scope::archive(repository.clone(), scope_id(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            scope::rename(repository.clone(), scope_id(1), "other")
                .await
                .unwrap(),
            Err(Refused::Archived)
        );
        assert_eq!(by_name(&repository, "other").await, None);
        assert_eq!(
            scope::restore(repository.clone(), scope_id(1), "revived")
                .await
                .unwrap(),
            Ok(live(1, "revived"))
        );
        assert_eq!(by_id(&repository, 1).await, Some(live(1, "revived")));
        assert_eq!(
            by_name(&repository, "revived").await,
            Some(live(1, "revived"))
        );
        assert_eq!(by_name(&repository, "release").await, None);
    })
    .await;
}

/// A restore repeated after it succeeded answers the scope, as a repeated create does, and one
/// under any other name, one that differs only in case included, fails as the scope exists.
#[tokio::test]
async fn a_repeated_restore_answers_the_scope() {
    scoped(async {
        let repository = null_repository().await;
        create(&repository, 1, "release").await.unwrap();
        scope::archive(repository.clone(), scope_id(1))
            .await
            .unwrap()
            .unwrap();
        scope::restore(repository.clone(), scope_id(1), "revived")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            scope::restore(repository.clone(), scope_id(1), "revived")
                .await
                .unwrap(),
            Ok(live(1, "revived"))
        );
        let err = scope::restore(repository.clone(), scope_id(1), "Revived")
            .await
            .unwrap_err();
        assert!(err.is_already_exists(), "{err:?}");
        assert_eq!(by_id(&repository, 1).await, Some(live(1, "revived")));
    })
    .await;
}

#[tokio::test]
async fn a_live_scope_is_not_restored_under_another_name() {
    scoped(async {
        let repository = null_repository().await;
        create(&repository, 1, "release").await.unwrap();
        let err = scope::restore(repository.clone(), scope_id(1), "other")
            .await
            .unwrap_err();
        assert!(err.is_already_exists(), "{err:?}");
        assert_eq!(by_name(&repository, "other").await, None);
        assert_eq!(by_id(&repository, 1).await, Some(live(1, "release")));
    })
    .await;
}

#[tokio::test]
async fn a_restore_under_a_taken_name_is_refused_and_leaves_the_scope_archived() {
    scoped(async {
        let repository = null_repository().await;
        create(&repository, 1, "release").await.unwrap();
        scope::archive(repository.clone(), scope_id(1))
            .await
            .unwrap()
            .unwrap();
        create(&repository, 2, "release").await.unwrap();
        let err = scope::restore(repository.clone(), scope_id(1), "Release")
            .await
            .unwrap_err();
        assert!(err.is_already_exists(), "{err:?}");
        assert_eq!(by_id(&repository, 1).await, Some(archived(1)));
        assert_eq!(
            by_name(&repository, "release").await,
            Some(live(2, "release"))
        );
    })
    .await;
}

#[tokio::test]
async fn a_write_of_a_scope_without_metadata_is_not_found() {
    scoped(async {
        let repository = null_repository().await;
        let missing = scope_id(5);
        let rename = scope::rename(repository.clone(), missing, "other").await;
        let archive = scope::archive(repository.clone(), missing).await;
        let restore = scope::restore(repository.clone(), missing, "other").await;
        assert!(rename.is_err_and(|err| err.is_not_found()));
        assert!(archive.is_err_and(|err| err.is_not_found()));
        assert!(restore.is_err_and(|err| err.is_not_found()));
    })
    .await;
}

/// A rename that stopped before it released the scope's old name leaves the name mapped to the
/// scope: a lookup by it finds none, and it stays taken until the next write of the scope releases
/// it.
#[tokio::test]
async fn an_old_name_a_stopped_rename_left_is_released_by_the_next_write() {
    scoped(async {
        let repository = null_repository().await;
        create(&repository, 1, "release").await.unwrap();
        races::stop_at(
            &repository,
            races::Op::Rename(races::RACED, "shipped"),
            lore_revision::test_util::BranchStep::RenameReleasing,
        )
        .await;
        assert_eq!(by_name(&repository, "release").await, None);
        assert_eq!(
            by_name(&repository, "shipped").await,
            Some(live(1, "shipped"))
        );
        let err = create(&repository, 2, "release").await.unwrap_err();
        assert!(err.is_already_exists(), "{err:?}");
        scope::archive(repository.clone(), scope_id(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            create(&repository, 2, "release").await.unwrap(),
            live(2, "release")
        );
    })
    .await;
}

/// An archive that stopped before it released the scope's name leaves the name mapped to it: a
/// restore under another name releases it, and the name is free.
#[tokio::test]
async fn a_restore_releases_the_name_a_stopped_archive_left() {
    scoped(async {
        let repository = null_repository().await;
        create(&repository, 1, "release").await.unwrap();
        races::stop_at(
            &repository,
            races::Op::Archive(races::RACED),
            lore_revision::test_util::BranchStep::ArchiveReleasing,
        )
        .await;
        assert_eq!(by_id(&repository, 1).await, Some(archived(1)));
        assert_eq!(by_name(&repository, "release").await, None);
        assert_eq!(
            scope::restore(repository.clone(), scope_id(1), "revived")
                .await
                .unwrap(),
            Ok(live(1, "revived"))
        );
        assert_eq!(
            create(&repository, 2, "release").await.unwrap(),
            live(2, "release")
        );
    })
    .await;
}

/// A release of an older claim of a name by a scope leaves a later claim of it by the scope.
#[tokio::test]
async fn a_stale_release_leaves_a_later_claim_of_the_name() {
    scoped(async {
        let repository = null_repository().await;
        create(&repository, 1, "release").await.unwrap();
        for name in ["shipped", "release"] {
            scope::rename(repository.clone(), scope_id(1), name)
                .await
                .unwrap()
                .unwrap();
        }
        assert!(
            !record::release_held::<Scopes>(repository.clone(), scope_id(1), "release", 1)
                .await
                .unwrap()
        );
        assert_eq!(
            by_name(&repository, "release").await,
            Some(live(1, "release"))
        );
    })
    .await;
}

/// A create that stopped after its metadata leaves a scope no name maps to, and a repeated create
/// of it claims the name.
#[tokio::test]
async fn a_repeated_create_claims_the_name_a_stopped_create_left_unclaimed() {
    scoped(async {
        let repository = null_repository().await;
        races::stop_at(
            &repository,
            races::Op::Create(races::RACED, "release"),
            lore_revision::test_util::BranchStep::CreateMetadataStored,
        )
        .await;
        assert_eq!(by_name(&repository, "release").await, None);
        assert_eq!(
            by_id(&repository, races::RACED).await,
            Some(live(1, "release"))
        );
        assert_eq!(
            create(&repository, 1, "release").await.unwrap(),
            live(1, "release")
        );
        assert_eq!(
            by_name(&repository, "release").await,
            Some(live(1, "release"))
        );
    })
    .await;
}

/// A restore under the scope's own name that stopped after its claim, then an archive that settled
/// it and stopped before its release, leave the name marked released: the next archive releases it,
/// and the name is free.
#[tokio::test]
async fn an_archive_releases_the_name_a_settled_restore_under_it_claimed() {
    scoped(async {
        let repository = null_repository().await;
        create(&repository, 1, "release").await.unwrap();
        scope::archive(repository.clone(), scope_id(1))
            .await
            .unwrap()
            .unwrap();
        races::stop_at(
            &repository,
            races::Op::Restore(races::RACED, "release"),
            lore_revision::test_util::BranchStep::RestoreNameClaimed,
        )
        .await;
        races::stop_at(
            &repository,
            races::Op::Archive(races::RACED),
            lore_revision::test_util::BranchStep::PendingRestoreSettled,
        )
        .await;
        assert_eq!(
            scope::archive(repository.clone(), scope_id(1))
                .await
                .unwrap(),
            Ok(archived(1))
        );
        assert_eq!(
            create(&repository, 2, "release").await.unwrap(),
            live(2, "release")
        );
    })
    .await;
}
