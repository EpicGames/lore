// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use lore_base::types::Context;
use lore_base::types::RepositoryId;
use lore_proto::auth::CheckUserPermissionResponse;
use lore_proto::auth::ResourcePermission;
use lore_server::auth::jwt::AuthorizationToken;
use lore_server::authnz::repository_authorizer::Action;
use lore_server::authnz::repository_authorizer::*;
use lore_server::settings::AuthSettings;
use tonic::Code;
use tonic::Status;

fn response(entries: Vec<ResourcePermission>) -> CheckUserPermissionResponse {
    CheckUserPermissionResponse {
        allowed_resource_permission: entries,
        denied_resource_permission: vec![],
    }
}

fn entry(resource_id: &str, permissions: &[&str]) -> ResourcePermission {
    ResourcePermission {
        resource_id: resource_id.to_string(),
        permission: permissions.iter().map(ToString::to_string).collect(),
    }
}

/// No `owner`: repository administration without auth rests on `write`
/// through the creator path, not on an action.
#[tokio::test]
async fn allow_all_grants_its_fixed_set_with_or_without_a_token() {
    let claims = AuthorizationToken::default();
    let token = VerifiedToken {
        raw: "raw",
        claims: &claims,
    };
    let repository: RepositoryId = Context::default().into();
    for token in [None, Some(&token)] {
        for action in [
            Action::Read,
            Action::Write,
            Action::Obliterate,
            Action::Presign,
        ] {
            AllowAllRepositoryAuthorizer
                .check_repository_access(token, repository, action)
                .await
                .unwrap();
        }
        for action in [
            Action::Owner,
            Action::Admin,
            Action::Migrate,
            Action::PushProtected,
        ] {
            let err = AllowAllRepositoryAuthorizer
                .check_repository_access(token, repository, action)
                .await
                .unwrap_err();
            assert_eq!(err.code(), Code::PermissionDenied, "{action}");
        }
    }
}

mod resources_claim {
    use std::str::FromStr;

    use lore_base::types::Context;

    use super::*;

    fn repository() -> RepositoryId {
        Context::from_str("0194b726b34e72b0b45550b88a967076")
            .unwrap()
            .into()
    }

    fn access_token_claims(
        resource_id: &str,
        permissions: &[&str],
    ) -> lore_server::auth::jwt::AuthorizationToken {
        AuthorizationToken {
            resources: Some(vec![lore_server::auth::jwt::ResourcePermission {
                resource_id: resource_id.to_string(),
                permission: permissions.iter().map(ToString::to_string).collect(),
            }]),
            ..Default::default()
        }
    }

    async fn check(claims: &AuthorizationToken, action: Action) -> Result<(), Status> {
        // A URL nothing listens on: reaching for the network here would
        // hang or error, so a verdict proves the claim answered in place.
        AuthClientAuthorizer::new("https://auth.invalid".to_string())
            .check_repository_access(
                Some(&VerifiedToken {
                    raw: "raw.jwt",
                    claims,
                }),
                repository(),
                action,
            )
            .await
    }

    /// The auth service refuses exchanged access tokens as
    /// `CheckUserPermission` credentials, so a token carrying a
    /// `resources` claim must be answered from the claim, never sent
    /// upstream.
    #[tokio::test]
    async fn an_access_token_is_answered_from_its_claim() {
        let granted = access_token_claims(
            &format!("urc-{}", repository()),
            &["read", "write", "migrate"],
        );
        check(&granted, Action::Read).await.unwrap();
        check(&granted, Action::Migrate).await.unwrap();
        check(&granted, Action::Obliterate).await.unwrap_err();

        let other_partition = access_token_claims("urc-somewhere-else", &["read"]);
        check(&other_partition, Action::Read).await.unwrap_err();

        // A wildcard grant reaches every partition, as everywhere else.
        let wildcard = access_token_claims("urc-*", &["read"]);
        check(&wildcard, Action::Read).await.unwrap();
        check(&wildcard, Action::Write).await.unwrap_err();
    }

    /// A `resources` claim scoped to no partition denies everything —
    /// present-but-empty is a verdict, not a fallback to the network.
    #[tokio::test]
    async fn an_empty_resources_claim_denies() {
        let empty = AuthorizationToken {
            resources: Some(vec![]),
            ..Default::default()
        };
        check(&empty, Action::Read).await.unwrap_err();
    }

    /// The enumeration and the per-question path answer from the same
    /// derivation, so their verdicts must agree for every access token.
    #[tokio::test]
    async fn enumeration_agrees_with_the_per_question_path() {
        let authorizer = AuthClientAuthorizer::new("https://auth.invalid".to_string());
        let tokens = [
            access_token_claims(&format!("urc-{}", repository()), &["read", "migrate"]),
            access_token_claims("urc-somewhere-else", &["read"]),
            access_token_claims("urc-*", &[]),
            AuthorizationToken {
                resources: Some(vec![]),
                ..Default::default()
            },
        ];
        for claims in &tokens {
            let token = VerifiedToken {
                raw: "raw.jwt",
                claims,
            };
            let grants = authorizer
                .granted_actions(Some(&token), repository())
                .await
                .unwrap()
                .expect("access tokens are enumerable");
            for action in [
                Action::Read,
                Action::Write,
                Action::Migrate,
                Action::Obliterate,
            ] {
                assert_eq!(
                    grants.permits(action),
                    check(claims, action).await.is_ok(),
                    "verdict for {action} must agree for {claims:?}"
                );
            }
        }
    }
}

mod sync_check {
    use std::str::FromStr;

    use lore_base::types::Context;

    use super::*;

    fn repository() -> RepositoryId {
        Context::from_str("0194b726b34e72b0b45550b88a967076")
            .unwrap()
            .into()
    }

    /// Implements only the required method, so the sync check is the
    /// trait's default.
    struct PolicyOnly;

    #[async_trait]
    impl RepositoryAuthorizer for PolicyOnly {
        async fn check_repository_access(
            &self,
            _token: Option<&VerifiedToken<'_>>,
            _repository_id: RepositoryId,
            _action: Action,
        ) -> Result<(), Status> {
            Ok(())
        }
    }

    #[test]
    fn default_cannot_answer_without_io() {
        assert!(
            PolicyOnly
                .check_repository_access_sync(None, repository(), Action::Read)
                .is_none()
        );
    }

    #[test]
    fn allow_all_answers_from_its_fixed_set() {
        let claims = AuthorizationToken::default();
        let token = VerifiedToken {
            raw: "raw",
            claims: &claims,
        };
        for token in [None, Some(&token)] {
            for (action, permitted) in [
                (Action::Read, true),
                (Action::Obliterate, true),
                (Action::Migrate, false),
            ] {
                let verdict = AllowAllRepositoryAuthorizer
                    .check_repository_access_sync(token, repository(), action)
                    .expect("allow-all needs no I/O");
                assert_eq!(verdict.is_ok(), permitted, "{action}");
            }
        }
    }

    /// An access token's `resources` claim is the auth service's own
    /// signed answer, so the legacy authorizer answers it in place and
    /// agrees with its async path. An identity token needs the network.
    #[tokio::test]
    async fn auth_client_answers_access_tokens_and_declines_identity_tokens() {
        let authorizer = AuthClientAuthorizer::new("https://auth.invalid".to_string());
        let granted = AuthorizationToken {
            resources: Some(vec![lore_server::auth::jwt::ResourcePermission {
                resource_id: format!("urc-{}", repository()),
                permission: vec!["migrate".to_string()],
            }]),
            ..Default::default()
        };
        let elsewhere = AuthorizationToken {
            resources: Some(vec![lore_server::auth::jwt::ResourcePermission {
                resource_id: "urc-somewhere-else".to_string(),
                permission: vec![],
            }]),
            ..Default::default()
        };
        for claims in [&granted, &elsewhere] {
            let token = VerifiedToken {
                raw: "raw.jwt",
                claims,
            };
            for action in [Action::Read, Action::Migrate, Action::Obliterate] {
                let sync = authorizer
                    .check_repository_access_sync(Some(&token), repository(), action)
                    .expect("access tokens are answered in place");
                let asynchronous = authorizer
                    .check_repository_access(Some(&token), repository(), action)
                    .await;
                assert_eq!(
                    sync.is_ok(),
                    asynchronous.is_ok(),
                    "{action:?} on {claims:?}"
                );
            }
        }

        let identity = AuthorizationToken::default();
        let token = VerifiedToken {
            raw: "raw.jwt",
            claims: &identity,
        };
        assert!(
            authorizer
                .check_repository_access_sync(Some(&token), repository(), Action::Read)
                .is_none()
        );
    }

    /// No token is answered locally: nothing to forward upstream.
    #[tokio::test]
    async fn auth_client_denies_no_token_without_going_upstream() {
        let authorizer = AuthClientAuthorizer::new("https://auth.invalid".to_string());
        let err = authorizer
            .check_repository_access(None, repository(), Action::Read)
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::Unauthenticated);
    }
}

mod granted_access {
    use std::str::FromStr;

    use lore_base::types::Context;
    use lore_server::grpc::no_repository_access_status;

    use super::*;

    fn repository() -> RepositoryId {
        Context::from_str("0194b726b34e72b0b45550b88a967076")
            .unwrap()
            .into()
    }

    /// Non-enumerable: `granted_actions` keeps its `Ok(None)` default and
    /// only the per-question path answers.
    struct PolicyOnly(bool);

    #[async_trait]
    impl RepositoryAuthorizer for PolicyOnly {
        async fn check_repository_access(
            &self,
            _token: Option<&VerifiedToken<'_>>,
            _repository_id: RepositoryId,
            action: Action,
        ) -> Result<(), Status> {
            assert_eq!(
                action,
                Action::Read,
                "granted_access asks reachability only"
            );
            if self.0 {
                Ok(())
            } else {
                Err(Status::permission_denied("the policy's own reason"))
            }
        }
    }

    #[tokio::test]
    async fn enumerable_authorizer_yields_its_grants() {
        let authorizer: Arc<dyn RepositoryAuthorizer> = Arc::new(AllowAllRepositoryAuthorizer);
        let grants = authorizer.granted_access(None, repository()).await.unwrap();
        assert_eq!(
            grants,
            Some(Grants::Actions(
                ALLOW_ALL_ACTIONS.iter().map(ToString::to_string).collect()
            ))
        );
    }

    #[tokio::test]
    async fn non_enumerable_authorizer_answers_reachability_with_no_grants() {
        let authorizer: Arc<dyn RepositoryAuthorizer> = Arc::new(PolicyOnly(true));
        let grants = authorizer.granted_access(None, repository()).await.unwrap();
        assert_eq!(grants, None);

        let denied: Arc<dyn RepositoryAuthorizer> = Arc::new(PolicyOnly(false));
        let err = denied.granted_access(None, repository()).await.unwrap_err();
        assert_eq!(err.message(), no_repository_access_status().message());
    }

    /// An enumerated denial flattens to the uniform status, so an
    /// unauthorized caller learns nothing from the reason.
    #[tokio::test]
    async fn denials_flatten_to_the_uniform_status() {
        let claims = AuthorizationToken {
            resources: Some(vec![lore_server::auth::jwt::ResourcePermission {
                resource_id: "urc-somewhere-else".to_string(),
                permission: vec![],
            }]),
            ..Default::default()
        };
        let token = VerifiedToken {
            raw: "raw.jwt",
            claims: &claims,
        };
        let enumerable: Arc<dyn RepositoryAuthorizer> = Arc::new(AuthClientAuthorizer::new(
            "https://auth.invalid".to_string(),
        ));
        let err = enumerable
            .granted_access(Some(&token), repository())
            .await
            .unwrap_err();
        assert_eq!(err.message(), no_repository_access_status().message());
    }
}

mod permits_helper {
    use std::str::FromStr;

    use lore_base::types::Context;
    use lore_server::authnz::repository_authorizer::RawToken;

    use super::*;

    fn repository() -> RepositoryId {
        Context::from_str("0194b726b34e72b0b45550b88a967076")
            .unwrap()
            .into()
    }

    fn extensions_with(grants: Option<PartitionGrants>, token: bool) -> tonic::Extensions {
        let mut extensions = tonic::Extensions::new();
        if let Some(grants) = grants {
            extensions.insert(grants);
        }
        if token {
            extensions.insert(AuthorizationToken::default());
            extensions.insert(RawToken("raw.jwt".into()));
        }
        extensions
    }

    /// The layer's enumerated grants answer first — proven by pairing an
    /// allow-all authorizer with grants that deny: the denial wins.
    #[tokio::test]
    async fn enumerated_grants_answer_before_the_authorizer() {
        let authorizer: Arc<dyn RepositoryAuthorizer> = Arc::new(AllowAllRepositoryAuthorizer);
        let extensions = extensions_with(
            Some(PartitionGrants {
                repository_id: repository(),
                grants: Grants::Actions(HashSet::new()),
            }),
            true,
        );
        assert!(
            !authorizer
                .permits(&extensions, repository(), Action::Migrate)
                .await
        );
    }

    /// Grants naming another partition are not consumed; the check falls
    /// back to the authorizer with the verified token.
    #[tokio::test]
    async fn grants_for_another_partition_fall_back_to_the_authorizer() {
        let authorizer: Arc<dyn RepositoryAuthorizer> = Arc::new(AllowAllRepositoryAuthorizer);
        let other: RepositoryId = Context::from_str("f6ca55437aa34198ba0f0fdc33154d51")
            .unwrap()
            .into();
        let extensions = extensions_with(
            Some(PartitionGrants {
                repository_id: other,
                grants: Grants::Denied,
            }),
            true,
        );
        assert!(
            authorizer
                .permits(&extensions, repository(), Action::Write)
                .await
        );
    }

    /// Without a verified token the authorizer still decides: allow-all
    /// answers from its fixed set, the token-reading authorizers deny.
    #[tokio::test]
    async fn no_token_is_asked_of_the_authorizer() {
        let extensions = extensions_with(None, false);
        let allow_all: Arc<dyn RepositoryAuthorizer> = Arc::new(AllowAllRepositoryAuthorizer);
        assert!(
            allow_all
                .permits(&extensions, repository(), Action::Write)
                .await
        );
        assert!(
            !allow_all
                .permits(&extensions, repository(), Action::Owner)
                .await
        );
        assert!(
            !allow_all
                .permits(&extensions, repository(), Action::Migrate)
                .await
        );

        let tier_1: Arc<dyn RepositoryAuthorizer> = Arc::new(
            lore_server::authnz::global_grants_authorizer::GlobalGrantsAuthorizer::new(
                None,
                vec![Action::Read, Action::Write],
            ),
        );
        assert!(
            !tier_1
                .permits(&extensions, repository(), Action::Read)
                .await
        );
    }
}

mod grants {
    use super::*;

    #[test]
    fn permits_and_reachable_follow_the_variant() {
        assert!(!Grants::Denied.reachable());
        assert!(!Grants::Denied.permits(Action::Obliterate));

        let actions = Grants::Actions(["migrate".to_string()].into());
        assert!(actions.reachable());
        assert!(actions.permits(Action::Migrate));
        assert!(!actions.permits(Action::Obliterate));

        // Reachable with nothing granted: a matched entry with an empty
        // permission list, or Tier 1 without a permission claim.
        let none = Grants::Actions(HashSet::new());
        assert!(none.reachable());
        assert!(
            none.permits(Action::Read),
            "reaching the partition grants read"
        );
        assert!(!none.permits(Action::Write));
    }

    /// The response-derived grants merge every entry naming the
    /// resource and ignore the rest.
    #[test]
    fn response_grants_merge_matching_entries() {
        let response = response(vec![
            entry("urc-abc", &["read"]),
            entry("urc-other", &["obliterate"]),
            entry("urc-abc", &["migrate"]),
        ]);
        let grants = grants_from_response(&response, "urc-abc");
        assert!(grants.reachable());
        assert!(grants.permits(Action::Read));
        assert!(grants.permits(Action::Migrate));
        assert!(!grants.permits(Action::Obliterate));

        assert_eq!(
            grants_from_response(&response, "urc-absent"),
            Grants::Denied
        );
    }
}

#[test]
fn bearer_header_rebuilds_the_forwarded_header() {
    let claims = AuthorizationToken::default();
    let token = VerifiedToken {
        raw: "abc.def.ghi",
        claims: &claims,
    };
    assert_eq!(
        bearer_header(Some(&token)),
        Some("Bearer abc.def.ghi".to_string())
    );
    assert_eq!(bearer_header(None), None);
}

#[test]
fn upstream_request_carries_resource_and_authorization() {
    let request =
        check_user_permission_request("urc-abc".into(), Some("Bearer tok".into())).unwrap();
    assert_eq!(request.get_ref().resource_id, vec!["urc-abc".to_string()]);
    assert_eq!(request.get_ref().target_user, None);
    assert_eq!(
        request
            .metadata()
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap(),
        "Bearer tok"
    );
}

#[test]
fn action_requires_membership_in_the_permission_list() {
    let response = response(vec![entry("urc-abc", &["read", "obliterate"])]);
    evaluate_check_user_permission(&response, "urc-abc", Action::Obliterate).unwrap();
    evaluate_check_user_permission(&response, "urc-abc", Action::Read).unwrap();
    // The fail-open case: an authorizer that ignores the action would
    // permit this.
    let err = evaluate_check_user_permission(&response, "urc-abc", Action::Presign).unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied);
    // `read` does not imply `write`.
    let err = evaluate_check_user_permission(&response, "urc-abc", Action::Write).unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied);
}

#[test]
fn an_entry_grants_read_and_only_its_listed_actions() {
    let response = response(vec![entry("urc-abc", &[])]);
    evaluate_check_user_permission(&response, "urc-abc", Action::Read).unwrap();
    for action in [Action::Write, Action::Obliterate] {
        let err = evaluate_check_user_permission(&response, "urc-abc", action).unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);
    }
}

#[test]
fn absent_resource_denies_every_action() {
    let response = response(vec![]);
    for action in [Action::Read, Action::Obliterate] {
        let err = evaluate_check_user_permission(&response, "urc-abc", action).unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);
    }
}

#[test]
fn mismatched_resource_denies() {
    let response = response(vec![entry("urc-other", &["read", "obliterate"])]);
    for action in [Action::Read, Action::Obliterate] {
        let err = evaluate_check_user_permission(&response, "urc-abc", action).unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);
    }
}

#[test]
fn the_resource_is_found_past_the_first_entry() {
    let response = response(vec![
        entry("urc-other", &["obliterate"]),
        entry("urc-abc", &["read"]),
    ]);
    evaluate_check_user_permission(&response, "urc-abc", Action::Read).unwrap();
    let err = evaluate_check_user_permission(&response, "urc-abc", Action::Obliterate).unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied);
}

/// `[server.auth]` settings with the mandatory pair present and `extra`
/// TOML appended, mirroring how a config file builds them.
fn auth_settings(extra: &str) -> AuthSettings {
    toml::from_str(&format!(
        "jwt_issuer = \"https://auth.example.com\"\njwt_audience = [\"lore\"]\n{extra}"
    ))
    .unwrap()
}

const AUTH_URL: Option<&str> = Some("https://legacy-auth.example.com");

#[test]
fn selection_follows_the_flowchart() {
    // (auth settings, legacy auth_url) → selected implementation.
    let table = [
        (None, None, AuthorizerSelection::AllowAll),
        (
            Some(auth_settings("")),
            AUTH_URL,
            AuthorizerSelection::AuthClient,
        ),
        (
            Some(auth_settings("resource_claim = \"resources\"")),
            None,
            AuthorizerSelection::ResourceGrants,
        ),
        (
            Some(auth_settings("")),
            None,
            AuthorizerSelection::GlobalGrants,
        ),
        (
            Some(auth_settings("permission_claim = \"realm_access.roles\"")),
            None,
            AuthorizerSelection::GlobalGrants,
        ),
    ];
    for (auth, auth_url, expected) in table {
        assert_eq!(
            select_repository_authorizer(auth.as_ref(), auth_url).unwrap(),
            expected,
            "auth: {auth:?}, auth_url: {auth_url:?}"
        );
        // Construction agrees with selection for every valid row.
        repository_authorizer(auth.as_ref(), auth_url.map(ToString::to_string)).unwrap();
    }
}

/// The check that makes a `UrcAuthApi` deployment's move onto the token
/// claims a deliberate step: setting `resource_claim` while `auth_url`
/// is still configured refuses to start instead of silently staying on
/// the auth service.
#[test]
fn auth_url_with_resource_claim_refuses_startup_naming_both() {
    let auth = auth_settings("resource_claim = \"resources\"");
    let message = select_repository_authorizer(Some(&auth), AUTH_URL)
        .unwrap_err()
        .to_string();
    assert!(message.contains("auth_url"), "{message}");
    assert!(message.contains("resource_claim"), "{message}");
}

/// `auth_url` names an authorization service, but without `[server.auth]`
/// nothing verifies tokens: the server would run open while the operator
/// believed they had configured authorization. Refused, naming both.
#[test]
fn auth_url_without_server_auth_refuses_startup_naming_both() {
    let message = select_repository_authorizer(None, AUTH_URL)
        .unwrap_err()
        .to_string();
    assert!(message.contains("auth_url"), "{message}");
    assert!(message.contains("[server.auth]"), "{message}");
}

#[test]
fn implicit_actions_reach_the_tier_1_authorizer() {
    let auth = auth_settings("implicit_actions = [\"read\", \"obliterate\"]");
    assert_eq!(
        auth.implicit_actions(),
        vec![Action::Read, Action::Obliterate]
    );
    assert_eq!(auth_settings("").implicit_actions(), vec![Action::Write]);
    repository_authorizer(Some(&auth), None).unwrap();
}

/// Action names are checked when the config is parsed, so a misspelt or
/// wrongly cased name is a configuration error rather than an action no
/// check ever asks for.
#[test]
fn implicit_actions_refuses_unknown_action_names() {
    for wrong in ["Read", "read-write", "push"] {
        let parsed: Result<AuthSettings, _> = toml::from_str(&format!(
            "jwt_issuer = \"https://auth.example.com\"\njwt_audience = [\"lore\"]\nimplicit_actions = [\"{wrong}\"]"
        ));
        let message = parsed.expect_err(wrong).to_string();
        assert!(message.contains(wrong), "{message}");
    }
}

/// `implicit_actions` is Tier 1's setting: alongside `auth_url` or
/// `resource_claim` it would do nothing, so startup refuses it.
#[test]
fn implicit_actions_outside_tier_1_refuses_startup() {
    let with_resource_claim =
        auth_settings("implicit_actions = [\"read\"]\nresource_claim = \"resources\"");
    let with_auth_url = auth_settings("implicit_actions = [\"read\"]");
    for (auth, auth_url) in [(&with_resource_claim, None), (&with_auth_url, AUTH_URL)] {
        let message = select_repository_authorizer(Some(auth), auth_url)
            .unwrap_err()
            .to_string();
        assert!(message.contains("implicit_actions"), "{message}");
    }
}

/// `implicit_actions = []` with no `permission_claim` is a read-only server:
/// any token reads, nobody writes.
#[test]
fn empty_implicit_actions_without_permission_claim_starts_read_only() {
    select_repository_authorizer(Some(&auth_settings("implicit_actions = []")), None).unwrap();
}

/// The startup log prints the `Display` form, so an operator can tell
/// which implementation they got.
#[test]
fn selection_display_names_the_implementation() {
    assert_eq!(
        AuthorizerSelection::AllowAll.to_string(),
        "AllowAllRepositoryAuthorizer"
    );
    assert_eq!(
        AuthorizerSelection::AuthClient.to_string(),
        "AuthClientAuthorizer"
    );
    assert_eq!(
        AuthorizerSelection::GlobalGrants.to_string(),
        "GlobalGrantsAuthorizer"
    );
    assert_eq!(
        AuthorizerSelection::ResourceGrants.to_string(),
        "ResourceGrantsAuthorizer"
    );
}
