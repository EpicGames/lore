// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

use dashmap::DashSet;
use lore_revision::lore::RepositoryId;
use lore_server::authnz::repository_authorizer::Action;
use lore_server::authnz::repository_authorizer::Grants;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::VerifiedTokenOwned;
use lore_server::protocol::storage::session::*;
use lore_storage::CorrelationHold;
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
            held: Vec::new(),
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
        .start(
            random(),
            "corr-1".into(),
            String::new(),
            None,
            None,
            Vec::new(),
        )
        .unwrap();
    assert_eq!(id, 1);
}

#[test]
fn start_increments_session_id() {
    let map = SessionMap::default();
    let repo = random::<RepositoryId>();
    let (id1, _) = map
        .start(repo, "corr-1".into(), String::new(), None, None, Vec::new())
        .unwrap();
    let (id2, _) = map
        .start(repo, "corr-2".into(), String::new(), None, None, Vec::new())
        .unwrap();
    assert_eq!(id1, 1);
    assert_eq!(id2, 2);
}

#[test]
fn start_always_allocates_new_id() {
    let map = SessionMap::default();
    let repo = random::<RepositoryId>();
    let (id1, _) = map
        .start(repo, "corr-1".into(), String::new(), None, None, Vec::new())
        .unwrap();
    let (id2, _) = map
        .start(repo, "corr-1".into(), String::new(), None, None, Vec::new())
        .unwrap();
    assert_ne!(id1, id2);
}

#[test]
fn start_empty_correlation_generates_uuid() {
    let map = SessionMap::default();
    let repo = random::<RepositoryId>();
    let (id1, corr1) = map
        .start(repo, String::new(), String::new(), None, None, Vec::new())
        .unwrap();
    let (id2, corr2) = map
        .start(repo, String::new(), String::new(), None, None, Vec::new())
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
        .start(repo, "corr-1".into(), String::new(), None, None, Vec::new())
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
        .start(
            random(),
            "corr-1".into(),
            String::new(),
            None,
            None,
            Vec::new(),
        )
        .unwrap();
    map.stop(id).unwrap();
    assert_eq!(map.stop(id), Err(SessionError::NotFound));
}

#[test]
fn start_after_stop_allocates_new_id() {
    let map = SessionMap::default();
    let repo = random::<RepositoryId>();
    let (id1, _) = map
        .start(repo, "corr-1".into(), String::new(), None, None, Vec::new())
        .unwrap();
    map.stop(id1).unwrap();
    let (id2, _) = map
        .start(repo, "corr-1".into(), String::new(), None, None, Vec::new())
        .unwrap();
    assert_ne!(id1, id2);
}

#[test]
fn get_returns_entry_with_user_id() {
    let map = SessionMap::default();
    let repo = random::<RepositoryId>();
    let (id, _) = map
        .start(
            repo,
            "corr-1".into(),
            "user-42".into(),
            None,
            None,
            Vec::new(),
        )
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
        map.start(
            repo,
            format!("corr-{i}"),
            String::new(),
            None,
            None,
            Vec::new(),
        )
        .unwrap();
    }
    assert_eq!(
        map.start(
            repo,
            "one-more".into(),
            String::new(),
            None,
            None,
            Vec::new()
        ),
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
            .start(
                repo,
                format!("corr-{i}"),
                String::new(),
                None,
                None,
                Vec::new(),
            )
            .unwrap();
        ids.push(id);
    }
    assert_eq!(
        map.start(
            repo,
            "blocked".into(),
            String::new(),
            None,
            None,
            Vec::new()
        ),
        Err(SessionError::LimitReached)
    );
    map.stop(ids[0]).unwrap();
    map.start(repo, "freed".into(), String::new(), None, None, Vec::new())
        .unwrap();
}

/// Maps sharing a counter skip zero, which names no session, as the counter wraps.
#[test]
fn shared_ids_skip_zero_as_the_counter_wraps() {
    let map = SessionMap::sharing_ids(Arc::new(AtomicU32::new(u32::MAX)));
    let repo = random::<RepositoryId>();

    let (last, _) = map
        .start(repo, "last".into(), String::new(), None, None, Vec::new())
        .unwrap();
    let (wrapped, _) = map
        .start(
            repo,
            "wrapped".into(),
            String::new(),
            None,
            None,
            Vec::new(),
        )
        .unwrap();

    assert_eq!((last, wrapped), (u32::MAX, 1));
}

/// A counter that wrapped onto an id still in use gives the next one, and the session holding the
/// id keeps it.
#[test]
fn a_wrapped_counter_skips_an_id_still_in_use() {
    let counter = Arc::new(AtomicU32::new(7));
    let map = SessionMap::sharing_ids(counter.clone());
    let (held, holder) = (random::<RepositoryId>(), random::<RepositoryId>());
    let (id, _) = map
        .start(held, "held".into(), String::new(), None, None, Vec::new())
        .unwrap();
    counter.store(id, Ordering::Relaxed);

    let (next, _) = map
        .start(holder, "next".into(), String::new(), None, None, Vec::new())
        .unwrap();

    assert_eq!(next, id + 1);
    assert_eq!(map.get(id).map(|entry| entry.repository), Some(held));
}

/// An id one connection's map issued names no session in another's that shares its counter.
#[test]
fn an_id_from_one_connection_names_no_session_on_another() {
    let counter = Arc::new(AtomicU32::new(1));
    let (first, second) = (
        SessionMap::sharing_ids(counter.clone()),
        SessionMap::sharing_ids(counter),
    );
    let repo = random::<RepositoryId>();

    let (issued, _) = first
        .start(repo, "first".into(), String::new(), None, None, Vec::new())
        .unwrap();
    let (other, _) = second
        .start(repo, "second".into(), String::new(), None, None, Vec::new())
        .unwrap();

    assert_ne!(issued, other);
    assert!(second.get(issued).is_none());
}

/// A session keeps what it was started holding until it stops, and a session still open keeps
/// its own until its map drops, as the map of a closed connection does.
#[test]
fn a_session_keeps_its_holds_until_it_stops_or_its_map_drops() {
    let held = Arc::new(());
    let hold = || -> Vec<CorrelationHold> { vec![Box::new(held.clone())] };
    let map = SessionMap::default();
    let repo = random::<RepositoryId>();

    let (stopped, _) = map
        .start(repo, "shared".into(), String::new(), None, None, hold())
        .unwrap();
    map.start(repo, "shared".into(), String::new(), None, None, hold())
        .unwrap();
    assert_eq!(Arc::strong_count(&held), 3);

    map.stop(stopped).unwrap();
    assert_eq!(Arc::strong_count(&held), 2, "the stopped session's hold");

    drop(map);
    assert_eq!(Arc::strong_count(&held), 1, "the open session's hold");
}
