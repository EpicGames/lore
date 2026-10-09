// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod environment;
mod forwarded_repository;
mod forwarded_requests;
mod forwarded_revision;
mod handlers;
mod lock_service;
mod notification_service;
mod repository;
mod revision;
mod server;
mod storage;
mod thinclient;
mod tower;

use std::str::FromStr;
use std::sync::Arc;

use bytes::Bytes;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_revision::branch::BranchError;
use lore_revision::diff::DiffError;
use lore_revision::find::FindError;
use lore_revision::immutable::ImmutableError;
use lore_revision::lore::RepositoryId;
use lore_revision::metadata::MetadataError;
use lore_revision::metadata::branch::BranchMetadataError;
use lore_revision::metadata::repository::RepositoryMetadataError;
use lore_revision::repository::RepositoryError;
use lore_revision::state::StateError;
use lore_server::auth::jwt::AuthorizationToken;
use lore_server::auth::jwt::ResourcePermission;
use lore_server::authnz::repository_authorizer::RawToken;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::VerifiedToken;
use lore_server::grpc::*;
use lore_storage::StoreError;
use tonic::Code;
use tonic::Extensions;
use tonic::Status;

mod link_read {
    use std::collections::HashSet;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use async_trait::async_trait;
    use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
    use lore_server::authnz::resource_grants_authorizer::ResourceGrantsAuthorizer;

    use super::*;

    fn repository(hex: &str) -> RepositoryId {
        Context::from_str(hex).unwrap().into()
    }

    const GRANTED: &str = "0194b726b34e72b0b45550b88a967076";
    const UNGRANTED: &str = "f6ca55437aa34198ba0f0fdc33154d51";

    /// Answers in memory from a fixed partition set, and records if the
    /// async path is ever entered. `answers: false` models an authorizer
    /// that needs I/O for every question.
    struct Recording {
        answers: bool,
        granted: HashSet<RepositoryId>,
        async_entered: Arc<AtomicBool>,
    }

    #[async_trait]
    impl RepositoryAuthorizer for Recording {
        async fn check_repository_access(
            &self,
            _token: Option<&VerifiedToken<'_>>,
            _repository_id: RepositoryId,
            _action: Option<&str>,
        ) -> Result<(), Status> {
            self.async_entered.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn check_repository_access_sync(
            &self,
            _token: Option<&VerifiedToken<'_>>,
            repository_id: RepositoryId,
            action: Option<&str>,
        ) -> Option<Result<(), Status>> {
            assert_eq!(action, None, "a link read asks reachability only");
            self.answers.then(|| {
                if self.granted.contains(&repository_id) {
                    Ok(())
                } else {
                    Err(Status::permission_denied("not granted"))
                }
            })
        }
    }

    fn extensions_with_token(claims: AuthorizationToken) -> Extensions {
        let mut extensions = Extensions::new();
        extensions.insert(claims);
        extensions.insert(RawToken("raw.jwt".to_string()));
        extensions
    }

    /// The closure never enters the authorizer's async path: it takes
    /// the synchronous verdict as-is.
    #[test]
    fn answers_from_the_sync_verdict_without_entering_the_async_path() {
        let async_entered = Arc::new(AtomicBool::new(false));
        let authorizer: Arc<dyn RepositoryAuthorizer> = Arc::new(Recording {
            answers: true,
            granted: [repository(GRANTED)].into(),
            async_entered: async_entered.clone(),
        });
        let can_read =
            link_read_authorizer(&authorizer, &extensions_with_token(Default::default()));
        assert!(can_read(repository(GRANTED)));
        assert!(!can_read(repository(UNGRANTED)));
        assert!(!async_entered.load(Ordering::SeqCst));
    }

    /// An authorizer that cannot answer without I/O denies the link read;
    /// the async path — which would permit here — is not consulted.
    #[test]
    fn denies_when_the_authorizer_cannot_answer_synchronously() {
        let async_entered = Arc::new(AtomicBool::new(false));
        let authorizer: Arc<dyn RepositoryAuthorizer> = Arc::new(Recording {
            answers: false,
            granted: [repository(GRANTED)].into(),
            async_entered: async_entered.clone(),
        });
        let can_read =
            link_read_authorizer(&authorizer, &extensions_with_token(Default::default()));
        assert!(!can_read(repository(GRANTED)));
        assert!(!async_entered.load(Ordering::SeqCst));
    }

    /// No verifier configured: no token in the extensions, and the
    /// allow-all authorizer selected alongside lets every link through.
    #[test]
    fn no_token_under_allow_all_reads_every_partition() {
        let authorizer: Arc<dyn RepositoryAuthorizer> = Arc::new(AllowAllRepositoryAuthorizer);
        let can_read = link_read_authorizer(&authorizer, &Extensions::new());
        assert!(can_read(repository(GRANTED)));
        assert!(can_read(repository(UNGRANTED)));
    }

    /// Tier 2 end to end: the verified token's claim decides, per
    /// partition, with no network.
    #[test]
    fn resource_grants_answers_from_the_verified_token() {
        let authorizer: Arc<dyn RepositoryAuthorizer> = Arc::new(ResourceGrantsAuthorizer::new(
            "resources".to_string(),
            "resource_id".to_string(),
            None,
            "urc-{id}".to_string(),
            "urc-*".to_string(),
        ));
        let claims = AuthorizationToken {
            resources: Some(vec![ResourcePermission {
                resource_id: format!("urc-{}", repository(GRANTED)),
                permission: vec![],
            }]),
            ..Default::default()
        };
        let can_read = link_read_authorizer(&authorizer, &extensions_with_token(claims));
        assert!(can_read(repository(GRANTED)));
        assert!(!can_read(repository(UNGRANTED)));

        // A verifier is configured but this request carries no token:
        // nothing is granted.
        let can_read = link_read_authorizer(&authorizer, &Extensions::new());
        assert!(!can_read(repository(GRANTED)));
    }
}

#[test]
fn revision_signature_reads_a_whole_signature() {
    let hash = Hash::hash_buffer(&[1, 2, 3]);
    let read = revision_signature(Bytes::from(hash)).expect("a whole signature is read");
    assert_eq!(read, hash);
}

// Not a partial signature but an unset one, which the callers already answer
// for as a revision that does not exist.
#[test]
fn revision_signature_reads_an_absent_signature_as_zero() {
    let read = revision_signature(Bytes::new()).expect("an unset signature is read");
    assert!(read.is_zero());
}

// Both directions of wrong: a prefix of a hash, and a hash with anything
// trailing it.
#[test]
fn revision_signature_refuses_a_signature_that_is_not_whole() {
    let hash = Hash::hash_buffer(&[1, 2, 3]);
    for length in [1, 8, size_of::<Hash>() - 1, size_of::<Hash>() + 1] {
        let mut signature = Vec::from(Bytes::from(hash));
        signature.resize(length, 0);

        let status = revision_signature(Bytes::from(signature))
            .expect_err("a signature that is not whole is refused");
        assert_eq!(
            status.code(),
            tonic::Code::FailedPrecondition,
            "a {length} byte signature was not refused as a failed precondition",
        );
        assert!(
            status.message().contains("partial revision hash signature"),
            "the refusal does not say what was wrong: {}",
            status.message(),
        );
    }
}

#[test]
fn get_authorization_extracts_auth() {
    let mut extensions = Extensions::new();
    let test_authz_token = AuthorizationToken::default();
    extensions.insert(test_authz_token.clone());

    let authz_data = get_authorization(&extensions).ok().unwrap();
    assert_eq!(authz_data, test_authz_token);
}

#[test]
fn user_permissions_includes_matched_repo_permissions() {
    let mut extensions = Extensions::new();
    let test_repository_id = "urc-0194b726b34e72b0b45550b88a967076".to_string();
    let unrelated_repository_id = "urc-0192ae48ccf17060bc1ba9d04f6acb2f".to_string();
    let mut test_authz_token = AuthorizationToken::default();

    let test_resource_permission = ResourcePermission {
        resource_id: test_repository_id.clone(),
        permission: vec!["test_permission".to_string()],
    };

    test_authz_token.resources = Some(vec![test_resource_permission.clone()]);
    extensions.insert(test_authz_token.clone());

    let test_repository_context: RepositoryId =
        Context::from_str(test_repository_id.strip_prefix("urc-").unwrap())
            .unwrap()
            .into();
    let test_unrelated_repository_context: RepositoryId =
        Context::from_str(unrelated_repository_id.strip_prefix("urc-").unwrap())
            .unwrap()
            .into();
    let matched_permissions = user_permissions(&extensions, test_repository_context);
    let no_matched_permissions = user_permissions(&extensions, test_unrelated_repository_context);
    assert_eq!(matched_permissions, vec!["test_permission".to_string()]);
    assert_eq!(no_matched_permissions, Vec::<String>::new());
}

#[test]
fn user_permissions_includes_wildcard_resource() {
    let mut extensions = Extensions::new();
    let test_repository_id = "urc-0194b726b34e72b0b45550b88a967076".to_string();
    let unrelated_repository_id = "urc-0192ae48ccf17060bc1ba9d04f6acb2f".to_string();
    let mut test_authz_token = AuthorizationToken::default();

    let test_resource_permission = ResourcePermission {
        resource_id: test_repository_id.clone(),
        permission: vec!["test_permission".to_string()],
    };
    let test_wildcard_resource_permission = ResourcePermission {
        resource_id: "urc-*".to_string().clone(),
        permission: vec!["test_wildcard_permission".to_string()],
    };

    test_authz_token.resources = Some(vec![
        test_resource_permission.clone(),
        test_wildcard_resource_permission.clone(),
    ]);
    extensions.insert(test_authz_token.clone());

    let test_repository_context: RepositoryId =
        Context::from_str(test_repository_id.strip_prefix("urc-").unwrap())
            .unwrap()
            .into();
    let test_unrelated_repository_context: RepositoryId =
        Context::from_str(unrelated_repository_id.strip_prefix("urc-").unwrap())
            .unwrap()
            .into();
    let matched_permissions = user_permissions(&extensions, test_repository_context);
    let no_matched_permissions = user_permissions(&extensions, test_unrelated_repository_context);
    assert_eq!(
        matched_permissions,
        vec![
            "test_permission".to_string(),
            "test_wildcard_permission".to_string()
        ]
    );
    assert_eq!(
        no_matched_permissions,
        vec!["test_wildcard_permission".to_string()]
    );
}

#[test]
fn finds_existing_matching_permission_with_regular_repo() {
    let mut extensions = Extensions::new();
    let test_repository_id = "urc-0194b726b34e72b0b45550b88a967076".to_string();
    let unrelated_repository_id = "urc-0192ae48ccf17060bc1ba9d04f6acb2f".to_string();
    let mut test_authz_token = AuthorizationToken::default();

    let test_resource_permission = ResourcePermission {
        resource_id: test_repository_id.clone(),
        permission: vec![
            "test_permission".to_string(),
            "other_permission".to_string(),
        ],
    };

    test_authz_token.resources = Some(vec![test_resource_permission.clone()]);
    extensions.insert(test_authz_token.clone());

    let test_repository_context: RepositoryId =
        Context::from_str(test_repository_id.strip_prefix("urc-").unwrap())
            .unwrap()
            .into();
    let test_unrelated_repository_context: RepositoryId =
        Context::from_str(unrelated_repository_id.strip_prefix("urc-").unwrap())
            .unwrap()
            .into();

    // user has test_permission for a given repo in their token
    assert!(
        user_permissions(&extensions, test_repository_context)
            .contains(&"test_permission".to_string())
    );
    // user has other_permission for a given repo in their token
    assert!(
        user_permissions(&extensions, test_repository_context)
            .contains(&"other_permission".to_string())
    );
    // user doesn't have test_permission2 for a given repo in their token
    assert!(
        !user_permissions(&extensions, test_repository_context)
            .contains(&"test_permission2".to_string())
    );
    // user doesn't have test_permission for an unrelated repository
    assert!(
        !user_permissions(&extensions, test_unrelated_repository_context)
            .contains(&"test_permission".to_string())
    );
    // user doesn't have other_permission for an unrelated repository
    assert!(
        !user_permissions(&extensions, test_unrelated_repository_context)
            .contains(&"other_permission".to_string())
    );
}

#[test]
fn finds_existing_matching_permission_with_wildcard_repo() {
    let mut extensions = Extensions::new();
    let test_repository_id = "urc-0194b726b34e72b0b45550b88a967076".to_string();
    let unrelated_repository_id = "urc-0192ae48ccf17060bc1ba9d04f6acb2f".to_string();
    let mut test_authz_token = AuthorizationToken::default();

    let test_resource_permission = ResourcePermission {
        resource_id: test_repository_id.clone(),
        permission: vec![
            "test_permission".to_string(),
            "unique_permission".to_string(),
        ],
    };
    let test_wildcard_resource_permission = ResourcePermission {
        resource_id: "urc-*".to_string().clone(),
        permission: vec![
            "test_permission".to_string(),
            "test_wildcard_permission".to_string(),
            "another_wildcard_permission".to_string(),
        ],
    };

    test_authz_token.resources = Some(vec![
        test_resource_permission.clone(),
        test_wildcard_resource_permission.clone(),
    ]);
    extensions.insert(test_authz_token.clone());

    let test_repository_context: RepositoryId =
        Context::from_str(test_repository_id.strip_prefix("urc-").unwrap())
            .unwrap()
            .into();
    let test_unrelated_repository_context: RepositoryId =
        Context::from_str(unrelated_repository_id.strip_prefix("urc-").unwrap())
            .unwrap()
            .into();

    // user has test_permission for a given repo in their token
    assert!(
        user_permissions(&extensions, test_repository_context)
            .contains(&"test_permission".to_string())
    );
    // user has unique_permission for a given repo in their token
    assert!(
        user_permissions(&extensions, test_repository_context)
            .contains(&"unique_permission".to_string())
    );
    // user has test_wildcard_permission for a given repo — through the wildcard resource
    assert!(
        user_permissions(&extensions, test_repository_context)
            .contains(&"test_wildcard_permission".to_string())
    );
    // user also has test_permission for an unrelated repository — through the wildcard resource
    assert!(
        user_permissions(&extensions, test_unrelated_repository_context)
            .contains(&"test_permission".to_string())
    );

    // user doesn't have unique_permission for an unrelated repository
    assert!(
        !user_permissions(&extensions, test_unrelated_repository_context)
            .contains(&"unique_permission".to_string())
    );
}

#[test]
fn can_admin_lock_with_direct_permission_claim() {
    let mut extensions = Extensions::new();
    let test_repository_id = "urc-0194b726b34e72b0b45550b88a967076".to_string();
    let unrelated_repository_id = "urc-0192ae48ccf17060bc1ba9d04f6acb2f".to_string();
    let mut test_authz_token = AuthorizationToken::default();

    let test_resource_permission = ResourcePermission {
        resource_id: test_repository_id.clone(),
        permission: vec!["test_permission".to_string(), "migrate".to_string()],
    };

    test_authz_token.resources = Some(vec![test_resource_permission.clone()]);
    extensions.insert(test_authz_token.clone());

    let test_repository_context: RepositoryId =
        Context::from_str(test_repository_id.strip_prefix("urc-").unwrap())
            .unwrap()
            .into();
    let test_unrelated_repository_context: RepositoryId =
        Context::from_str(unrelated_repository_id.strip_prefix("urc-").unwrap())
            .unwrap()
            .into();

    // as user has "migrate" permission for a given repo, they CAN admin lock that repo
    assert!(can_admin_lock(&extensions, test_repository_context));

    // as user doesn't have "migrate" permission for an unrelated repo, they CAN'T admin lock that repo
    assert!(!can_admin_lock(
        &extensions,
        test_unrelated_repository_context
    ));
}

#[test]
fn can_admin_lock_with_wildcard_permission_claim() {
    let mut extensions = Extensions::new();
    let test_repository_id = "urc-0194b726b34e72b0b45550b88a967076".to_string();
    let unrelated_repository_id = "urc-0192ae48ccf17060bc1ba9d04f6acb2f".to_string();
    let mut test_authz_token = AuthorizationToken::default();

    let test_resource_permission = ResourcePermission {
        resource_id: test_repository_id.clone(),
        permission: vec!["test_permission".to_string(), "migrate".to_string()],
    };

    let test_wildcard_resource_permission = ResourcePermission {
        resource_id: "urc-*".to_string().clone(),
        permission: vec![
            "migrate".to_string(),
            "test_wildcard_permission".to_string(),
        ],
    };

    test_authz_token.resources = Some(vec![
        test_resource_permission.clone(),
        test_wildcard_resource_permission.clone(),
    ]);
    extensions.insert(test_authz_token.clone());

    let test_repository_context: RepositoryId =
        Context::from_str(test_repository_id.strip_prefix("urc-").unwrap())
            .unwrap()
            .into();
    let test_unrelated_repository_context: RepositoryId =
        Context::from_str(unrelated_repository_id.strip_prefix("urc-").unwrap())
            .unwrap()
            .into();

    // as user has "migrate" permission for a given repo, they CAN admin lock that repo
    assert!(can_admin_lock(&extensions, test_repository_context));

    // user doesn't have direct "migrate" permission for an unrelated repo
    // but they have a wildcard token with "migrate", so they should be able to admin lock arbitrary repo
    assert!(can_admin_lock(
        &extensions,
        test_unrelated_repository_context
    ));
}

/// Regression: two entries matching the same partition merge
/// their permission lists. The old reader stopped at the first matching
/// entry, so whichever permission sat in a later entry was lost.
#[test]
fn user_permissions_merge_across_matching_entries() {
    let mut extensions = Extensions::new();
    let test_repository_id = "urc-0194b726b34e72b0b45550b88a967076".to_string();
    let mut test_authz_token = AuthorizationToken::default();

    let test_resource_permissions = vec![
        ResourcePermission {
            resource_id: test_repository_id.clone(),
            permission: vec!["push".to_string()],
        },
        ResourcePermission {
            resource_id: "urc-*".to_string(),
            permission: vec!["migrate".to_string()],
        },
        ResourcePermission {
            resource_id: test_repository_id.clone(),
            permission: vec!["obliterate".to_string()],
        },
    ];

    test_authz_token.resources = Some(test_resource_permissions);
    extensions.insert(test_authz_token.clone());

    let test_repository_context: RepositoryId =
        Context::from_str(test_repository_id.strip_prefix("urc-").unwrap())
            .unwrap()
            .into();

    assert_eq!(
        user_permissions(&extensions, test_repository_context),
        vec![
            "push".to_string(),
            "migrate".to_string(),
            "obliterate".to_string()
        ]
    );
}

/// Regression: a `urc-*` grant now satisfies `can_obliterate`
/// and `is_owner_or_admin`, not just `can_admin_lock`. The old
/// `user_permissions` reader never looked at the wildcard entry, so the
/// three checks disagreed about one token.
#[test]
fn wildcard_grant_satisfies_every_action_check() {
    let mut extensions = Extensions::new();
    let test_repository_id = "urc-0194b726b34e72b0b45550b88a967076".to_string();
    let unrelated_repository_id = "urc-0192ae48ccf17060bc1ba9d04f6acb2f".to_string();
    let mut test_authz_token = AuthorizationToken::default();

    let test_wildcard_resource_permission = ResourcePermission {
        resource_id: "urc-*".to_string(),
        permission: vec![
            "obliterate".to_string(),
            "admin".to_string(),
            "migrate".to_string(),
        ],
    };

    test_authz_token.resources = Some(vec![test_wildcard_resource_permission]);
    extensions.insert(test_authz_token.clone());

    for repository_id in [test_repository_id, unrelated_repository_id] {
        let repository_context: RepositoryId =
            Context::from_str(repository_id.strip_prefix("urc-").unwrap())
                .unwrap()
                .into();
        assert!(can_obliterate(&extensions, repository_context));
        assert!(is_owner_or_admin(&extensions, repository_context));
        assert!(can_admin_lock(&extensions, repository_context));
    }
}

#[test]
fn no_grant_denies_every_action_check() {
    let mut extensions = Extensions::new();
    let mut test_authz_token = AuthorizationToken::default();
    let test_resource_permissions = Vec::new();
    test_authz_token.resources = Some(test_resource_permissions);
    extensions.insert(test_authz_token.clone());

    let test_repository_context: RepositoryId =
        Context::from_str("0194b726b34e72b0b45550b88a967076")
            .unwrap()
            .into();

    assert!(!is_owner_or_admin(&extensions, test_repository_context));
    assert!(!can_obliterate(&extensions, test_repository_context));
    assert!(!can_admin_lock(&extensions, test_repository_context));
}

mod timeout_grpc_tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn returns_ok_when_future_succeeds_within_timeout() {
        let fut = async { Ok::<_, Status>(42) };
        let result = timeout_grpc(Duration::from_secs(1), fut).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn preserves_original_error_when_future_fails_within_timeout() {
        let fut = async { Err::<i32, _>(Status::not_found("missing")) };
        let result = timeout_grpc(Duration::from_secs(1), fut).await;
        let status = result.unwrap_err();
        assert_eq!(status.code(), Code::NotFound);
        assert_eq!(status.message(), "missing");
    }

    #[tokio::test]
    async fn returns_cancelled_when_future_exceeds_timeout() {
        let fut = async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok::<_, Status>(42)
        };
        let result = timeout_grpc(Duration::from_millis(10), fut).await;
        let status = result.unwrap_err();
        assert_eq!(status.code(), Code::Cancelled);
        assert!(status.message().contains("timeout"));
    }
}

mod filter_slow_down_tests {
    use lore_base::error::SlowDown;

    use super::*;

    #[test]
    fn state_ok_passes_through() {
        let result: Result<i32, StateError> = Ok(42);
        let filtered = result.filter_slow_down().unwrap();
        assert_eq!(filtered.unwrap(), 42);
    }

    #[test]
    fn state_slow_down_returns_resource_exhausted() {
        let result: Result<i32, StateError> = Err(StateError::from(SlowDown));
        let status = result.filter_slow_down().unwrap_err();
        assert_eq!(status.code(), Code::ResourceExhausted);
    }

    #[test]
    fn state_error_passes_through() {
        let result: Result<i32, StateError> = Err(StateError::internal("other error"));
        let filtered = result.filter_slow_down().unwrap();
        let underlying_error = filtered.expect_err("Should be err");
        assert!(!underlying_error.is_slow_down());
    }

    #[test]
    fn metadata_ok_passes_through() {
        let result: Result<i32, MetadataError> = Ok(42);
        let filtered = result.filter_slow_down().unwrap();
        assert_eq!(filtered.unwrap(), 42);
    }

    #[test]
    fn metadata_slow_down_returns_resource_exhausted() {
        let result: Result<i32, MetadataError> = Err(MetadataError::from(SlowDown));
        let status = result.filter_slow_down().unwrap_err();
        assert_eq!(status.code(), Code::ResourceExhausted);
    }

    #[test]
    fn metadata_error_passes_through() {
        let result: Result<i32, MetadataError> = Err(MetadataError::internal("other error"));
        let filtered = result.filter_slow_down().unwrap();
        let underlying_error = filtered.expect_err("Should be err");
        assert!(!underlying_error.is_slow_down());
    }

    #[test]
    fn store_ok_passes_through() {
        let result: Result<i32, StoreError> = Ok(42);
        let filtered = result.filter_slow_down().unwrap();
        assert_eq!(filtered.unwrap(), 42);
    }

    #[test]
    fn store_slow_down_returns_resource_exhausted() {
        let result: Result<i32, StoreError> = Err(StoreError::from(SlowDown));
        let status = result.filter_slow_down().unwrap_err();
        assert_eq!(status.code(), Code::ResourceExhausted);
    }

    #[test]
    fn store_error_passes_through() {
        let result: Result<i32, StoreError> = Err(StoreError::internal("other error"));
        let filtered = result.filter_slow_down().unwrap();
        let underlying_error = filtered.expect_err("Should be err");
        assert!(!underlying_error.is_slow_down());
    }

    #[test]
    fn branch_slow_down_returns_resource_exhausted() {
        let result: Result<i32, BranchError> = Err(BranchError::from(SlowDown));
        let status = result.filter_slow_down().unwrap_err();
        assert_eq!(status.code(), Code::ResourceExhausted);
    }

    #[test]
    fn branch_error_passes_through() {
        let result: Result<i32, BranchError> = Err(BranchError::internal("other error"));
        let filtered = result.filter_slow_down().unwrap();
        let underlying_error = filtered.expect_err("Should be err");
        assert!(!underlying_error.is_slow_down());
    }

    #[test]
    fn repository_slow_down_returns_resource_exhausted() {
        let result: Result<i32, RepositoryError> = Err(RepositoryError::from(SlowDown));
        let status = result.filter_slow_down().unwrap_err();
        assert_eq!(status.code(), Code::ResourceExhausted);
    }

    #[test]
    fn immutable_slow_down_returns_resource_exhausted() {
        let result: Result<i32, ImmutableError> = Err(ImmutableError::from(SlowDown));
        let status = result.filter_slow_down().unwrap_err();
        assert_eq!(status.code(), Code::ResourceExhausted);
    }

    #[test]
    fn branch_metadata_slow_down_returns_resource_exhausted() {
        let result: Result<i32, BranchMetadataError> = Err(BranchMetadataError::from(SlowDown));
        let status = result.filter_slow_down().unwrap_err();
        assert_eq!(status.code(), Code::ResourceExhausted);
    }

    /// `filter_slow_down`'s mapping must survive: flattening every
    /// undiscarded error into the internal arm loses it silently, because
    /// both arms still produce a `Status`.
    #[test]
    fn discarded_error_is_none_and_others_keep_their_status() {
        let absent: Result<i32, StoreError> = Err(StoreError::from(
            lore_base::error::AddressNotFound::from(lore_base::types::Address::default()),
        ));
        assert_eq!(
            none_or_status(absent, StoreError::is_address_not_found).unwrap(),
            None
        );

        let throttled: Result<i32, StoreError> = Err(StoreError::from(SlowDown));
        let status = none_or_status(throttled, StoreError::is_address_not_found)
            .expect_err("an undiscarded error must not become None");
        assert_eq!(status.code(), Code::ResourceExhausted);

        let failed: Result<i32, StoreError> = Err(StoreError::internal("store unusable"));
        let status = none_or_status(failed, StoreError::is_address_not_found)
            .expect_err("an undiscarded error must not become None");
        assert_eq!(status.code(), Code::Internal);
    }

    #[test]
    fn find_slow_down_returns_resource_exhausted() {
        let result: Result<i32, FindError> = Err(FindError::from(SlowDown));
        let status = result.filter_slow_down().unwrap_err();
        assert_eq!(status.code(), Code::ResourceExhausted);
    }

    #[test]
    fn find_error_passes_through() {
        let result: Result<i32, FindError> = Err(FindError::internal("no revision found"));
        let filtered = result.filter_slow_down().unwrap();
        let underlying_error = filtered.expect_err("Should be err");
        assert!(!underlying_error.is_slow_down());
    }

    #[test]
    fn diff_slow_down_returns_resource_exhausted() {
        let result: Result<i32, DiffError> = Err(DiffError::from(SlowDown));
        let status = result.filter_slow_down().unwrap_err();
        assert_eq!(status.code(), Code::ResourceExhausted);
    }

    #[test]
    fn repository_metadata_slow_down_returns_resource_exhausted() {
        let result: Result<i32, RepositoryMetadataError> =
            Err(RepositoryMetadataError::from(SlowDown));
        let status = result.filter_slow_down().unwrap_err();
        assert_eq!(status.code(), Code::ResourceExhausted);
    }
}

mod map_message_handle_error_to_status {
    use lore_server::protocol::storage::messages::MessageHandleError;

    use super::*;

    /// A rejected argument is the client's fault: it answers `InvalidArgument`
    /// with the reason, and is not counted as a server error.
    #[test]
    fn invalid_argument_answers_invalid_argument() {
        let error = MessageHandleError::InvalidArgument("bad key_type".into());

        let status = lore_server::grpc::map_message_handle_error_to_status(&error, None, None);

        assert_eq!(status.code(), Code::InvalidArgument);
        assert_eq!(status.message(), "Invalid argument: bad key_type");
        assert!(!is_code_considered_server_error(&status.code()));
    }
}
