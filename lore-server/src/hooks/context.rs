// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Hook context for passing operation details to hooks.
//!
//! This module defines [`HookContext`], which provides hooks with read-only
//! operation metadata:
//!
//! - Correlation ID for request tracing
//! - Hook point that triggered the hook
//! - Repository context
//! - Optional user, branch, revision, and metadata
//!
//! The context is immutable once created. Pre hooks can reject requests via
//! their return value, and post hooks only act asynchronously on read-only
//! information.

use std::collections::HashMap;

use lore_base::types::Hash;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;

use crate::hooks::traits::HookPoint;

/// Metadata key for the target branch's name. The v1 branch push, create and
/// delete handlers set it before their pre-handlers run.
pub const METADATA_BRANCH_NAME: &str = "branch_name";

/// Metadata key for whether the caller is a service account, `"true"` or
/// `"false"`. Set wherever [`METADATA_BRANCH_NAME`] is.
pub const METADATA_IS_SERVICE_ACCOUNT: &str = "is_service_account";

/// Context data passed to hooks during execution.
///
/// The context provides immutable operation details to hooks. All fields are
/// set when the context is created via the builder and cannot be modified.
///
/// # Fields
///
/// - `correlation_id` - Unique request identifier for audit trail
/// - `hook_point` - The extension point that triggered this hook
/// - `repository` - The repository context for the operation
/// - `user` - The user performing the operation (optional)
/// - `branch` - The target branch (optional)
/// - `revision` - The target revision (optional)
/// - `revision_number` - The revision number (optional, set after push)
/// - `metadata` - Arbitrary key-value metadata
///
/// # Thread Safety
///
/// `HookContext` is `Send + Sync` and can be safely shared across threads.
/// Since all fields are immutable, no synchronization is required.
///
/// # Hook Execution Model
///
/// - **Pre hooks**: Can read the context and reject requests via return value
/// - **Post hooks**: Run asynchronously with read-only access to the context
///
/// # Example
///
/// ```
/// use lore_server::hooks::{HookContext, HookPoint};
/// use lore_revision::lore::RepositoryId;
///
/// // Create context for a branch push operation
/// let ctx = HookContext::builder()
///     .correlation_id("abc-123")
///     .hook_point(HookPoint::BranchPush)
///     .repository(RepositoryId::default())
///     .user("user@example.com")
///     .build();
///
/// // Read fields
/// assert_eq!(ctx.correlation_id(), "abc-123");
/// assert_eq!(ctx.hook_point(), HookPoint::BranchPush);
/// assert_eq!(ctx.user(), Some("user@example.com"));
///
/// // Get metadata
/// assert!(ctx.get_metadata("key").is_none());
/// ```
#[derive(Clone)]
pub struct HookContext {
    correlation_id: String,
    hook_point: HookPoint,
    repository: RepositoryId,
    user: Option<String>,
    branch: Option<BranchId>,
    revision: Option<Hash>,
    revision_number: Option<u64>,
    metadata: HashMap<String, String>,
}

impl HookContext {
    /// Creates a new `HookContextBuilder` for constructing a context.
    pub fn builder() -> HookContextBuilder {
        HookContextBuilder::new()
    }

    /// Returns the correlation ID for this operation.
    ///
    /// The correlation ID is a unique identifier for the request that can be
    /// used to trace the operation across services and log entries.
    #[inline]
    pub fn correlation_id(&self) -> &str {
        &self.correlation_id
    }

    /// Returns the hook point that triggered this hook execution.
    #[inline]
    pub fn hook_point(&self) -> HookPoint {
        self.hook_point
    }

    /// Returns the repository context for this operation.
    #[inline]
    pub fn repository(&self) -> RepositoryId {
        self.repository
    }

    /// Returns the user for this operation, if set.
    #[inline]
    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    /// Returns the target branch for this operation, if set.
    #[inline]
    pub fn branch(&self) -> Option<BranchId> {
        self.branch
    }

    /// Returns the target revision for this operation, if set.
    #[inline]
    pub fn revision(&self) -> Option<Hash> {
        self.revision
    }

    /// Returns the revision number for this operation, if set.
    #[inline]
    pub fn revision_number(&self) -> Option<u64> {
        self.revision_number
    }

    /// Sets the revision number.
    ///
    /// This allows handlers to set the revision number after the operation
    /// completes but before passing the context to post-hooks.
    #[inline]
    pub fn set_revision_number(&mut self, revision_number: u64) {
        self.revision_number = Some(revision_number);
    }

    /// Returns a specific metadata value.
    ///
    /// Returns `None` if the key doesn't exist.
    #[inline]
    pub fn get_metadata(&self, key: &str) -> Option<&str> {
        self.metadata.get(key).map(|s| s.as_str())
    }

    /// Returns all metadata as a reference to the internal `HashMap`.
    #[inline]
    pub fn metadata(&self) -> &HashMap<String, String> {
        &self.metadata
    }
}

impl std::fmt::Debug for HookContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookContext")
            .field("correlation_id", &self.correlation_id)
            .field("hook_point", &self.hook_point)
            .field("repository", &self.repository)
            .field("user", &self.user)
            .field("branch", &self.branch)
            .field("revision", &self.revision)
            .field("revision_number", &self.revision_number)
            .field("metadata", &self.metadata)
            .finish()
    }
}

/// Builder for constructing [`HookContext`] instances.
///
/// # Example
///
/// ```
/// use lore_server::hooks::{HookContext, HookPoint};
/// use lore_revision::lore::{BranchId, RepositoryId};
/// use lore_base::types::Hash;
///
/// let ctx = HookContext::builder()
///     .correlation_id("abc-123")
///     .hook_point(HookPoint::BranchPush)
///     .repository(RepositoryId::default())
///     .branch(BranchId::default())
///     .user("user@example.com")
///     .revision(Hash::default())
///     .metadata("key", "value")
///     .build();
///
/// assert_eq!(ctx.correlation_id(), "abc-123");
/// assert!(ctx.branch().is_some());
/// assert!(ctx.revision().is_some());
/// assert_eq!(ctx.get_metadata("key"), Some("value"));
/// ```
#[derive(Default)]
pub struct HookContextBuilder {
    correlation_id: Option<String>,
    hook_point: Option<HookPoint>,
    repository: Option<RepositoryId>,
    user: Option<String>,
    branch: Option<BranchId>,
    revision: Option<Hash>,
    revision_number: Option<u64>,
    metadata: HashMap<String, String>,
}

impl HookContextBuilder {
    /// Creates a new builder with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the correlation ID.
    ///
    /// This is required before calling `build()`.
    pub fn correlation_id(mut self, correlation_id: impl Into<String>) -> Self {
        self.correlation_id = Some(correlation_id.into());
        self
    }

    /// Sets the hook point.
    ///
    /// This is required before calling `build()`.
    pub fn hook_point(mut self, hook_point: HookPoint) -> Self {
        self.hook_point = Some(hook_point);
        self
    }

    /// Sets the repository context.
    ///
    /// This is required before calling `build()`.
    pub fn repository(mut self, repository: RepositoryId) -> Self {
        self.repository = Some(repository);
        self
    }

    /// Sets the user.
    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    /// Sets the branch context.
    pub fn branch(mut self, branch: BranchId) -> Self {
        self.branch = Some(branch);
        self
    }

    /// Sets the revision hash.
    pub fn revision(mut self, revision: Hash) -> Self {
        self.revision = Some(revision);
        self
    }

    /// Sets the revision number.
    pub fn revision_number(mut self, revision_number: u64) -> Self {
        self.revision_number = Some(revision_number);
        self
    }

    /// Adds a metadata entry.
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Builds the [`HookContext`].
    ///
    /// # Panics
    ///
    /// Panics if `correlation_id`, `hook_point`, or `repository` is not set.
    pub fn build(self) -> HookContext {
        self.try_build().unwrap_or_else(|e| panic!("{e}"))
    }

    /// Tries to build the [`HookContext`], returning an error if required fields are missing.
    ///
    /// # Errors
    ///
    /// Returns an error string if `correlation_id`, `hook_point`, or `repository` is not set.
    pub fn try_build(self) -> Result<HookContext, &'static str> {
        let correlation_id = self
            .correlation_id
            .ok_or("correlation_id is required for HookContext")?;
        let hook_point = self
            .hook_point
            .ok_or("hook_point is required for HookContext")?;
        let repository = self
            .repository
            .ok_or("repository is required for HookContext")?;

        Ok(HookContext {
            correlation_id,
            hook_point,
            repository,
            user: self.user,
            branch: self.branch,
            revision: self.revision,
            revision_number: self.revision_number,
            metadata: self.metadata,
        })
    }
}
