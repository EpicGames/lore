// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

use dashmap::DashMap;
use dashmap::DashSet;
use lore_revision::lore::RepositoryId;

use crate::authnz::repository_authorizer::Grants;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::authnz::repository_authorizer::VerifiedTokenOwned;

pub(crate) const MAX_CONCURRENT_SESSIONS: u32 = 10_000;

pub struct SessionEntry {
    pub repository: RepositoryId,
    pub correlation_id: String,
    pub user_id: String,
    /// The caller's grants on `repository`, when the authorizer could
    /// enumerate them at session start. `None` means per-action checks fall
    /// back to the authorizer with `token`.
    pub grants: Option<Grants>,
    /// The verified token the session was authorized with. `None` on a
    /// server with no verifier configured. Shared: the dispatch clones it per
    /// command, so the clone must stay a refcount bump.
    pub token: Option<Arc<VerifiedTokenOwned>>,
    /// Source repositories a cross-partition copy has already cleared with
    /// `token`. A repeated copy from the same source consults this set
    /// instead of re-asking the authorizer, which for the online authorizer
    /// spares a `CheckUserPermission` round trip per copy. Scoped to the
    /// session, so does not leak permissions for a shared Connection.
    pub authorized_sources: Arc<DashSet<RepositoryId>>,
}

impl SessionEntry {
    /// Whether the session's caller may perform `action` on the session's
    /// repository — the per-operation check for an action-gated storage
    /// command. Same contract as `RepositoryAuthorizer::permits`: answered
    /// from the grants enumerated at session start, asked of the authorizer
    /// when they were not enumerable, and without a verified token nothing
    /// is granted.
    pub async fn permits(&self, authorizer: &dyn RepositoryAuthorizer, action: &str) -> bool {
        if let Some(grants) = &self.grants {
            return grants.permits(action);
        }
        let Some(token) = &self.token else {
            return false;
        };
        authorizer
            .check_repository_access(Some(&token.as_token()), self.repository, Some(action))
            .await
            .is_ok()
    }
}

/// Per-connection session state for the `lore-storage/0.4` protocol.
///
/// Tracks active sessions mapping session IDs to repository, correlation ID, and user ID tuples.
/// Each `start()` always allocates a new session ID — deduplication is handled client-side by
/// `StorageConnector`.
pub struct SessionMap {
    entries: DashMap<u32, SessionEntry>,
    counter: AtomicU32,
}

#[derive(Debug, PartialEq)]
pub enum SessionError {
    LimitReached,
    CounterExhausted,
    NotFound,
}

impl Default for SessionMap {
    fn default() -> Self {
        Self {
            entries: DashMap::new(),
            counter: AtomicU32::new(1),
        }
    }
}

impl SessionMap {
    /// Start a new session. Always allocates a fresh session ID — deduplication
    /// is the client's responsibility (`StorageConnector`).
    pub fn start(
        &self,
        repository: RepositoryId,
        correlation_id: String,
        user_id: String,
        grants: Option<Grants>,
        token: Option<Arc<VerifiedTokenOwned>>,
    ) -> Result<(u32, String), SessionError> {
        if self.entries.len() >= MAX_CONCURRENT_SESSIONS as usize {
            return Err(SessionError::LimitReached);
        }

        let session_id = self.counter.fetch_add(1, Ordering::Relaxed);
        if session_id == 0 {
            return Err(SessionError::CounterExhausted);
        }

        let correlation_id = if correlation_id.is_empty() {
            uuid::Uuid::new_v4().to_string()
        } else {
            correlation_id
        };

        self.entries.insert(
            session_id,
            SessionEntry {
                repository,
                correlation_id: correlation_id.clone(),
                user_id,
                grants,
                token,
                authorized_sources: Arc::new(DashSet::new()),
            },
        );

        Ok((session_id, correlation_id))
    }

    /// Stop an active session.
    pub fn stop(&self, session_id: u32) -> Result<(), SessionError> {
        match self.entries.remove(&session_id) {
            Some(_) => Ok(()),
            None => Err(SessionError::NotFound),
        }
    }

    pub fn get(&self, session_id: u32) -> Option<dashmap::mapref::one::Ref<'_, u32, SessionEntry>> {
        self.entries.get(&session_id)
    }
}

#[cfg(test)]
mod tests {
    use rand::random;

    use super::*;

    mod entry_permits {
        use std::collections::HashSet;

        use async_trait::async_trait;
        use tonic::Status;

        use super::*;
        use crate::auth::jwt::AuthorizationToken;
        use crate::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
        use crate::authnz::repository_authorizer::VerifiedToken;

        /// Non-enumerable authorizer permitting exactly one action.
        struct PolicyOnly(&'static str);

        #[async_trait]
        impl RepositoryAuthorizer for PolicyOnly {
            async fn check_repository_access(
                &self,
                token: Option<&VerifiedToken<'_>>,
                _repository_id: RepositoryId,
                action: Option<&str>,
            ) -> Result<(), Status> {
                if token.is_some() && action == Some(self.0) {
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
            assert!(entry.permits(&PolicyOnly("other"), "migrate").await);
            assert!(
                !entry
                    .permits(&AllowAllRepositoryAuthorizer, "obliterate")
                    .await
            );
        }

        #[tokio::test]
        async fn non_enumerated_sessions_ask_the_authorizer_per_action() {
            let entry = entry(None, Some(owned_token()));
            assert!(entry.permits(&PolicyOnly("migrate"), "migrate").await);
            assert!(!entry.permits(&PolicyOnly("migrate"), "obliterate").await);
        }

        /// Without a verified token nothing is granted, whatever the
        /// authorizer would say — the contract `RepositoryAuthorizer::permits`
        /// keeps for request extensions.
        #[tokio::test]
        async fn no_token_grants_nothing_even_under_allow_all() {
            let entry = entry(None, None);
            assert!(
                !entry
                    .permits(&AllowAllRepositoryAuthorizer, "migrate")
                    .await
            );
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
}
