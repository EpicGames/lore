// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::error::NotSupported;
use lore_base::types::*;

use crate::connection::Connection;
use crate::error::ProtocolError;
use crate::types::*;

/// Protocol interface
#[async_trait]
pub trait Protocol: Send + Sync {
    /// Connect to remote storage service
    async fn storage(
        &self,
        connection: Weak<Connection>,
        remote_url: &str,
        auth_url: &str,
        identity: &str,
        repository: RepositoryId,
        index: usize,
    ) -> Result<Arc<dyn Storage>, ProtocolError>;

    /// Connect to remote revision service
    async fn revision(
        &self,
        connection: Weak<Connection>,
        remote_url: &str,
        auth_url: &str,
        identity: &str,
        repository: RepositoryId,
    ) -> Result<Arc<dyn Revision>, ProtocolError>;

    /// Connect to remote repository service
    async fn repository(
        &self,
        connection: Weak<Connection>,
        remote_url: &str,
        auth_url: &str,
        identity: &str,
    ) -> Result<Arc<dyn Repository>, ProtocolError>;

    /// Connect to remote admin service
    async fn admin(
        &self,
        connection: Weak<Connection>,
        remote_url: &str,
        auth_url: &str,
        identity: &str,
        repository: RepositoryId,
    ) -> Result<Arc<dyn Admin>, ProtocolError>;

    /// Connect to remote lock service
    async fn lock(
        &self,
        connection: Weak<Connection>,
        remote_url: &str,
        auth_url: &str,
        identity: &str,
        repository: RepositoryId,
    ) -> Result<Arc<dyn Lock>, ProtocolError>;

    /// Connect to remote environment service
    async fn environment(
        &self,
        connection: Weak<Connection>,
        remote_url: &str,
    ) -> Result<Arc<dyn Environment>, ProtocolError>;
}

/// Storage protocol
#[async_trait]
pub trait Storage: Send + Sync {
    /// Start a session for the given repository and correlation ID.
    /// Returns a raw session ID. The caller is responsible for calling
    /// `session_stop` when done. Prefer `StorageConnector::session()` for
    /// automatic lifecycle management.
    async fn session_start(
        &self,
        repository: RepositoryId,
        correlation_id: &str,
    ) -> Result<u32, ProtocolError>;

    /// Stop an active session, releasing server-side capacity.
    async fn session_stop(&self, session_id: u32) -> Result<(), ProtocolError>;

    /// Get the immutable fragment and payload for address
    async fn get(
        &self,
        session_id: u32,
        address: &Address,
    ) -> Result<(Fragment, Bytes), ProtocolError>;

    /// Get with priority scheduling hint for metadata/tree block reads.
    async fn get_priority(
        &self,
        session_id: u32,
        address: &Address,
    ) -> Result<(Fragment, Bytes), ProtocolError> {
        self.get(session_id, address).await
    }

    /// Get only the fragment metadata for an address — no payload bytes.
    ///
    /// Use this when the caller needs `Fragment` (`flags`, `size_payload`, `size_content`) but
    /// not the payload bytes — e.g. existence-with-size queries, metadata audits. Saves one
    /// full payload transfer per address vs. `get`. The default implementation falls back to
    /// `get` and discards the payload, so transports that haven't been updated still work;
    /// gRPC and QUIC override with optimized wire-level metadata-only paths.
    async fn get_metadata(
        &self,
        session_id: u32,
        address: &Address,
    ) -> Result<Fragment, ProtocolError> {
        let (fragment, _payload) = self.get(session_id, address).await?;
        Ok(fragment)
    }

    /// Return short-lived direct-download URLs for a batch of immutable payloads.
    async fn presign_downloads(
        &self,
        session_id: u32,
        addresses: &[Address],
        expires_in: Duration,
    ) -> Result<Vec<DirectDownload>, ProtocolError> {
        let _ = (session_id, addresses, expires_in);
        Err(ProtocolError::from(NotSupported {
            operation: "direct download".to_string(),
        }))
    }

    /// Put the immutable fragment and optional payload for address
    async fn put(
        &self,
        session_id: u32,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
    ) -> Result<(), ProtocolError>;

    /// Query if the given addresses exist
    async fn query(&self, session_id: u32, address: &[Address]) -> Result<Bytes, ProtocolError>;

    /// Verify the fragment at the given address
    async fn verify(
        &self,
        session_id: u32,
        address: &Address,
        heal: bool,
    ) -> Result<VerifyResult, ProtocolError>;

    /// Copy a fragment from `(source_repository, source_address)` to
    /// `(session.repository, source_address.hash, target_context)`.
    ///
    /// The hash is preserved by the transport (content-addressed); the target context lets the
    /// caller pivot the destination's dedup tag without ever transferring the payload — including
    /// the same-partition different-context case used for in-partition duplication.
    async fn copy(
        &self,
        session_id: u32,
        source_repository: RepositoryId,
        source_address: Address,
        target_context: Context,
    ) -> Result<(), ProtocolError>;

    /// Load a mutable key's value.
    async fn mutable_load(
        &self,
        session_id: u32,
        key: &Hash,
        key_type: KeyType,
    ) -> Result<Hash, ProtocolError> {
        let _ = (session_id, key, key_type);
        Err(ProtocolError::internal("unsupported: mutable_load"))
    }

    /// Store a mutable key-value pair.
    async fn mutable_store(
        &self,
        session_id: u32,
        key: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<(), ProtocolError> {
        let _ = (session_id, key, value, key_type);
        Err(ProtocolError::internal("unsupported: mutable_store"))
    }

    /// Compare-and-swap a mutable key. Returns the current value of the key.
    async fn mutable_compare_and_swap(
        &self,
        session_id: u32,
        key: Hash,
        expected: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<Hash, ProtocolError> {
        let _ = (session_id, key, expected, value, key_type);
        Err(ProtocolError::internal("unsupported: mutable_cas"))
    }

    /// Gracefully close the underlying transport, draining any in-flight streams
    /// before sending the connection close frame. Default implementation is a no-op.
    async fn close(&self) {}
}

/// Revision protocol
#[async_trait]
pub trait Revision: Send + Sync {
    /// Create a branch with the given identifier and name (must match) and parent branch stack
    /// Will return the current LATEST pointer to allow the caller to distinguish created branch
    /// and an already existing branch.
    async fn branch_create(
        &self,
        branch: BranchId,
        name: &str,
        category: &str,
        creator: &str,
        stack: &[BranchPoint],
    ) -> Result<Hash, ProtocolError>;

    /// Destroy the given branch
    async fn branch_delete(&self, branch: BranchId) -> Result<(), ProtocolError>;

    /// Query the branch information
    async fn branch_query(
        &self,
        id: Option<BranchId>,
        name: Option<&str>,
    ) -> Result<BranchQueryResponse, ProtocolError>;

    /// Push a new LATEST pointer for branch. Returns the (new) current LATEST pointer for the branch,
    /// if this is different from the given LATEST pointer the operation failed due to the
    /// LATEST pointer having moved.
    async fn branch_push(
        &self,
        branch: BranchId,
        latest: Hash,
        force: bool,
        fast_forward_merge: bool,
    ) -> Result<BranchPushResponse, ProtocolError>;

    /// List all branches
    async fn branch_list(&self) -> Result<BranchListResponse, ProtocolError>;

    async fn revision_list(
        &self,
        signature: RevisionListStart,
    ) -> Result<RevisionListResponse, ProtocolError>;

    /// Get the current branch metadata hash pointer
    async fn branch_metadata_get(&self, branch: BranchId) -> Result<Hash, ProtocolError>;

    /// Compare-and-swap the branch metadata hash pointer
    async fn branch_metadata_set(
        &self,
        branch: BranchId,
        expected: Hash,
        new: Hash,
    ) -> Result<MetadataSetResult, ProtocolError>;
}

/// Repository protocol
#[async_trait]
pub trait Repository: Send + Sync {
    /// Create a new repository
    #[allow(clippy::too_many_arguments)]
    async fn create(
        &self,
        id: RepositoryId,
        name: &str,
        description: &str,
        default_branch_id: Context,
        default_branch_name: &str,
        creator: &str,
        created: u64,
    ) -> Result<RepositoryData, ProtocolError>;

    /// Delete a repository
    async fn delete(&self, id: RepositoryId) -> Result<(), ProtocolError>;

    /// Get repository metadata from id or name
    async fn query(
        &self,
        id: Option<RepositoryId>,
        name: Option<&str>,
    ) -> Result<RepositoryData, ProtocolError>;

    /// List all repositories
    async fn list(&self) -> Result<Vec<RepositoryData>, ProtocolError>;

    /// Get the current repository metadata hash pointer
    async fn metadata_get(&self, id: RepositoryId) -> Result<Hash, ProtocolError>;

    /// Compare-and-swap the repository metadata hash pointer
    async fn metadata_set(
        &self,
        id: RepositoryId,
        expected: Hash,
        new: Hash,
    ) -> Result<MetadataSetResult, ProtocolError>;
}

/// Admin protocol
#[async_trait]
pub trait Admin: Send + Sync {
    /// Obliterate the payloads and fragments for an address
    async fn obliterate(&self, address: Address) -> Result<(), ProtocolError>;
}

/// Lock protocol
#[async_trait]
pub trait Lock: Send + Sync {
    /// Acquire the lock over the resource(s)
    async fn lock(
        &self,
        resources: &[LockResource],
        owner: Option<&str>,
    ) -> Result<Vec<LockData>, ProtocolError>;

    /// Query the lock(s) on a branch, by an owner or by a description
    async fn query(
        &self,
        branch: Option<Context>,
        owner: Option<&str>,
        description: Option<&str>,
    ) -> Result<Vec<LockData>, ProtocolError>;

    /// Get the locking status of the resource(s)
    async fn status(&self, resources: &[LockResource]) -> Result<Vec<LockData>, ProtocolError>;

    /// Remove the lock over the resource(s)
    async fn unlock(&self, resources: &[LockResource]) -> Result<Vec<LockResource>, ProtocolError>;
}

/// Environment protocol
#[async_trait]
pub trait Environment: Send + Sync {
    /// Get server environment config
    async fn get(&self) -> Result<EnvironmentConfig, ProtocolError>;
}

/// Client-side authentication and authorization protocol trait.
///
/// Covers the full auth lifecycle: obtaining authentication tokens (via
/// interactive browser login, external token exchange, or refresh), exchanging
/// authentication tokens for repository-scoped authorization tokens, and
/// resolving user identities.
#[async_trait]
pub trait Authentication: Send + Sync {
    /// Starts an interactive authentication session.
    async fn start_auth_session(
        &self,
        auth_url: &str,
        client_state: &str,
        correlation_id: &str,
    ) -> Result<AuthSession, ProtocolError>;

    /// Polls for completion of an interactive auth session.
    async fn poll_auth_session(
        &self,
        auth_url: &str,
        client_state: &str,
        session_code: &str,
        correlation_id: &str,
    ) -> Result<Option<AuthenticationToken>, ProtocolError>;

    /// Exchanges an external token for a URC authentication token.
    async fn exchange_external_token(
        &self,
        auth_url: &str,
        token: &str,
        token_type: &str,
        correlation_id: &str,
    ) -> Result<AuthenticationToken, ProtocolError>;

    /// Refreshes the authentication token using a one-time-use refresh token.
    async fn refresh_authentication(
        &self,
        auth_url: &str,
        refresh_token: &str,
        correlation_id: &str,
    ) -> Result<AuthenticationToken, ProtocolError>;

    /// Exchanges an authentication token for an authorization token scoped
    /// to the given repository.
    async fn exchange_for_repository(
        &self,
        auth_url: &str,
        authn_token: &str,
        repository: RepositoryId,
        correlation_id: &str,
    ) -> Result<AuthorizationToken, ProtocolError>;

    /// Exchanges an authentication token for an authorization token scoped
    /// to an arbitrary, non-repository resource identifier. The `resource_id`
    /// is passed through verbatim to the auth backend; the caller is
    /// responsible for any prefix convention.
    async fn exchange_for_custom_resource(
        &self,
        auth_url: &str,
        authn_token: &str,
        resource_id: &str,
        correlation_id: &str,
    ) -> Result<AuthorizationToken, ProtocolError>;

    /// Resolves user IDs to display names.
    async fn get_user_info(
        &self,
        auth_url: &str,
        authz_token: &str,
        repository: RepositoryId,
        user_ids: &[String],
        correlation_id: &str,
    ) -> Result<Vec<ResolvedUser>, ProtocolError>;

    /// Resolves a display name back to a user ID.
    async fn get_user_id(
        &self,
        auth_url: &str,
        authz_token: &str,
        repository: RepositoryId,
        display_name: &str,
        correlation_id: &str,
    ) -> Result<Option<ResolvedUser>, ProtocolError>;
}
