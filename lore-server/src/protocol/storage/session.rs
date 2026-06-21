// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

use dashmap::DashMap;
use dashmap::DashSet;
use lore_revision::lore::RepositoryId;

pub(crate) const MAX_CONCURRENT_SESSIONS: u32 = 10_000;

pub struct SessionEntry {
    pub repository: RepositoryId,
    pub correlation_id: String,
    pub user_id: String,
    pub can_write: bool,
}

/// Per-connection session state for the `lore-storage/0.4` protocol.
///
/// Tracks active sessions mapping session IDs to repository, correlation ID, and user ID tuples,
/// with a set of authorized repositories for Copy checks. Each `start()` always allocates a new
/// session ID — deduplication is handled client-side by `StorageConnector`.
pub struct SessionMap {
    entries: DashMap<u32, SessionEntry>,
    authorized_repos: DashSet<RepositoryId>,
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
            authorized_repos: DashSet::new(),
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
    ) -> Result<(u32, String), SessionError> {
        self.start_with_permissions(repository, correlation_id, user_id, true)
    }

    pub fn start_with_permissions(
        &self,
        repository: RepositoryId,
        correlation_id: String,
        user_id: String,
        can_write: bool,
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

        self.authorized_repos.insert(repository);

        self.entries.insert(
            session_id,
            SessionEntry {
                repository,
                correlation_id: correlation_id.clone(),
                user_id,
                can_write,
            },
        );

        Ok((session_id, correlation_id))
    }

    /// Stop an active session. The repository remains in the authorized set
    /// for Copy source-repo checks.
    pub fn stop(&self, session_id: u32) -> Result<(), SessionError> {
        match self.entries.remove(&session_id) {
            Some(_) => Ok(()),
            None => Err(SessionError::NotFound),
        }
    }

    pub fn get(&self, session_id: u32) -> Option<dashmap::mapref::one::Ref<'_, u32, SessionEntry>> {
        self.entries.get(&session_id)
    }

    /// O(1) check whether a repository has been authorized on this connection
    /// (i.e. had at least one session started for it).
    pub fn is_repository_authorized(&self, repository: RepositoryId) -> bool {
        self.authorized_repos.contains(&repository)
    }
}

#[cfg(test)]
mod tests {
    use rand::random;

    use super::*;

    #[test]
    fn start_assigns_session_id_from_one() {
        let map = SessionMap::default();
        let (id, _) = map.start(random(), "corr-1".into(), String::new()).unwrap();
        assert_eq!(id, 1);
    }

    #[test]
    fn start_increments_session_id() {
        let map = SessionMap::default();
        let repo = random::<RepositoryId>();
        let (id1, _) = map.start(repo, "corr-1".into(), String::new()).unwrap();
        let (id2, _) = map.start(repo, "corr-2".into(), String::new()).unwrap();
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
    }

    #[test]
    fn start_always_allocates_new_id() {
        let map = SessionMap::default();
        let repo = random::<RepositoryId>();
        let (id1, _) = map.start(repo, "corr-1".into(), String::new()).unwrap();
        let (id2, _) = map.start(repo, "corr-1".into(), String::new()).unwrap();
        assert_ne!(id1, id2);
    }

    #[test]
    fn start_empty_correlation_generates_uuid() {
        let map = SessionMap::default();
        let repo = random::<RepositoryId>();
        let (id1, corr1) = map.start(repo, String::new(), String::new()).unwrap();
        let (id2, corr2) = map.start(repo, String::new(), String::new()).unwrap();
        assert_ne!(id1, id2);
        assert!(!corr1.is_empty());
        assert!(!corr2.is_empty());
        assert_ne!(corr1, corr2);
    }

    #[test]
    fn stop_removes_session() {
        let map = SessionMap::default();
        let repo = random::<RepositoryId>();
        let (id, _) = map.start(repo, "corr-1".into(), String::new()).unwrap();
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
        let (id, _) = map.start(random(), "corr-1".into(), String::new()).unwrap();
        map.stop(id).unwrap();
        assert_eq!(map.stop(id), Err(SessionError::NotFound));
    }

    #[test]
    fn start_after_stop_allocates_new_id() {
        let map = SessionMap::default();
        let repo = random::<RepositoryId>();
        let (id1, _) = map.start(repo, "corr-1".into(), String::new()).unwrap();
        map.stop(id1).unwrap();
        let (id2, _) = map.start(repo, "corr-1".into(), String::new()).unwrap();
        assert_ne!(id1, id2);
    }

    #[test]
    fn get_returns_entry_with_user_id() {
        let map = SessionMap::default();
        let repo = random::<RepositoryId>();
        let (id, _) = map.start(repo, "corr-1".into(), "user-42".into()).unwrap();
        let entry = map.get(id).unwrap();
        assert_eq!(entry.repository, repo);
        assert_eq!(entry.correlation_id, "corr-1");
        assert_eq!(entry.user_id, "user-42");
        assert!(entry.can_write);
    }

    #[test]
    fn start_with_permissions_records_write_access() {
        let map = SessionMap::default();
        let repo = random::<RepositoryId>();
        let (id, _) = map
            .start_with_permissions(repo, "corr-1".into(), "user-42".into(), false)
            .unwrap();
        let entry = map.get(id).unwrap();
        assert!(!entry.can_write);
    }

    #[test]
    fn get_returns_none_for_unknown() {
        let map = SessionMap::default();
        assert!(map.get(42).is_none());
    }

    #[test]
    fn is_repository_authorized() {
        let map = SessionMap::default();
        let repo_a = random::<RepositoryId>();
        let repo_b = random::<RepositoryId>();
        map.start(repo_a, "corr-1".into(), String::new()).unwrap();

        assert!(map.is_repository_authorized(repo_a));
        assert!(!map.is_repository_authorized(repo_b));
    }

    #[test]
    fn stop_does_not_remove_authorized_repo() {
        let map = SessionMap::default();
        let repo = random::<RepositoryId>();
        let (id, _) = map.start(repo, "corr-1".into(), String::new()).unwrap();
        map.stop(id).unwrap();
        assert!(map.is_repository_authorized(repo));
    }

    #[test]
    fn concurrent_session_limit() {
        let map = SessionMap::default();
        let repo = random::<RepositoryId>();
        for i in 0..MAX_CONCURRENT_SESSIONS {
            map.start(repo, format!("corr-{i}"), String::new()).unwrap();
        }
        assert_eq!(
            map.start(repo, "one-more".into(), String::new()),
            Err(SessionError::LimitReached)
        );
    }

    #[test]
    fn limit_freed_by_stop() {
        let map = SessionMap::default();
        let repo = random::<RepositoryId>();
        let mut ids = Vec::new();
        for i in 0..MAX_CONCURRENT_SESSIONS {
            let (id, _) = map.start(repo, format!("corr-{i}"), String::new()).unwrap();
            ids.push(id);
        }
        assert_eq!(
            map.start(repo, "blocked".into(), String::new()),
            Err(SessionError::LimitReached)
        );
        map.stop(ids[0]).unwrap();
        map.start(repo, "freed".into(), String::new()).unwrap();
    }
}
