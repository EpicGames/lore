// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_proto::BranchListRequest;
use lore_proto::BranchListResponse;
use lore_revision::branch;
use lore_revision::branch::BranchLatestStatus;
use lore_revision::repository::RepositoryContext;
use lore_server::grpc::get_write_token;
use lore_server::grpc::handlers::branch_list::*;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use rand::random;
use tonic::Request;

use crate::store::test_support::test_store_create;

#[tokio::test]
async fn test_handle() {
    let repository = random::<Context>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution.clone(), async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository.into(),
            ));
            let write_token = get_write_token();
            // Create the main branch (without parent)
            let no_parent = Context::default(); // Zero, no parent
            let payload_main = random::<[u8; size_of::<Hash>()]>().to_vec();
            let hash_main = Hash::hash_buffer(&payload_main);
            let main = lore_revision::branch::create(
                repository.clone(),
                &write_token,
                Context::from(uuid::Uuid::now_v7()),
                lore_revision::branch::DEFAULT_DEFAULT_NAME,
                lore_revision::branch::default_category(),
                "MainCreator",
                1234,
                vec![],
                false,
                false,
            )
            .await
            .expect("Could not create main branch");

            let mut request = Request::new(BranchListRequest {});
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
            );
            let response = handler(request, immutable_store.clone(), mutable_store.clone())
                .await
                .expect("Failed BranchList message handle");
            assert_eq!(
                BranchListResponse {
                    branches: [lore_proto::Branch {
                        id: main.into(),
                        name: lore_revision::branch::DEFAULT_DEFAULT_NAME.to_string(),
                        category: lore_revision::branch::default_category().to_string(),
                        parent_deprecated: Some(Context::default().into()),
                        latest: Hash::default().into(),
                        branch_point_deprecated: Some(Hash::default().into()),
                        creator: "MainCreator".to_string(),
                        created: 1234,
                        stack: vec![],
                    }]
                    .to_vec()
                },
                response.into_inner()
            );

            // Create another branch1
            let payload_branch1 = random::<[u8; size_of::<Hash>()]>().to_vec();
            let hash_branch1 = Hash::hash_buffer(&payload_branch1);
            let branch1 = lore_revision::branch::create(
                repository.clone(),
                &write_token,
                Context::from(uuid::Uuid::now_v7()),
                "branch1",
                lore_revision::branch::default_category(),
                "TestCreator",
                4321,
                vec![],
                false,
                false,
            )
            .await
            .expect("Could not create branch1 branch");

            // Update main branch
            branch::store_latest(
                repository.clone(),
                main,
                Hash::default(),
                hash_main,
                BranchLatestStatus::Convergent,
            )
            .await
            .expect("Failed to store latest");

            // Update branch1 branch
            branch::store_latest(
                repository.clone(),
                branch1,
                Hash::default(),
                hash_branch1,
                BranchLatestStatus::Convergent,
            )
            .await
            .expect("Failed to store latest");

            let mut request = Request::new(BranchListRequest {});
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
            );
            let response = handler(request, immutable_store.clone(), mutable_store.clone())
                .await
                .expect("Failed BranchList message handle")
                .into_inner();

            assert!(response.branches.contains(&lore_proto::Branch {
                id: main.into(),
                name: lore_revision::branch::DEFAULT_DEFAULT_NAME.to_string(),
                category: lore_revision::branch::default_category().to_string(),
                parent_deprecated: Some(no_parent.into()),
                latest: hash_main.into(),
                branch_point_deprecated: Some(Hash::default().into()),
                creator: "MainCreator".to_string(),
                created: 1234,
                stack: vec![]
            }));

            assert!(response.branches.contains(&lore_proto::Branch {
                id: branch1.into(),
                name: "branch1".to_string(),
                category: lore_revision::branch::default_category().to_string(),
                parent_deprecated: Some(Context::default().into()),
                latest: hash_branch1.into(),
                branch_point_deprecated: Some(Hash::default().into()),
                creator: "TestCreator".to_string(),
                created: 4321,
                stack: vec![]
            }));
        })
        .await;
}

#[tokio::test]
async fn test_handle_no_branches() {
    let repository = random::<Context>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution.clone(), async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository.into(),
            ));
            let mut request = Request::new(BranchListRequest {});
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
            );
            let response = handler(request, immutable_store.clone(), mutable_store.clone())
                .await
                .expect("Failed BranchList message handle");
            assert_eq!(
                BranchListResponse { branches: vec![] },
                response.into_inner()
            );
        })
        .await;
}

/// How the name of a repository's default branch is remapped before a listing repairs it.
#[derive(Clone, Copy, Debug)]
enum Remap {
    /// To another branch with metadata.
    ToLiveBranch,
    /// To an id without metadata.
    ToIdWithoutMetadata,
    /// To none, as an archive leaves it.
    Released,
}

/// What the default branch's name maps to after a listing, where `remap` remapped it before the
/// listing. Answers the default branch, the id `remap` mapped the name to, and the mapping after.
async fn default_name_after_a_listing(remap: Remap) -> (Context, Context, Option<Context>) {
    let repository = random::<Context>();
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    Box::pin(LORE_CONTEXT.scope(execution.clone(), async move {
        let repository = Arc::new(RepositoryContext::new_server_context(
            immutable_store.clone(),
            mutable_store.clone(),
            repository.into(),
        ));
        let write_token = get_write_token();
        let create = |name: &'static str| {
            branch::create(
                repository.clone(),
                &write_token,
                Context::from(uuid::Uuid::now_v7()),
                name,
                branch::default_category(),
                "creator",
                1,
                vec![],
                false,
                false,
            )
        };
        let main = create(branch::DEFAULT_DEFAULT_NAME)
            .await
            .expect("Could not create main branch");
        let metadata_hash = lore_revision::repository::metadata_store(
            repository.clone(),
            lore_revision::repository::RepositoryMetadata {
                default_branch: main,
                default_branch_name: branch::DEFAULT_DEFAULT_NAME.to_string(),
                ..Default::default()
            },
        )
        .await
        .expect("Failed to store repository metadata");
        lore_revision::repository::metadata_store_hash(repository.clone(), metadata_hash)
            .await
            .expect("Failed to store repository metadata hash");
        let remapped = match remap {
            Remap::ToLiveBranch => create("elsewhere")
                .await
                .expect("Could not create another branch"),
            Remap::ToIdWithoutMetadata => Context::from(uuid::Uuid::now_v7()),
            Remap::Released => Context::default(),
        };
        if remapped.is_zero() {
            branch::release_name(repository.clone(), branch::DEFAULT_DEFAULT_NAME, main).await
        } else {
            branch::claim_name(
                repository.clone(),
                branch::DEFAULT_DEFAULT_NAME,
                main,
                remapped,
            )
            .await
        }
        .expect("the default branch's name is remapped");

        let mut request = Request::new(BranchListRequest {});
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.id.data()),
        );
        handler(request, immutable_store.clone(), mutable_store.clone())
            .await
            .expect("Failed BranchList message handle");

        let after = branch::load_name_to_id_local(repository.clone(), branch::DEFAULT_DEFAULT_NAME)
            .await
            .ok();
        (main, remapped, after)
    }))
    .await
}

/// A listing that misses the default branch because another live branch holds its name leaves the
/// name with that branch.
#[tokio::test]
async fn default_branch_repair_skips_a_claimed_name() {
    let (_main, holder, after) = default_name_after_a_listing(Remap::ToLiveBranch).await;
    assert_eq!(after, Some(holder));
}

/// A listing that misses the default branch because its name maps to an id without metadata, or
/// to none, maps the name back to the default branch.
#[tokio::test]
async fn default_branch_repair_replaces_a_stale_mapping() {
    for remap in [Remap::ToIdWithoutMetadata, Remap::Released] {
        let (main, _remapped, after) = default_name_after_a_listing(remap).await;
        assert_eq!(after, Some(main), "{remap:?}");
    }
}
