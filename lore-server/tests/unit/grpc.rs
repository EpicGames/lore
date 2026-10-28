// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod environment;
mod forwarded_lock_service;
mod forwarded_repository;
mod forwarded_requests;
mod forwarded_revision;
mod handlers;
mod lock_service;
mod notification_service;
mod push_protected;
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
use lore_server::authnz::repository_authorizer::Action;
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
            _action: Action,
        ) -> Result<(), Status> {
            self.async_entered.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn check_repository_access_sync(
            &self,
            _token: Option<&VerifiedToken<'_>>,
            repository_id: RepositoryId,
            action: Action,
        ) -> Option<Result<(), Status>> {
            assert_eq!(action, Action::Read, "a link read asks read");
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

        // Any entry naming the partition grants the link read, whatever it lists.
        let write_only = AuthorizationToken {
            resources: Some(vec![ResourcePermission {
                resource_id: format!("urc-{}", repository(GRANTED)),
                permission: vec!["write".to_string()],
            }]),
            ..Default::default()
        };
        let can_read = link_read_authorizer(&authorizer, &extensions_with_token(write_only));
        assert!(can_read(repository(GRANTED)));

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

mod may_administer {
    use std::collections::HashSet;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use lore_server::authnz::repository_authorizer::Grants;

    use super::*;

    /// Grants a fixed action set and records the questions asked of it.
    /// `enumerates: false` models an authorizer that answers only
    /// per-action questions.
    struct Recording {
        enumerates: bool,
        actions: HashSet<String>,
        asked: Mutex<Vec<String>>,
        enumerated: Mutex<usize>,
    }

    impl Recording {
        fn new(enumerates: bool, actions: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                enumerates,
                actions: actions.iter().map(ToString::to_string).collect(),
                asked: Mutex::new(Vec::new()),
                enumerated: Mutex::new(0),
            })
        }
    }

    #[async_trait]
    impl RepositoryAuthorizer for Recording {
        async fn check_repository_access(
            &self,
            _token: Option<&VerifiedToken<'_>>,
            _repository_id: RepositoryId,
            action: Action,
        ) -> Result<(), Status> {
            self.asked.lock().unwrap().push(action.to_string());
            if self.actions.contains(action.as_str()) {
                Ok(())
            } else {
                Err(Status::permission_denied("not granted"))
            }
        }

        async fn granted_actions(
            &self,
            _token: Option<&VerifiedToken<'_>>,
            _repository_id: RepositoryId,
        ) -> Result<Option<Grants>, Status> {
            if !self.enumerates {
                return Ok(None);
            }
            *self.enumerated.lock().unwrap() += 1;
            Ok(Some(Grants::Actions(self.actions.clone())))
        }
    }

    fn extensions_with_token() -> Extensions {
        let mut extensions = Extensions::new();
        extensions.insert(AuthorizationToken::default());
        extensions.insert(RawToken("raw.jwt".to_string()));
        extensions
    }

    /// An enumerating authorizer answers `owner` and `admin` from one
    /// enumeration, with no per-action question.
    #[tokio::test]
    async fn one_enumeration_answers_owner_and_admin() {
        for (actions, permitted) in [(&["admin"][..], true), (&["write"][..], false)] {
            let recording = Recording::new(true, actions);
            let authorizer: Arc<dyn RepositoryAuthorizer> = recording.clone();
            assert_eq!(
                may_administer_repository(
                    &authorizer,
                    &extensions_with_token(),
                    RepositoryId::default(),
                    "alice",
                    "mallory",
                )
                .await,
                permitted,
                "{actions:?}"
            );
            assert_eq!(*recording.enumerated.lock().unwrap(), 1);
            assert!(recording.asked.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn a_non_enumerating_authorizer_is_asked_per_action() {
        let recording = Recording::new(false, &["admin"]);
        let authorizer: Arc<dyn RepositoryAuthorizer> = recording.clone();
        assert!(
            may_administer_repository(
                &authorizer,
                &extensions_with_token(),
                RepositoryId::default(),
                "alice",
                "mallory",
            )
            .await
        );
        assert_eq!(
            *recording.asked.lock().unwrap(),
            ["owner".to_string(), "admin".to_string()]
        );
    }

    /// The creator needs `write`, answered from the same one enumeration.
    #[tokio::test]
    async fn the_creator_needs_write() {
        for (actions, permitted) in [(&["write"][..], true), (&["read"][..], false)] {
            let recording = Recording::new(true, actions);
            let authorizer: Arc<dyn RepositoryAuthorizer> = recording.clone();
            assert_eq!(
                may_administer_repository(
                    &authorizer,
                    &extensions_with_token(),
                    RepositoryId::default(),
                    "alice",
                    "alice",
                )
                .await,
                permitted,
                "{actions:?}"
            );
            assert!(recording.asked.lock().unwrap().is_empty());
            assert_eq!(*recording.enumerated.lock().unwrap(), 1);
        }
    }

    /// A non-enumerating authorizer is asked `owner`, `admin` and then
    /// `write`, and `write` permits only the recorded creator.
    #[tokio::test]
    async fn a_non_enumerating_authorizer_decides_the_creator_path_per_action() {
        for (user, permitted) in [("alice", true), ("mallory", false)] {
            let recording = Recording::new(false, &["write"]);
            let authorizer: Arc<dyn RepositoryAuthorizer> = recording.clone();
            assert_eq!(
                may_administer_repository(
                    &authorizer,
                    &extensions_with_token(),
                    RepositoryId::default(),
                    "alice",
                    user,
                )
                .await,
                permitted,
                "{user}"
            );
            assert_eq!(
                *recording.asked.lock().unwrap(),
                [
                    "owner".to_string(),
                    "admin".to_string(),
                    "write".to_string()
                ]
            );
        }
    }

    /// The grants the partition-access layer already enumerated answer, so
    /// the authorizer is not asked again.
    #[tokio::test]
    async fn the_layers_grants_answer_without_another_enumeration() {
        let recording = Recording::new(true, &[]);
        let authorizer: Arc<dyn RepositoryAuthorizer> = recording.clone();
        let mut extensions = extensions_with_token();
        extensions.insert(
            lore_server::authnz::repository_authorizer::PartitionGrants {
                repository_id: RepositoryId::default(),
                grants: Grants::Actions(["admin".to_string()].into()),
            },
        );
        assert!(
            may_administer_repository(
                &authorizer,
                &extensions,
                RepositoryId::default(),
                "alice",
                "mallory",
            )
            .await
        );
        assert_eq!(*recording.enumerated.lock().unwrap(), 0);
        assert!(recording.asked.lock().unwrap().is_empty());
    }

    /// No verified token means no identity to compare, as on a server with
    /// no `[server.auth]` where every caller is `<unknown>` and the recorded
    /// creator is whatever the client sent: `write` alone decides.
    #[tokio::test]
    async fn without_a_token_write_alone_decides() {
        for (actions, permitted) in [(&["write"][..], true), (&["read"][..], false)] {
            let authorizer: Arc<dyn RepositoryAuthorizer> = Recording::new(true, actions);
            assert_eq!(
                may_administer_repository(
                    &authorizer,
                    &Extensions::new(),
                    RepositoryId::default(),
                    "alice",
                    "<unknown>",
                )
                .await,
                permitted,
                "{actions:?}"
            );
        }
    }
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

mod hook_error_to_status {
    use std::time::Duration;

    use lore_server::hooks::HookError;
    use lore_server::hooks::StatusCode;

    use super::*;

    #[test]
    fn unavailable_rejection_answers_unavailable() {
        let error =
            HookError::rejected("auth", "auth service unreachable", StatusCode::Unavailable);

        assert_eq!(hook_error_to_status(error).code(), Code::Unavailable);
    }

    #[test]
    fn timeout_answers_unavailable() {
        let error = HookError::timeout("auth", Duration::from_millis(200));

        assert_eq!(hook_error_to_status(error).code(), Code::Unavailable);
    }

    #[test]
    fn panic_answers_internal() {
        let error = HookError::panic("auth", "boom");

        assert_eq!(hook_error_to_status(error).code(), Code::Internal);
    }
}
