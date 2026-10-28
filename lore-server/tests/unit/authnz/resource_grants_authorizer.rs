// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;

use lore_base::types::Context;
use lore_base::types::RepositoryId;
use lore_server::auth::jwt::AuthorizationToken;
use lore_server::auth::jwt::ResourcePermission;
use lore_server::authnz::repository_authorizer::Action;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::VerifiedToken;
use lore_server::authnz::resource_grants_authorizer::*;
use serde_json::json;
use tonic::Code;
use tonic::Status;

const REPOSITORY_HEX: &str = "0194b726b34e72b0b45550b88a967076";
const UNRELATED_HEX: &str = "0192ae48ccf17060bc1ba9d04f6acb2f";

fn repository(id: &str) -> RepositoryId {
    Context::from_str(id).unwrap().into()
}

fn default_authorizer() -> ResourceGrantsAuthorizer {
    ResourceGrantsAuthorizer::new(
        "resources".to_string(),
        "resource_id".to_string(),
        None,
        "urc-{id}".to_string(),
        "urc-*".to_string(),
    )
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
    authorizer: &ResourceGrantsAuthorizer,
    claims: &AuthorizationToken,
    repository_id: RepositoryId,
    action: Action,
) -> Result<(), Status> {
    let token = VerifiedToken { raw: "raw", claims };
    authorizer
        .check_repository_access(Some(&token), repository_id, action)
        .await
}

/// `resource_claim = "resources"` with the default template reads the
/// legacy claim exactly as today's matching does: `UrcAuthApi` tokens
/// need no migration.
#[tokio::test]
async fn default_template_reproduces_the_legacy_matching() {
    let authorizer = default_authorizer();
    let claims = AuthorizationToken {
        resources: Some(vec![ResourcePermission {
            resource_id: format!("urc-{REPOSITORY_HEX}"),
            permission: vec!["obliterate".to_string()],
        }]),
        ..Default::default()
    };

    check(
        &authorizer,
        &claims,
        repository(REPOSITORY_HEX),
        Action::Read,
    )
    .await
    .unwrap();
    check(
        &authorizer,
        &claims,
        repository(REPOSITORY_HEX),
        Action::Obliterate,
    )
    .await
    .unwrap();
    for action in [Action::Read, Action::Obliterate] {
        let err = check(&authorizer, &claims, repository(UNRELATED_HEX), action)
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);
    }
}

fn claims_with_resources(resources: Vec<ResourcePermission>) -> AuthorizationToken {
    // `resources` is a named field, and `claim_at` resolves named fields
    // before `extra` — exactly as decoding a real token would populate it.
    AuthorizationToken {
        resources: Some(resources),
        ..Default::default()
    }
}

fn entry(resource_id: &str, permissions: &[&str]) -> ResourcePermission {
    ResourcePermission {
        resource_id: resource_id.to_string(),
        permission: permissions.iter().map(ToString::to_string).collect(),
    }
}

#[tokio::test]
async fn wildcard_entry_matches_every_repository() {
    let authorizer = default_authorizer();
    let claims = claims_with_resources(vec![entry("urc-*", &["migrate"])]);

    for repository in [repository(REPOSITORY_HEX), repository(UNRELATED_HEX)] {
        check(&authorizer, &claims, repository, Action::Read)
            .await
            .unwrap();
        check(&authorizer, &claims, repository, Action::Migrate)
            .await
            .unwrap();
        let err = check(&authorizer, &claims, repository, Action::Obliterate)
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);
    }
}

#[tokio::test]
async fn permissions_merge_across_matching_entries() {
    let authorizer = default_authorizer();
    let claims = claims_with_resources(vec![
        entry(&format!("urc-{REPOSITORY_HEX}"), &["write"]),
        entry("urc-*", &["migrate"]),
        entry(&format!("urc-{REPOSITORY_HEX}"), &["obliterate"]),
    ]);

    for action in [Action::Write, Action::Migrate, Action::Obliterate] {
        check(&authorizer, &claims, repository(REPOSITORY_HEX), action)
            .await
            .unwrap();
    }
    // The unrelated repository sees only the wildcard entry's actions.
    check(
        &authorizer,
        &claims,
        repository(UNRELATED_HEX),
        Action::Migrate,
    )
    .await
    .unwrap();
    let err = check(
        &authorizer,
        &claims,
        repository(UNRELATED_HEX),
        Action::Write,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied);
}

#[tokio::test]
async fn any_entry_grants_read_and_write_must_be_listed() {
    let authorizer = default_authorizer();
    let claims = claims_with_resources(vec![
        entry(&format!("urc-{REPOSITORY_HEX}"), &["write"]),
        entry(&format!("urc-{UNRELATED_HEX}"), &["migrate"]),
    ]);

    for action in [Action::Read, Action::Write] {
        check(&authorizer, &claims, repository(REPOSITORY_HEX), action)
            .await
            .unwrap();
    }
    check(
        &authorizer,
        &claims,
        repository(UNRELATED_HEX),
        Action::Read,
    )
    .await
    .unwrap();
    let err = check(
        &authorizer,
        &claims,
        repository(UNRELATED_HEX),
        Action::Write,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied);
}

#[tokio::test]
async fn no_resource_claim_denies_everything_including_read() {
    let authorizer = default_authorizer();
    let claims = token_with_extra(json!({}));

    for action in [Action::Read, Action::Obliterate] {
        let err = check(&authorizer, &claims, repository(REPOSITORY_HEX), action)
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);
    }
}

#[tokio::test]
async fn permission_claim_names_the_actions_field() {
    let authorizer = ResourceGrantsAuthorizer::new(
        "authorization.grants".to_string(),
        "resource_id".to_string(),
        Some("actions".to_string()),
        "repo:{id}".to_string(),
        "repo:all".to_string(),
    );
    let claims = token_with_extra(json!({
        "authorization": {
            "grants": [
                { "resource_id": format!("repo:{REPOSITORY_HEX}"), "actions": ["obliterate"] },
            ]
        }
    }));

    check(
        &authorizer,
        &claims,
        repository(REPOSITORY_HEX),
        Action::Obliterate,
    )
    .await
    .unwrap();
    let err = check(
        &authorizer,
        &claims,
        repository(UNRELATED_HEX),
        Action::Read,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied);
}

/// Keycloak's UMA `authorization.permissions` shape, which the provider
/// cannot be configured out of: `rsname` names the resource and `scopes`
/// the actions. Both entry field names are configuration, so this shape
/// needs no mapper on the provider side.
#[tokio::test]
async fn resource_id_claim_names_the_resource_field() {
    let authorizer = ResourceGrantsAuthorizer::new(
        "authorization.permissions".to_string(),
        "rsname".to_string(),
        Some("scopes".to_string()),
        "urc-{id}".to_string(),
        "urc-*".to_string(),
    );
    let claims = token_with_extra(json!({
        "authorization": {
            "permissions": [
                { "rsname": format!("urc-{REPOSITORY_HEX}"), "scopes": ["obliterate"] },
                // An entry carrying `resource_id` under this config does
                // not name a resource at all, so it is skipped.
                { "resource_id": format!("urc-{UNRELATED_HEX}"), "scopes": ["obliterate"] },
            ]
        }
    }));

    check(
        &authorizer,
        &claims,
        repository(REPOSITORY_HEX),
        Action::Obliterate,
    )
    .await
    .unwrap();
    let err = check(
        &authorizer,
        &claims,
        repository(UNRELATED_HEX),
        Action::Read,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied);
}

#[tokio::test]
async fn malformed_entries_are_skipped_rather_than_fatal() {
    // A provider emitting loose shapes emits them under a custom claim:
    // the typed `resources` field could not even decode these. A custom
    // claim lands in `extra`, where every shape reaches the reader.
    let authorizer = ResourceGrantsAuthorizer::new(
        "grants".to_string(),
        "resource_id".to_string(),
        None,
        "urc-{id}".to_string(),
        "urc-*".to_string(),
    );
    let claims = token_with_extra(json!({
        "grants": [
            "not an object",
            { "no_resource_id": true },
            { "resource_id": 42 },
            { "resource_id": format!("urc-{REPOSITORY_HEX}"), "permission": "not an array" },
            { "resource_id": format!("urc-{REPOSITORY_HEX}"), "permission": [1, null, "write"] },
        ]
    }));

    // The unreadable actions grant nothing; the readable one still counts.
    check(
        &authorizer,
        &claims,
        repository(REPOSITORY_HEX),
        Action::Write,
    )
    .await
    .unwrap();
    let err = check(
        &authorizer,
        &claims,
        repository(REPOSITORY_HEX),
        Action::Obliterate,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied);

    // A claim that is not an array at all denies everything.
    let claims = token_with_extra(json!({ "grants": "not an array" }));
    let err = check(
        &authorizer,
        &claims,
        repository(REPOSITORY_HEX),
        Action::Read,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied);
}

/// The verdict is in the token, so the sync check always answers and
/// agrees with the async one, for the granted partition and another.
#[tokio::test]
async fn sync_check_answers_and_agrees_with_the_async_path() {
    let authorizer = default_authorizer();
    let claims = claims_with_resources(vec![entry(
        &format!("urc-{REPOSITORY_HEX}"),
        &["obliterate"],
    )]);
    let token = VerifiedToken {
        raw: "raw",
        claims: &claims,
    };
    for token in [None, Some(&token)] {
        for repository in [repository(REPOSITORY_HEX), repository(UNRELATED_HEX)] {
            for action in [Action::Read, Action::Obliterate, Action::Admin] {
                let sync = authorizer
                    .check_repository_access_sync(token, repository, action)
                    .expect("the token holds the verdict");
                let asynchronous = authorizer
                    .check_repository_access(token, repository, action)
                    .await;
                assert_eq!(
                    sync.is_ok(),
                    asynchronous.is_ok(),
                    "{action:?} on {repository}"
                );
            }
        }
    }
}

#[tokio::test]
async fn no_token_denies_even_read() {
    let authorizer = default_authorizer();
    for action in [Action::Read, Action::Obliterate] {
        let err = authorizer
            .check_repository_access(None, repository(REPOSITORY_HEX), action)
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::Unauthenticated);
    }
}

/// A lookup needs a verified token and no grant: the client looks a repository up before
/// it can exchange for a token naming it. Reaching the partition still needs the grant.
#[tokio::test]
async fn a_lookup_needs_a_verified_token_and_no_grant() {
    let authorizer = default_authorizer();
    let login = token_with_extra(json!({}));
    let token = VerifiedToken {
        raw: "raw",
        claims: &login,
    };

    for id in [REPOSITORY_HEX, UNRELATED_HEX] {
        authorizer
            .check_repository_lookup(Some(&token), repository(id))
            .await
            .expect("any verified token may look a repository up");
        let err = check(&authorizer, &login, repository(id), Action::Read)
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);
    }

    let err = authorizer
        .check_repository_lookup(None, repository(REPOSITORY_HEX))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unauthenticated);
}
