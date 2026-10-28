// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Changing branch protection is repository administration: `owner` or
//! `admin`, or `write` for the repository's creator, through the protect
//! RPCs and through `BranchMetadataSet` alike.
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_proto::BranchProtectRequest;
use lore_proto::BranchUnprotectRequest;
use lore_revision::branch;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::metadata::Metadata;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryMetadata;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::grpc::get_write_token;
use lore_server::grpc::handlers::branch_metadata_set;
use lore_server::grpc::handlers::branch_protect;
use lore_server::grpc::handlers::branch_unprotect;
use lore_server::grpc::revision::v1::branch_metadata_set as branch_metadata_set_v1;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use rand::random;
use tonic::Code;
use tonic::Request;
use tonic::Status;

use crate::grpc::push_protected::TIERS;
use crate::grpc::push_protected::Tier;
use crate::grpc::push_protected::allow_all;
use crate::store::test_support::test_store_create;

/// The identity `Tier::with_token` puts in its tokens.
const CALLER: &str = "pusher";

struct Fixture {
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    repository: RepositoryId,
    branch: BranchId,
}

async fn seed(
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    creator: &str,
) -> Fixture {
    let id = random::<RepositoryId>();
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store.clone(),
        mutable_store.clone(),
        id,
    ));
    let metadata_hash = repository::metadata_store(
        repository.clone(),
        RepositoryMetadata {
            name: "the-repository".to_string(),
            creator: creator.to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("Failed to store repository metadata");
    repository::metadata_store_hash(repository.clone(), metadata_hash)
        .await
        .expect("Failed to store repository metadata hash");
    repository::store_name_to_id(repository.clone(), "the-repository", id)
        .await
        .expect("Failed to store repository name to id mapping");

    let branch = BranchId::from(uuid::Uuid::now_v7());
    branch::create(
        repository,
        &get_write_token(),
        branch,
        "test-branch",
        branch::default_category(),
        creator,
        1,
        vec![],
        false,
        false,
    )
    .await
    .expect("create branch");

    Fixture {
        immutable_store,
        mutable_store,
        repository: id,
        branch,
    }
}

/// A request naming `repository`, carrying `tier`'s token holding `actions`,
/// or no token when `tier` is `None`.
fn with_token<T>(
    tier: Option<Tier>,
    message: T,
    repository: RepositoryId,
    actions: &[&str],
) -> Request<T> {
    let mut request = Request::new(message);
    request.metadata_mut().insert_bin(
        REPOSITORY_ID_KEY,
        tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
    );
    match tier {
        Some(tier) => tier.with_token(request, repository, actions),
        None => request,
    }
}

/// Protects then unprotects the fixture's branch, as the caller `tier`'s
/// token describes, or with no token at all when `tier` is `None`.
async fn protect_and_unprotect(
    creator: &'static str,
    tier: Option<Tier>,
    actions: &'static [&'static str],
) -> Result<(), Status> {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let fixture = seed(immutable_store, mutable_store, creator).await;
        let authorizer: Arc<dyn RepositoryAuthorizer> =
            tier.map_or_else(allow_all, Tier::authorizer);

        branch_protect::handler(
            with_token(
                tier,
                BranchProtectRequest {
                    branch: fixture.branch.into(),
                },
                fixture.repository,
                actions,
            ),
            &authorizer,
            fixture.immutable_store.clone(),
            fixture.mutable_store.clone(),
        )
        .await?;
        branch_unprotect::handler(
            with_token(
                tier,
                BranchUnprotectRequest {
                    branch: fixture.branch.into(),
                },
                fixture.repository,
                actions,
            ),
            &authorizer,
            fixture.immutable_store,
            fixture.mutable_store,
        )
        .await?;
        Ok(())
    }))
    .await
}

#[tokio::test]
async fn owner_or_admin_administers_a_repository_they_did_not_create() {
    for tier in TIERS {
        for actions in [&["owner"][..], &["admin"][..]] {
            protect_and_unprotect("alice", Some(tier), actions)
                .await
                .unwrap_or_else(|err| panic!("{tier:?} {actions:?}: {err}"));
        }
    }
}

#[tokio::test]
async fn the_creator_needs_write() {
    for tier in TIERS {
        protect_and_unprotect(CALLER, Some(tier), &["write"])
            .await
            .unwrap_or_else(|err| panic!("{tier:?}: {err}"));
    }
}

#[tokio::test]
async fn write_does_not_let_a_non_creator_protect() {
    for tier in TIERS {
        let err = protect_and_unprotect("alice", Some(tier), &["read", "write"])
            .await
            .expect_err("a non-creator without owner or admin is refused");
        assert_eq!(err.code(), Code::PermissionDenied, "{tier:?}");
    }
}

/// No verifier: the client's identity on create is not the `<unknown>` the
/// server sees, and `write` from allow-all decides alone.
#[tokio::test]
async fn without_a_verifier_any_caller_with_write_protects() {
    protect_and_unprotect("alice", None, &[]).await.unwrap();
}

#[derive(Clone, Copy, Debug)]
enum Change {
    Protection,
    OtherKey,
}

/// Sets the fixture branch's metadata through the v0 and then the v1
/// `BranchMetadataSet`, as the caller `tier`'s token holding `actions`.
async fn set_metadata(
    creator: &'static str,
    tier: Tier,
    actions: &'static [&'static str],
    change: Change,
) -> Result<(), Status> {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let fixture = seed(immutable_store, mutable_store, creator).await;
        let authorizer = tier.authorizer();
        let repository = Arc::new(RepositoryContext::new_server_context(
            fixture.immutable_store.clone(),
            fixture.mutable_store.clone(),
            fixture.repository,
        ));

        for v1 in [false, true] {
            let current = branch::metadata_hash(repository.clone(), fixture.branch)
                .await
                .expect("metadata hash");
            let mut proposed = Metadata::deserialize(repository.clone(), current)
                .await
                .expect("deserialize current");
            match change {
                Change::Protection => {
                    let protected = proposed.get_bool(branch::PROTECT).unwrap_or_default();
                    proposed.set_bool(branch::PROTECT, !protected)
                }
                Change::OtherKey => proposed.set_string("custom-key", if v1 { "v1" } else { "v0" }),
            }
            .expect("set key");
            let updated = proposed
                .serialize(repository.clone())
                .await
                .expect("serialize proposed");

            if v1 {
                branch_metadata_set_v1::handler(
                    with_token(
                        Some(tier),
                        lore_proto::lore::revision::v1::BranchMetadataSetRequest {
                            id: fixture.branch.into(),
                            expected: current.into(),
                            updated: updated.into(),
                        },
                        fixture.repository,
                        actions,
                    ),
                    &authorizer,
                    fixture.immutable_store.clone(),
                    fixture.mutable_store.clone(),
                )
                .await?;
            } else {
                branch_metadata_set::handler(
                    with_token(
                        Some(tier),
                        lore_proto::BranchMetadataSetRequest {
                            branch_id: fixture.branch.into(),
                            expected_hash: current.into(),
                            new_hash: updated.into(),
                        },
                        fixture.repository,
                        actions,
                    ),
                    &authorizer,
                    fixture.immutable_store.clone(),
                    fixture.mutable_store.clone(),
                )
                .await?;
            }
        }
        Ok(())
    }))
    .await
}

#[tokio::test]
async fn write_does_not_let_a_non_creator_change_protection_through_metadata_set() {
    for tier in TIERS {
        let err = set_metadata("alice", tier, &["read", "write"], Change::Protection)
            .await
            .expect_err("a protection change is repository administration");
        assert_eq!(err.code(), Code::PermissionDenied, "{tier:?}");
    }
}

#[tokio::test]
async fn write_still_sets_other_metadata_keys() {
    for tier in TIERS {
        set_metadata("alice", tier, &["write"], Change::OtherKey)
            .await
            .unwrap_or_else(|err| panic!("{tier:?}: {err}"));
    }
}

/// Metadata set is a write in any case, since one call can change any key;
/// a protection change additionally needs `owner`, `admin` or creatorship.
#[tokio::test]
async fn owner_or_the_creator_with_write_changes_protection_through_metadata_set() {
    for tier in TIERS {
        set_metadata("alice", tier, &["write", "owner"], Change::Protection)
            .await
            .unwrap_or_else(|err| panic!("{tier:?} owner: {err}"));
        set_metadata(CALLER, tier, &["write"], Change::Protection)
            .await
            .unwrap_or_else(|err| panic!("{tier:?} creator: {err}"));
    }
}

/// A repository whose metadata cannot be read: an owner still protects,
/// since the creator is read only when the decision rests on it, while a
/// writer on the creator path gets `NotFound`.
#[tokio::test]
async fn the_creator_is_read_only_when_the_decision_rests_on_it() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    Box::pin(LORE_CONTEXT.scope(execution, async move {
        let id = random::<RepositoryId>();
        let repository = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            id,
        ));
        let branch = BranchId::from(uuid::Uuid::now_v7());
        branch::create(
            repository,
            &get_write_token(),
            branch,
            "test-branch",
            branch::default_category(),
            CALLER,
            1,
            vec![],
            false,
            false,
        )
        .await
        .expect("create branch");
        let tier = Tier::Resource;
        let authorizer = tier.authorizer();
        let protect = |actions: &'static [&'static str]| {
            branch_protect::handler(
                with_token(
                    Some(tier),
                    BranchProtectRequest {
                        branch: branch.into(),
                    },
                    id,
                    actions,
                ),
                &authorizer,
                immutable_store.clone(),
                mutable_store.clone(),
            )
        };

        protect(&["owner"]).await.expect("owner needs no creator");
        let err = protect(&["write"])
            .await
            .expect_err("the creator path needs the metadata");
        assert_eq!(err.code(), Code::NotFound);
    }))
    .await;
}
