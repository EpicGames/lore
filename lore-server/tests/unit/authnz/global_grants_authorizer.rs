// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::types::RepositoryId;
use lore_server::auth::jwt::AuthorizationToken;
use lore_server::authnz::global_grants_authorizer::*;
use lore_server::authnz::repository_authorizer::Action;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::VerifiedToken;
use serde_json::json;
use tonic::Code;
use tonic::Status;

/// `[server.auth].implicit_actions` when absent.
fn default_implicit_actions() -> Vec<Action> {
    vec![Action::Read, Action::Write]
}

fn token_with_extra(extra: serde_json::Value) -> AuthorizationToken {
    let serde_json::Value::Object(extra) = extra else {
        panic!("extra claims must be a JSON object");
    };
    AuthorizationToken {
        user_id: "the u".to_string(),
        extra,
        ..Default::default()
    }
}

async fn check(
    authorizer: &GlobalGrantsAuthorizer,
    claims: &AuthorizationToken,
    repository_id: RepositoryId,
    action: Action,
) -> Result<(), Status> {
    let token = VerifiedToken { raw: "raw", claims };
    authorizer
        .check_repository_access(Some(&token), repository_id, action)
        .await
}

#[tokio::test]
async fn nested_claim_grants_the_action_on_any_partition() {
    let authorizer = GlobalGrantsAuthorizer::new(
        Some("realm_access.roles".to_string()),
        default_implicit_actions(),
    );
    let claims = token_with_extra(json!({
        "realm_access": { "roles": ["obliterate"] }
    }));
    for repository in [RepositoryId::default(), RepositoryId::from([7u8; 16])] {
        check(&authorizer, &claims, repository, Action::Obliterate)
            .await
            .unwrap();
        let err = check(&authorizer, &claims, repository, Action::Admin)
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);
    }
}

#[tokio::test]
async fn flat_claim_grants_the_action_on_any_partition() {
    // Dex emits its grants as a flat `groups` claim.
    let authorizer =
        GlobalGrantsAuthorizer::new(Some("groups".to_string()), default_implicit_actions());
    let claims = AuthorizationToken {
        groups: Some(vec!["obliterate".to_string()]),
        ..Default::default()
    };
    check(
        &authorizer,
        &claims,
        RepositoryId::default(),
        Action::Obliterate,
    )
    .await
    .unwrap();
    let err = check(&authorizer, &claims, RepositoryId::default(), Action::Admin)
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied);
}

#[tokio::test]
async fn no_permission_claim_grants_the_implicit_actions_only() {
    let authorizer = GlobalGrantsAuthorizer::new(None, default_implicit_actions());
    let claims = token_with_extra(json!({
        "realm_access": { "roles": ["obliterate"] }
    }));
    for action in [Action::Read, Action::Write] {
        check(&authorizer, &claims, RepositoryId::default(), action)
            .await
            .unwrap();
    }
    for action in [Action::Obliterate, Action::Admin, Action::Presign] {
        let err = check(&authorizer, &claims, RepositoryId::default(), action)
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);
    }
}

#[tokio::test]
async fn a_role_adds_to_the_implicit_actions() {
    let authorizer =
        GlobalGrantsAuthorizer::new(Some("groups".to_string()), default_implicit_actions());
    let claims = AuthorizationToken {
        groups: Some(vec!["obliterate".to_string()]),
        ..Default::default()
    };
    for action in [Action::Read, Action::Write, Action::Obliterate] {
        check(&authorizer, &claims, RepositoryId::default(), action)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn no_implicit_actions_reads_read_and_write_from_the_claim() {
    let authorizer = GlobalGrantsAuthorizer::new(Some("groups".to_string()), vec![]);
    let claims = AuthorizationToken {
        groups: Some(vec!["read".to_string()]),
        ..Default::default()
    };
    check(&authorizer, &claims, RepositoryId::default(), Action::Read)
        .await
        .unwrap();
    let err = check(&authorizer, &claims, RepositoryId::default(), Action::Write)
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied);
}

#[tokio::test]
async fn implicit_actions_may_grant_a_privileged_action_to_every_token_holder() {
    let authorizer =
        GlobalGrantsAuthorizer::new(None, vec![Action::Read, Action::Write, Action::Obliterate]);
    let claims = token_with_extra(json!({}));
    check(
        &authorizer,
        &claims,
        RepositoryId::default(),
        Action::Obliterate,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn non_string_values_are_ignored_rather_than_panicking() {
    // `roles` is not a named field, so these shapes reach the reader
    // as-is instead of being shadowed by a typed field.
    let authorizer =
        GlobalGrantsAuthorizer::new(Some("roles".to_string()), default_implicit_actions());
    // A whole claim of the wrong shape denies.
    for wrong_shape in [
        json!({ "roles": "obliterate" }),
        json!({ "roles": 42 }),
        json!({ "roles": { "obliterate": true } }),
    ] {
        let claims = token_with_extra(wrong_shape);
        let err = check(
            &authorizer,
            &claims,
            RepositoryId::default(),
            Action::Obliterate,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);
    }
    // Non-string entries inside the array are skipped, not fatal.
    let claims = token_with_extra(json!({ "roles": [1, null, "obliterate"] }));
    check(
        &authorizer,
        &claims,
        RepositoryId::default(),
        Action::Obliterate,
    )
    .await
    .unwrap();
}

/// The verdict is in the token, so the sync check always answers and
/// agrees with the async one.
#[tokio::test]
async fn sync_check_answers_and_agrees_with_the_async_path() {
    let authorizer =
        GlobalGrantsAuthorizer::new(Some("groups".to_string()), default_implicit_actions());
    let claims = token_with_extra(json!({ "groups": ["obliterate"] }));
    let token = VerifiedToken {
        raw: "raw",
        claims: &claims,
    };
    for token in [None, Some(&token)] {
        for action in [Action::Read, Action::Obliterate, Action::Admin] {
            let sync = authorizer
                .check_repository_access_sync(token, RepositoryId::default(), action)
                .expect("the token holds the verdict");
            let asynchronous = authorizer
                .check_repository_access(token, RepositoryId::default(), action)
                .await;
            assert_eq!(sync.is_ok(), asynchronous.is_ok(), "{action:?}");
        }
    }
}

#[tokio::test]
async fn no_token_denies_even_read() {
    let authorizer =
        GlobalGrantsAuthorizer::new(Some("groups".to_string()), default_implicit_actions());
    for action in [Action::Read, Action::Obliterate] {
        let err = authorizer
            .check_repository_access(None, RepositoryId::default(), action)
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::Unauthenticated);
    }
}
