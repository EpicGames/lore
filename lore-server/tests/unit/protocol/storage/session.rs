// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use dashmap::DashSet;
use lore_revision::lore::RepositoryId;
use lore_server::authnz::repository_authorizer::Action;
use lore_server::authnz::repository_authorizer::Grants;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::VerifiedTokenOwned;
use lore_server::protocol::storage::session::*;
use rand::random;

mod entry_permits {
    use std::collections::HashSet;

    use async_trait::async_trait;
    use lore_server::auth::jwt::AuthorizationToken;
    use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
    use lore_server::authnz::repository_authorizer::VerifiedToken;
    use tonic::Status;

    use super::*;

    /// Non-enumerable authorizer permitting exactly one action.
    struct PolicyOnly(&'static str);

    #[async_trait]
    impl RepositoryAuthorizer for PolicyOnly {
        async fn check_repository_access(
            &self,
            token: Option<&VerifiedToken<'_>>,
            _repository_id: RepositoryId,
            action: Action,
        ) -> Result<(), Status> {
            if token.is_some() && action.as_str() == self.0 {
                Ok(())
            } else {
                Err(Status::permission_denied("Not permitted"))
            }
        }
    }

    fn entry(grants: Option<Grants>, token: Option<Arc<VerifiedTokenOwned>>) -> SessionEntry {
        SessionEntry {
            repository: random(),
            correlation_id: "corr".to_string(),
            user_id: String::new(),
            grants,
            token,
            authorized_sources: Arc::new(DashSet::new()),
        }
    }

    fn owned_token() -> Arc<VerifiedTokenOwned> {
        Arc::new(VerifiedTokenOwned {
            raw: "raw.jwt".to_string(),
            claims: AuthorizationToken::default(),
        })
    }

    /// The grants stored at session start answer first — proven by
    /// pairing them with authorizers that would answer the opposite.
    #[tokio::test]
    async fn stored_grants_answer_before_the_authorizer() {
        let entry = entry(
            Some(Grants::Actions(HashSet::from(["migrate".to_string()]))),
            Some(owned_token()),
        );
        assert!(entry.permits(&PolicyOnly("other"), Action::Migrate).await);
        assert!(
            !entry
                .permits(&AllowAllRepositoryAuthorizer, Action::Obliterate)
                .await
        );
    }

    #[tokio::test]
    async fn non_enumerated_sessions_ask_the_authorizer_per_action() {
        let entry = entry(None, Some(owned_token()));
        assert!(entry.permits(&PolicyOnly("migrate"), Action::Migrate).await);
        assert!(
            !entry
                .permits(&PolicyOnly("migrate"), Action::Obliterate)
                .await
        );
    }

    /// Without a verified token the authorizer still decides — the
    /// contract `RepositoryAuthorizer::permits` keeps for request
    /// extensions. Allow-all answers from its fixed set.
    #[tokio::test]
    async fn no_token_is_asked_of_the_authorizer() {
        let entry = entry(None, None);
        assert!(
            entry
                .permits(&AllowAllRepositoryAuthorizer, Action::Write)
                .await
        );
        assert!(
            !entry
                .permits(&AllowAllRepositoryAuthorizer, Action::Migrate)
                .await
        );
        assert!(!entry.permits(&PolicyOnly("migrate"), Action::Migrate).await);
    }
}

#[test]
fn start_assigns_session_id_from_one() {
    let map = SessionMap::default();
    let (id, _) = map
        .start(random(), "corr-1".into(), String::new(), None, None)
        .unwrap();
    assert_eq!(id, 1);
}

#[test]
fn start_increments_session_id() {
    let map = SessionMap::default();
    let repo = random::<RepositoryId>();
    let (id1, _) = map
        .start(repo, "corr-1".into(), String::new(), None, None)
        .unwrap();
    let (id2, _) = map
        .start(repo, "corr-2".into(), String::new(), None, None)
        .unwrap();
    assert_eq!(id1, 1);
    assert_eq!(id2, 2);
}

#[test]
fn start_always_allocates_new_id() {
    let map = SessionMap::default();
    let repo = random::<RepositoryId>();
    let (id1, _) = map
        .start(repo, "corr-1".into(), String::new(), None, None)
        .unwrap();
    let (id2, _) = map
        .start(repo, "corr-1".into(), String::new(), None, None)
        .unwrap();
    assert_ne!(id1, id2);
}

#[test]
fn start_empty_correlation_generates_uuid() {
    let map = SessionMap::default();
    let repo = random::<RepositoryId>();
    let (id1, corr1) = map
        .start(repo, String::new(), String::new(), None, None)
        .unwrap();
    let (id2, corr2) = map
        .start(repo, String::new(), String::new(), None, None)
        .unwrap();
    assert_ne!(id1, id2);
    assert!(!corr1.is_empty());
    assert!(!corr2.is_empty());
    assert_ne!(corr1, corr2);
}

#[test]
fn stop_removes_session() {
    let map = SessionMap::default();
    let repo = random::<RepositoryId>();
    let (id, _) = map
        .start(repo, "corr-1".into(), String::new(), None, None)
        .unwrap();
    assert!(map.get(id).is_some());
    map.stop(id).unwrap();
    assert!(map.get(id).is_none());
}

#[test]
fn stop_unknown_returns_not_found() {
    let map = SessionMap::default();
    assert_eq!(map.stop(999), Err(SessionError::NotFound));
}

#[test]
fn stop_already_stopped_returns_not_found() {
    let map = SessionMap::default();
    let (id, _) = map
        .start(random(), "corr-1".into(), String::new(), None, None)
        .unwrap();
    map.stop(id).unwrap();
    assert_eq!(map.stop(id), Err(SessionError::NotFound));
}

#[test]
fn start_after_stop_allocates_new_id() {
    let map = SessionMap::default();
    let repo = random::<RepositoryId>();
    let (id1, _) = map
        .start(repo, "corr-1".into(), String::new(), None, None)
        .unwrap();
    map.stop(id1).unwrap();
    let (id2, _) = map
        .start(repo, "corr-1".into(), String::new(), None, None)
        .unwrap();
    assert_ne!(id1, id2);
}

#[test]
fn get_returns_entry_with_user_id() {
    let map = SessionMap::default();
    let repo = random::<RepositoryId>();
    let (id, _) = map
        .start(repo, "corr-1".into(), "user-42".into(), None, None)
        .unwrap();
    let entry = map.get(id).unwrap();
    assert_eq!(entry.repository, repo);
    assert_eq!(entry.correlation_id, "corr-1");
    assert_eq!(entry.user_id, "user-42");
}

#[test]
fn get_returns_none_for_unknown() {
    let map = SessionMap::default();
    assert!(map.get(42).is_none());
}

#[test]
fn concurrent_session_limit() {
    let map = SessionMap::default();
    let repo = random::<RepositoryId>();
    for i in 0..MAX_CONCURRENT_SESSIONS {
        map.start(repo, format!("corr-{i}"), String::new(), None, None)
            .unwrap();
    }
    assert_eq!(
        map.start(repo, "one-more".into(), String::new(), None, None),
        Err(SessionError::LimitReached)
    );
}

#[test]
fn limit_freed_by_stop() {
    let map = SessionMap::default();
    let repo = random::<RepositoryId>();
    let mut ids = Vec::new();
    for i in 0..MAX_CONCURRENT_SESSIONS {
        let (id, _) = map
            .start(repo, format!("corr-{i}"), String::new(), None, None)
            .unwrap();
        ids.push(id);
    }
    assert_eq!(
        map.start(repo, "blocked".into(), String::new(), None, None),
        Err(SessionError::LimitReached)
    );
    map.stop(ids[0]).unwrap();
    map.start(repo, "freed".into(), String::new(), None, None)
        .unwrap();
}
