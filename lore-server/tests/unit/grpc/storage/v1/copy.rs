// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::RepositoryId;
use lore_proto::lore::storage::v1 as storage_v1;
use lore_server::auth::jwt::AuthorizationToken;
use lore_server::auth::jwt::ResourcePermission;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::AuthClientAuthorizer;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::VerifiedTokenOwned;
use lore_server::grpc::storage::v1::copy::*;
use lore_storage::ImmutableStore;
use rand::random;
use tonic::Code;
use zerocopy::IntoBytes;

use crate::store::test_support::SlowDownImmutableStore;
use crate::store::test_support::test_store_create;

fn copy_request(source_repository: RepositoryId) -> storage_v1::CopyRequest {
    storage_v1::CopyRequest {
        source_repository_id: source_repository.as_bytes().to_vec().into(),
        source_address: Some(lore_proto::lore::model::v1::Address {
            hash: vec![0u8; 32].into(),
            context: vec![0u8; 16].into(),
        }),
        target_context: Vec::new().into(),
    }
}

/// Both halves of a verified access token whose `resources` claim grants
/// exactly `resource_ids`.
fn access_token(resource_ids: &[String]) -> Arc<VerifiedTokenOwned> {
    Arc::new(VerifiedTokenOwned {
        raw: "raw.jwt".to_string(),
        claims: AuthorizationToken {
            resources: Some(
                resource_ids
                    .iter()
                    .map(|resource_id| ResourcePermission {
                        resource_id: resource_id.clone(),
                        permission: vec![],
                    })
                    .collect(),
            ),
            ..Default::default()
        },
    })
}

/// Copies one item and returns its status code. `store` replaces the real store the copy runs
/// against when set.
async fn item_code(
    auth_token: Option<Arc<VerifiedTokenOwned>>,
    repository_authorizer: Arc<dyn RepositoryAuthorizer>,
    source_repository: RepositoryId,
    destination_repository: RepositoryId,
    store: Option<Arc<dyn ImmutableStore>>,
) -> i32 {
    let (real_store, _mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    let store = store.unwrap_or(real_store);
    LORE_CONTEXT
        .scope(execution, async move {
            let response = copy_item(
                Ok(copy_request(source_repository)),
                destination_repository,
                auth_token,
                repository_authorizer,
                "correlation".to_string(),
                "user".to_string(),
                store,
            )
            .await
            .expect("per-item outcomes travel in-band");
            response.status.expect("every item reports a status").code as i32
        })
        .await
}

/// The cross-partition shape on the legacy tier: the access token's
/// `resources` claim holds the destination but not the source, and the
/// claim is answered in place — the URL points nowhere, so reaching for
/// the network would error instead of denying.
#[tokio::test]
async fn source_without_a_grant_is_denied_in_band() {
    let source = random::<RepositoryId>();
    let destination = random::<RepositoryId>();
    let authorizer: Arc<dyn RepositoryAuthorizer> = Arc::new(AuthClientAuthorizer::new(
        "https://auth.invalid".to_string(),
    ));
    let code = item_code(
        Some(access_token(&[format!("urc-{destination}")])),
        authorizer,
        source,
        destination,
        None,
    )
    .await;
    assert_eq!(code, Code::PermissionDenied as i32);
}

/// The same claim with the source granted passes the check and reaches
/// the store, which answers `NOT_FOUND` for the absent address — so the
/// denial above is the source check, not the missing fragment.
#[tokio::test]
async fn granted_source_reaches_the_store() {
    let source = random::<RepositoryId>();
    let destination = random::<RepositoryId>();
    let authorizer: Arc<dyn RepositoryAuthorizer> = Arc::new(AuthClientAuthorizer::new(
        "https://auth.invalid".to_string(),
    ));
    let code = item_code(
        Some(access_token(&[
            format!("urc-{destination}"),
            format!("urc-{source}"),
        ])),
        authorizer,
        source,
        destination,
        None,
    )
    .await;
    assert_eq!(code, Code::NotFound as i32);
}

/// An in-partition item — the dedup hot path — never asks the
/// authorizer: the partition-access layer already answered for the
/// destination. Proven with a token granting nothing at all.
#[tokio::test]
async fn in_partition_item_skips_the_authorizer() {
    let repository = random::<RepositoryId>();
    let authorizer: Arc<dyn RepositoryAuthorizer> = Arc::new(AuthClientAuthorizer::new(
        "https://auth.invalid".to_string(),
    ));
    let code = item_code(
        Some(access_token(&[])),
        authorizer,
        repository,
        repository,
        None,
    )
    .await;
    assert_eq!(code, Code::NotFound as i32);
}

/// No `[server.auth]`: no interceptor ran, so no token — the allow-all
/// authorizer keeps cross-partition copy open exactly as today.
#[tokio::test]
async fn tokenless_caller_stays_open_under_allow_all() {
    let code = item_code(
        None,
        Arc::new(AllowAllRepositoryAuthorizer),
        random::<RepositoryId>(),
        random::<RepositoryId>(),
        None,
    )
    .await;
    assert_eq!(code, Code::NotFound as i32);
}

/// When the store rejects a copy with a slow-down error, the item reports `RESOURCE_EXHAUSTED`
/// in-band.
#[tokio::test]
async fn throttled_store_reports_resource_exhausted_in_band() {
    let repository = random::<RepositoryId>();
    let code = item_code(
        None,
        Arc::new(AllowAllRepositoryAuthorizer),
        repository,
        repository,
        Some(Arc::new(SlowDownImmutableStore)),
    )
    .await;
    assert_eq!(code, Code::ResourceExhausted as i32);
}
