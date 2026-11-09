// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

use dashmap::DashMap;
use dashmap::DashSet;
use dashmap::mapref::entry::Entry;
use lore_revision::lore::RepositoryId;
use lore_storage::CorrelationHold;

use crate::authnz::repository_authorizer::Action;
use crate::authnz::repository_authorizer::Grants;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::authnz::repository_authorizer::VerifiedTokenOwned;
use crate::authnz::repository_authorizer::permits_with;

#[lore_macro::test_pub]
#[lore_macro::test_pub]
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
    /// What the stores keep for the session's correlation id, given up as the session ends.
    pub held: Vec<CorrelationHold>,
}

impl SessionEntry {
    /// Whether the session's caller may perform `action` on the session's
    /// repository — the per-operation check for an action-gated storage
    /// command. Same contract as `RepositoryAuthorizer::permits`: answered
    /// from the grants enumerated at session start, asked of the authorizer
    /// when they were not enumerable, with or without a verified token.
    pub async fn permits(&self, authorizer: &dyn RepositoryAuthorizer, action: Action) -> bool {
        let token = self.token.as_deref().map(VerifiedTokenOwned::as_token);
        permits_with(
            authorizer,
            self.grants.as_ref(),
            token.as_ref(),
            self.repository,
            action,
        )
        .await
    }
}

/// Per-connection session state for the `lore-storage/0.4` protocol.
///
/// Tracks active sessions mapping session IDs to repository, correlation ID, and user ID tuples.
/// Each `start()` always allocates a new session ID — deduplication is handled client-side by
/// `StorageConnector`.
pub struct SessionMap {
    entries: DashMap<u32, SessionEntry>,
    counter: Arc<AtomicU32>,
    /// Whether ids wrap past zero, as a shared counter seeded anywhere in its range must.
    wraps: bool,
}

#[derive(Debug, PartialEq)]
pub enum SessionError {
    LimitReached,
    CounterExhausted,
    NotFound,
}

/// The correlation id a session runs under: the one its client names, or a fresh one for a
/// client that names none.
pub fn correlation_id_or_new(correlation_id: String) -> String {
    if correlation_id.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        correlation_id
    }
}

impl Default for SessionMap {
    fn default() -> Self {
        Self {
            entries: DashMap::new(),
            counter: Arc::new(AtomicU32::new(1)),
            wraps: false,
        }
    }
}

impl SessionMap {
    /// A map drawing its session ids from `counter`, shared with other maps, so an id a closed
    /// connection handed out names no session on another until the counter wraps. Zero, which
    /// means no session, is skipped when the counter wraps, and so is an id still in use here.
    pub fn sharing_ids(counter: Arc<AtomicU32>) -> Self {
        Self {
            entries: DashMap::new(),
            counter,
            wraps: true,
        }
    }

    /// Start a new session, which keeps `held` until it stops or the map drops. Always allocates
    /// a fresh session ID — deduplication is the client's responsibility (`StorageConnector`).
    pub fn start(
        &self,
        repository: RepositoryId,
        correlation_id: String,
        user_id: String,
        grants: Option<Grants>,
        token: Option<Arc<VerifiedTokenOwned>>,
        held: Vec<CorrelationHold>,
    ) -> Result<(u32, String), SessionError> {
        if self.entries.len() >= MAX_CONCURRENT_SESSIONS as usize {
            return Err(SessionError::LimitReached);
        }

        let correlation_id = correlation_id_or_new(correlation_id);

        let session_id = self.insert_fresh(SessionEntry {
            repository,
            correlation_id: correlation_id.clone(),
            user_id,
            grants,
            token,
            authorized_sources: Arc::new(DashSet::new()),
            held,
        })?;

        Ok((session_id, correlation_id))
    }

    /// Inserts `entry` under the first id the counter gives that names no session here, as a
    /// counter that wrapped may give one still in use.
    ///
    /// The `entry` shard lock is held for the check and the insert alone, across no await.
    #[allow(clippy::disallowed_methods)]
    fn insert_fresh(&self, entry: SessionEntry) -> Result<u32, SessionError> {
        loop {
            let session_id = self.next_id()?;
            if let Entry::Vacant(slot) = self.entries.entry(session_id) {
                slot.insert(entry);
                return Ok(session_id);
            }
        }
    }

    /// The next id the counter gives, skipping zero where ids wrap.
    fn next_id(&self) -> Result<u32, SessionError> {
        let session_id = self.counter.fetch_add(1, Ordering::Relaxed);
        if session_id != 0 {
            return Ok(session_id);
        }
        if !self.wraps {
            return Err(SessionError::CounterExhausted);
        }
        Ok(self.counter.fetch_add(1, Ordering::Relaxed))
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
