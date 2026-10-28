// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_proto::lore::storage::v1 as storage_v1;
use lore_revision::fragment::generate_random;
use lore_server::authnz::repository_authorizer::Grants;
use lore_server::authnz::repository_authorizer::PartitionGrants;
use lore_server::grpc::storage::v1::verify::handler;
use rand::random;
use tonic::Code;

use crate::grpc::storage::v1::test_utils::make_request_with_metadata;
use crate::store::test_support::test_store_create;

/// Healing changes the server's store, so it needs `write`; a plain verify
/// is a read that any grant permits.
#[tokio::test]
async fn heal_needs_write_and_a_plain_verify_does_not() {
    let (immutable_store, _mutable_store, execution) =
        test_store_create().await.expect("Failed to create store");
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = random::<Context>();
            let (_, address, _) = generate_random();
            let authorizer = crate::util::test_support::allow_all_authorizer();
            let verify = |heal: bool, actions: &[&str]| {
                let mut request = make_request_with_metadata(
                    storage_v1::VerifyRequest {
                        address: Some(address.into()),
                        heal,
                    },
                    repository,
                    "",
                );
                request.extensions_mut().insert(PartitionGrants {
                    repository_id: repository.into(),
                    grants: Grants::Actions(actions.iter().map(ToString::to_string).collect()),
                });
                handler(request, &authorizer, immutable_store.clone())
            };

            let denied = verify(true, &["read"]).await.expect_err("heal needs write");
            assert_eq!(denied.code(), Code::PermissionDenied);
            for (heal, actions) in [(false, &["read"][..]), (true, &["write"][..])] {
                if let Err(status) = verify(heal, actions).await {
                    assert_ne!(
                        status.code(),
                        Code::PermissionDenied,
                        "heal {heal} with {actions:?}"
                    );
                }
            }
        })
        .await;
}
