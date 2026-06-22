// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Hook traits for compile-time event-driven extensions.
//!
//! This module defines the core traits and types for the hook system, which
//! allows code to be executed at specific points in the server lifecycle
//! (such as branch push, branch create, etc.) without modifying core handlers.
//!
//! # Architecture
//!
//! The hook system consists of:
//!
//! - [`Hook`] trait - Defines the interface for hook implementations
//! - [`HookFactory`] trait - Creates hook instances from configuration
//! - [`HookPoint`] enum - Defines the extension points where hooks can be triggered
//! - [`HookError`] enum - Error types for hook operations
//! - [`StatusCode`] enum - Semantic status codes for hook rejections
//!
//! # Three-Phase Execution Model
//!
//! The hook system supports a three-phase execution model:
//!
//! 1. **Pre-handler** (synchronous): Called BEFORE the operation
//!    - Read-only access to context
//!    - Can veto/reject by returning `Err(HookError::Rejected)`
//!    - Subject to timeout (default 200ms)
//!    - Panics are caught and isolated
//!
//! 2. **Response handler** (synchronous): Called AFTER the operation succeeds
//!    - Read-only access to context
//!    - Returns [`HookResponse`] with optional data for the client response
//!    - Errors are logged but do NOT fail the operation
//!    - Subject to same timeout as pre-handler
//!
//! 3. **Post-handler** (asynchronous): Called AFTER the operation
//!    - Read-only access to context
//!    - Spawned in separate tokio task
//!    - Does not block the response
//!    - Errors are logged but not propagated
//!
//! # Example
//!
//! ```
//! use lore_server::hooks::{Hook, HookContext, HookError, HookPoint, StatusCode};
//! use async_trait::async_trait;
//! use lore_base::types::Context;
//!
//! struct ComplianceHook;
//!
//! #[async_trait]
//! impl Hook for ComplianceHook {
//!     fn name(&self) -> &'static str {
//!         "compliance"
//!     }
//!
//!     fn hook_points(&self) -> &'static [HookPoint] {
//!         &[HookPoint::BranchPush, HookPoint::BranchCreate]
//!     }
//!
//!     fn pre_handler(&self, ctx: &HookContext) -> Result<(), HookError> {
//!         // Check compliance rules before operation
//!         if let Some(user) = ctx.user() {
//!             if user.starts_with("blocked_") {
//!                 return Err(HookError::rejected(
//!                     self.name(),
//!                     format!("User '{}' is not authorized", user),
//!                     StatusCode::PermissionDenied,
//!                 ));
//!             }
//!         }
//!         Ok(())
//!     }
//!
//!     async fn post_handler(&self, ctx: &HookContext) -> Result<(), HookError> {
//!         // Audit logging after operation completes
//!         println!(
//!             "Audit: correlation_id={}, hook_point={:?}",
//!             ctx.correlation_id(),
//!             ctx.hook_point(),
//!         );
//!         Ok(())
//!     }
//! }
//!
//! // Usage
//! # fn main() {
//! let hook = ComplianceHook;
//! assert_eq!(hook.name(), "compliance");
//! assert!(hook.hook_points().contains(&HookPoint::BranchPush));
//! # }
//! ```

use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;

use crate::hooks::context::HookContext;

/// Extension points where hooks can be triggered.
///
/// Each variant corresponds to a specific server operation where registered
/// hooks will be executed before the operation completes.
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub enum HookPoint {
    /// Triggered when a branch is pushed to.
    /// Context includes: repository, branch, user, revision
    BranchPush,

    /// Triggered when a new branch is created.
    /// Context includes: repository, branch, user
    BranchCreate,

    /// Triggered when a branch is deleted.
    /// Context includes: repository, branch, user
    BranchDelete,

    /// Triggered when a new repository is created.
    /// Context includes: repository, user
    RepositoryCreate,

    /// Triggered when data is obliterated.
    /// Context includes: repository
    Obliterate,
}

impl HookPoint {
    /// Returns all defined hook points.
    pub fn all() -> &'static [Self] {
        &[
            Self::BranchPush,
            Self::BranchCreate,
            Self::BranchDelete,
            Self::RepositoryCreate,
            Self::Obliterate,
        ]
    }
}

impl std::fmt::Display for HookPoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BranchPush => write!(f, "BranchPush"),
            Self::BranchCreate => write!(f, "BranchCreate"),
            Self::BranchDelete => write!(f, "BranchDelete"),
            Self::RepositoryCreate => write!(f, "RepositoryCreate"),
            Self::Obliterate => write!(f, "Obliterate"),
        }
    }
}

/// Hooks return a `StatusCode` to indicate the type of rejection,
/// which is then mapped to the appropriate gRPC status by the handler.
///
/// # Example
///
/// ```
/// use lore_server::hooks::StatusCode;
///
/// // Use specific codes for different rejection types
/// let permission = StatusCode::PermissionDenied;
/// let rate_limit = StatusCode::ResourceExhausted;
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusCode {
    /// Policy or authorization violations → gRPC `PERMISSION_DENIED`
    PermissionDenied,

    /// General business rule violations → gRPC `FAILED_PRECONDITION`
    FailedPrecondition,

    /// Rate limiting, quota exceeded → gRPC `RESOURCE_EXHAUSTED`
    ResourceExhausted,

    /// Validation failures → gRPC `INVALID_ARGUMENT`
    InvalidArgument,

    /// Conflict/concurrency issues → gRPC `ABORTED`
    Aborted,

    /// Internal failure → gRPC `INTERNAL`
    Internal,
}

impl std::fmt::Display for StatusCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PermissionDenied => write!(f, "PERMISSION_DENIED"),
            Self::FailedPrecondition => write!(f, "FAILED_PRECONDITION"),
            Self::ResourceExhausted => write!(f, "RESOURCE_EXHAUSTED"),
            Self::InvalidArgument => write!(f, "INVALID_ARGUMENT"),
            Self::Aborted => write!(f, "ABORTED"),
            Self::Internal => write!(f, "INTERNAL"),
        }
    }
}

/// Errors that can occur during hook operations.
#[derive(Clone, Debug, Error)]
pub enum HookError {
    /// Hook explicitly rejected the operation with a semantic status code.
    /// Pre-handler returns this to veto the operation.
    #[error("Hook '{hook_name}' rejected: {message}")]
    Rejected {
        /// Name of the hook that rejected the operation
        hook_name: String,
        /// Reason for the rejection
        message: String,
        /// Semantic status code for gRPC mapping
        status: StatusCode,
    },

    /// Hook execution failed with an error.
    /// The hook returned an error, which may veto the operation.
    #[error("Hook '{hook_name}' execution failed: {message}")]
    ExecutionFailed {
        /// Name of the hook that failed
        hook_name: String,
        /// Error message describing the failure
        message: String,
    },

    /// Hook execution timed out.
    /// The hook did not complete within the configured timeout.
    #[error("Hook '{hook_name}' timed out after {timeout:?}")]
    Timeout {
        /// Name of the hook that timed out
        hook_name: String,
        /// The timeout duration that was exceeded
        timeout: Duration,
    },

    /// Hook panicked during execution.
    /// The panic was caught and converted to this error.
    #[error("Hook '{hook_name}' panicked: {message}")]
    Panic {
        /// Name of the hook that panicked
        hook_name: String,
        /// Panic message if available
        message: String,
    },

    /// Hook configuration is invalid.
    #[error("Hook '{hook_name}' configuration error: {message}")]
    ConfigError {
        /// Name of the hook with invalid config
        hook_name: String,
        /// Error message describing the configuration issue
        message: String,
    },

    /// Hook initialization failed.
    #[error("Hook '{hook_name}' initialization failed: {message}")]
    InitError {
        /// Name of the hook that failed to initialize
        hook_name: String,
        /// Error message describing the initialization failure
        message: String,
    },
}

impl HookError {
    /// Creates a rejection error with a semantic status code.
    ///
    /// # Example
    ///
    /// ```
    /// use lore_server::hooks::{HookError, StatusCode};
    ///
    /// let error = HookError::rejected(
    ///     "compliance",
    ///     "Branch name violates naming policy",
    ///     StatusCode::PermissionDenied,
    /// );
    ///
    /// assert_eq!(error.hook_name(), "compliance");
    /// ```
    pub fn rejected(
        hook_name: impl Into<String>,
        message: impl Into<String>,
        status: StatusCode,
    ) -> Self {
        Self::Rejected {
            hook_name: hook_name.into(),
            message: message.into(),
            status,
        }
    }

    /// Creates a rejection error with default status (`Internal`).
    ///
    /// # Example
    ///
    /// ```
    /// use lore_server::hooks::HookError;
    ///
    /// let error = HookError::rejected_default("validator", "Validation failed");
    /// assert_eq!(error.hook_name(), "validator");
    /// ```
    pub fn rejected_default(hook_name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::rejected(hook_name, message, StatusCode::Internal)
    }

    /// Creates an execution failed error.
    pub fn execution_failed(hook_name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::ExecutionFailed {
            hook_name: hook_name.into(),
            message: message.into(),
        }
    }

    /// Creates a timeout error.
    pub fn timeout(hook_name: impl Into<String>, timeout: Duration) -> Self {
        Self::Timeout {
            hook_name: hook_name.into(),
            timeout,
        }
    }

    /// Creates a panic error.
    pub fn panic(hook_name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Panic {
            hook_name: hook_name.into(),
            message: message.into(),
        }
    }

    /// Creates a configuration error.
    pub fn config_error(hook_name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::ConfigError {
            hook_name: hook_name.into(),
            message: message.into(),
        }
    }

    /// Creates an initialization error.
    pub fn init_error(hook_name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::InitError {
            hook_name: hook_name.into(),
            message: message.into(),
        }
    }

    /// Returns the name of the hook that caused the error.
    pub fn hook_name(&self) -> &str {
        match self {
            Self::Rejected { hook_name, .. }
            | Self::ExecutionFailed { hook_name, .. }
            | Self::Timeout { hook_name, .. }
            | Self::Panic { hook_name, .. }
            | Self::ConfigError { hook_name, .. }
            | Self::InitError { hook_name, .. } => hook_name,
        }
    }

    /// Returns the status code if this is a Rejected error.
    pub fn status_code(&self) -> Option<StatusCode> {
        match self {
            Self::Rejected { status, .. } => Some(*status),
            _ => None,
        }
    }
}

/// Response data that hooks can contribute to the client response.
///
/// Returned by [`Hook::response_handler`] and merged by the dispatcher.
/// Messages from multiple hooks are joined with newlines.
#[derive(Debug, Clone, Default)]
pub struct HookResponse {
    /// Optional message to include in the response to the client.
    pub message: Option<String>,
}

impl HookResponse {
    /// Creates an empty response with no message.
    pub fn empty() -> Self {
        Self { message: None }
    }

    /// Creates a response with a message.
    pub fn with_message(message: impl Into<String>) -> Self {
        Self {
            message: Some(message.into()),
        }
    }
}

/// Trait for hook implementations with three-phase execution model.
///
/// # Execution Model
///
/// 1. **Pre-handler** (synchronous): Called BEFORE the operation
///    - Read-only access to context
///    - Can veto/reject by returning `Err(HookError::Rejected)`
///    - Subject to timeout (default 200ms)
///    - Panics are caught and isolated
///
/// 2. **Response handler** (synchronous): Called AFTER the operation succeeds
///    - Read-only access to context
///    - Returns [`HookResponse`] with optional data for the client response
///    - Errors are logged but do NOT fail the operation
///    - Subject to same timeout as pre-handler
///    - Panics are caught and isolated
///
/// 3. **Post-handler** (asynchronous): Called AFTER the operation
///    - Read-only access to context
///    - Spawned in separate tokio task
///    - Does not block the response
///    - Errors are logged but not propagated
///
/// # Default Implementations
///
/// Both `pre_handler()` and `post_handler()` have default implementations
/// that do nothing (return `Ok(())`). Hooks only need to implement the
/// methods they need.
///
/// # Example
///
/// ```
/// use lore_server::hooks::{Hook, HookContext, HookError, HookPoint, StatusCode};
/// use async_trait::async_trait;
/// use lore_base::types::Context;
///
/// /// A hook that validates branch names (pre) and logs operations (post)
/// struct BranchPolicyHook {
///     denied_prefixes: Vec<String>,
/// }
///
/// #[async_trait]
/// impl Hook for BranchPolicyHook {
///     fn name(&self) -> &'static str {
///         "branch_policy"
///     }
///
///     fn hook_points(&self) -> &'static [HookPoint] {
///         &[HookPoint::BranchCreate, HookPoint::BranchPush]
///     }
///
///     fn pre_handler(&self, ctx: &HookContext) -> Result<(), HookError> {
///         // Validate branch name in pre-handler (can veto)
///         if let Some(branch) = ctx.branch() {
///             let branch_str = format!("{:?}", branch);
///             for prefix in &self.denied_prefixes {
///                 if branch_str.starts_with(prefix) {
///                     return Err(HookError::rejected(
///                         self.name(),
///                         format!("Branch name cannot start with '{}'", prefix),
///                         StatusCode::PermissionDenied,
///                     ));
///                 }
///             }
///         }
///         Ok(())
///     }
///
///     async fn post_handler(&self, ctx: &HookContext) -> Result<(), HookError> {
///         // Log the operation (non-blocking, errors don't affect response)
///         println!(
///             "Branch operation completed: correlation_id={}",
///             ctx.correlation_id()
///         );
///         Ok(())
///     }
/// }
///
/// # fn main() {
/// let hook = BranchPolicyHook {
///     denied_prefixes: vec!["temp-".to_string(), "wip-".to_string()],
/// };
/// assert_eq!(hook.name(), "branch_policy");
/// # }
/// ```
#[async_trait]
pub trait Hook: Send + Sync {
    /// Returns the unique name of this hook.
    ///
    /// This name is used for:
    /// - Configuration lookup (`[hooks.<name>]`)
    /// - Logging and error messages
    /// - Identifying the hook in audit trails
    fn name(&self) -> &'static str;

    /// Returns the hook points this hook responds to.
    ///
    /// The hook will only be executed for operations matching these points.
    /// Return an empty slice to disable the hook for all points.
    fn hook_points(&self) -> &'static [HookPoint];

    /// Synchronous pre-handler called BEFORE the operation executes.
    ///
    /// # Semantics
    ///
    /// - Executes inline in the request handler thread
    /// - All enabled hooks' pre-handlers complete before operation begins
    /// - Return `Ok(())` to allow the operation to proceed
    /// - Return `Err(HookError::Rejected { ... })` to veto the operation
    ///
    /// # Default Implementation
    ///
    /// Returns `Ok(())` (no-op, allows operation to proceed)
    ///
    /// # Arguments
    ///
    /// * `ctx` - The hook context containing operation details
    ///
    /// # Errors
    ///
    /// Return `HookError::Rejected` to veto the operation with a semantic
    /// status code that maps to gRPC status.
    fn pre_handler(&self, ctx: &HookContext) -> Result<(), HookError> {
        let _ = ctx;
        Ok(())
    }

    /// Synchronous response handler called AFTER the operation succeeds,
    /// before building the response.
    ///
    /// # Semantics
    ///
    /// - Executes inline in the request handler thread
    /// - Returns a [`HookResponse`] with optional data for the client response
    /// - Errors are logged but do NOT fail the operation
    /// - Subject to same timeout as pre-handler
    /// - Panics are caught and isolated
    ///
    /// # Default Implementation
    ///
    /// Returns an empty `HookResponse` (no message)
    fn response_handler(&self, ctx: &HookContext) -> Result<HookResponse, HookError> {
        let _ = ctx;
        Ok(HookResponse::default())
    }

    /// Asynchronous post-handler called AFTER the operation completes.
    ///
    /// # Semantics
    ///
    /// - Spawned in a separate tokio task
    /// - Does NOT block the response to the client
    /// - Only called if the operation was successful
    /// - Errors are logged but do not affect the response
    ///
    /// # Default Implementation
    ///
    /// Returns `Ok(())` (no-op)
    ///
    /// # Arguments
    ///
    /// * `ctx` - The hook context containing operation details
    ///
    /// # Errors
    ///
    /// Errors are logged but not propagated to the client.
    async fn post_handler(&self, ctx: &HookContext) -> Result<(), HookError> {
        let _ = ctx;
        Ok(())
    }
}

/// Factory trait for creating hook instances from configuration.
///
/// Implementations are responsible for:
/// - Deserializing configuration from TOML
/// - Validating configuration values
/// - Constructing configured hook instances
///
/// # Example
///
/// ```
/// use lore_server::hooks::{Hook, HookContext, HookError, HookFactory, HookPoint};
/// use async_trait::async_trait;
/// use lore_base::types::Context;
///
/// // A simple hook implementation
/// struct SimpleHook {
///     message: String,
/// }
///
/// #[async_trait]
/// impl Hook for SimpleHook {
///     fn name(&self) -> &'static str {
///         "simple"
///     }
///
///     fn hook_points(&self) -> &'static [HookPoint] {
///         &[HookPoint::BranchPush]
///     }
///
///     fn pre_handler(&self, _ctx: &HookContext) -> Result<(), HookError> {
///         println!("{}", self.message);
///         Ok(())
///     }
/// }
///
/// // A factory that creates SimpleHook instances
/// struct SimpleHookFactory;
///
/// impl HookFactory for SimpleHookFactory {
///     fn name(&self) -> &'static str {
///         "simple"
///     }
///
///     fn create(&self, config: &toml::Value) -> Result<Box<dyn Hook>, HookError> {
///         let message = config
///             .get("message")
///             .and_then(|v| v.as_str())
///             .unwrap_or("default message")
///             .to_string();
///
///         Ok(Box::new(SimpleHook { message }))
///     }
/// }
///
/// # fn main() {
/// let factory = SimpleHookFactory;
/// assert_eq!(factory.name(), "simple");
///
/// // Create hook with default config
/// let config = toml::Value::Table(toml::map::Map::new());
/// let hook = factory.create(&config).unwrap();
/// assert_eq!(hook.name(), "simple");
/// # }
/// ```
pub trait HookFactory: Send + Sync {
    /// Returns the unique name of hooks created by this factory.
    ///
    /// This must match the configuration section name (`[hooks.<name>]`).
    fn name(&self) -> &'static str;

    /// Creates a new hook instance from the provided configuration.
    ///
    /// # Arguments
    ///
    /// * `config` - TOML configuration value for this hook
    ///
    /// # Returns
    ///
    /// A boxed hook instance on success, or a [`HookError`] on failure.
    ///
    /// # Errors
    ///
    /// - [`HookError::ConfigError`] - Configuration parsing/validation failed
    /// - [`HookError::InitError`] - Hook initialization failed
    fn create(&self, config: &toml::Value) -> Result<Box<dyn Hook>, HookError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hook_point_display() {
        assert_eq!(HookPoint::BranchPush.to_string(), "BranchPush");
        assert_eq!(HookPoint::BranchCreate.to_string(), "BranchCreate");
        assert_eq!(HookPoint::BranchDelete.to_string(), "BranchDelete");
        assert_eq!(HookPoint::RepositoryCreate.to_string(), "RepositoryCreate");
        assert_eq!(HookPoint::Obliterate.to_string(), "Obliterate");
    }

    #[test]
    fn test_hook_point_all() {
        let all = HookPoint::all();
        assert_eq!(all.len(), 5);
        assert!(all.contains(&HookPoint::BranchPush));
        assert!(all.contains(&HookPoint::BranchCreate));
        assert!(all.contains(&HookPoint::BranchDelete));
        assert!(all.contains(&HookPoint::RepositoryCreate));
        assert!(all.contains(&HookPoint::Obliterate));
    }

    #[test]
    fn test_hook_point_hash_eq() {
        use std::collections::HashSet;

        let mut set = HashSet::new();
        set.insert(HookPoint::BranchPush);
        set.insert(HookPoint::BranchPush); // Duplicate

        assert_eq!(set.len(), 1);
        assert!(set.contains(&HookPoint::BranchPush));
    }

    #[test]
    fn test_status_code_display() {
        assert_eq!(
            StatusCode::PermissionDenied.to_string(),
            "PERMISSION_DENIED"
        );
        assert_eq!(
            StatusCode::FailedPrecondition.to_string(),
            "FAILED_PRECONDITION"
        );
        assert_eq!(
            StatusCode::ResourceExhausted.to_string(),
            "RESOURCE_EXHAUSTED"
        );
        assert_eq!(StatusCode::InvalidArgument.to_string(), "INVALID_ARGUMENT");
        assert_eq!(StatusCode::Aborted.to_string(), "ABORTED");
    }

    #[test]
    fn test_hook_error_rejected() {
        let err = HookError::rejected("test_hook", "Access denied", StatusCode::PermissionDenied);
        assert_eq!(err.hook_name(), "test_hook");
        assert_eq!(err.status_code(), Some(StatusCode::PermissionDenied));
        let msg = err.to_string();
        assert!(msg.contains("test_hook"));
        assert!(msg.contains("rejected"));
        assert!(msg.contains("Access denied"));
    }

    #[test]
    fn test_hook_error_rejected_default() {
        let err = HookError::rejected_default("test_hook", "Validation failed");
        assert_eq!(err.hook_name(), "test_hook");
        assert_eq!(err.status_code(), Some(StatusCode::Internal));
    }

    #[test]
    fn test_hook_error_execution_failed() {
        let err = HookError::execution_failed("test_hook", "Something went wrong");
        assert_eq!(err.hook_name(), "test_hook");
        assert_eq!(err.status_code(), None);
        let msg = err.to_string();
        assert!(msg.contains("test_hook"));
        assert!(msg.contains("execution failed"));
        assert!(msg.contains("Something went wrong"));
    }

    #[test]
    fn test_hook_error_timeout() {
        let err = HookError::timeout("test_hook", Duration::from_secs(5));
        assert_eq!(err.hook_name(), "test_hook");
        assert_eq!(err.status_code(), None);
        let msg = err.to_string();
        assert!(msg.contains("test_hook"));
        assert!(msg.contains("timed out"));
        assert!(msg.contains("5s"));
    }

    #[test]
    fn test_hook_error_panic() {
        let err = HookError::panic("test_hook", "panic message");
        assert_eq!(err.hook_name(), "test_hook");
        assert_eq!(err.status_code(), None);
        let msg = err.to_string();
        assert!(msg.contains("test_hook"));
        assert!(msg.contains("panicked"));
        assert!(msg.contains("panic message"));
    }

    #[test]
    fn test_hook_error_config_error() {
        let err = HookError::config_error("test_hook", "missing field 'pattern'");
        assert_eq!(err.hook_name(), "test_hook");
        let msg = err.to_string();
        assert!(msg.contains("test_hook"));
        assert!(msg.contains("configuration error"));
        assert!(msg.contains("missing field 'pattern'"));
    }

    #[test]
    fn test_hook_error_init_error() {
        let err = HookError::init_error("test_hook", "failed to connect");
        assert_eq!(err.hook_name(), "test_hook");
        let msg = err.to_string();
        assert!(msg.contains("test_hook"));
        assert!(msg.contains("initialization failed"));
        assert!(msg.contains("failed to connect"));
    }

    #[test]
    fn test_hook_response_empty() {
        let response = HookResponse::empty();
        assert!(response.message.is_none());
    }

    #[test]
    fn test_hook_response_default() {
        let response = HookResponse::default();
        assert!(response.message.is_none());
    }

    #[test]
    fn test_hook_response_with_message() {
        let response = HookResponse::with_message("hello");
        assert_eq!(response.message, Some("hello".to_string()));
    }

    struct MinimalHook;

    #[async_trait]
    impl Hook for MinimalHook {
        fn name(&self) -> &'static str {
            "minimal"
        }

        fn hook_points(&self) -> &'static [HookPoint] {
            &[HookPoint::BranchPush]
        }
    }

    #[test]
    fn test_hook_default_pre_handler() {
        use lore_revision::lore::RepositoryId;

        let hook = MinimalHook;
        let ctx = HookContext::builder()
            .correlation_id("test")
            .hook_point(HookPoint::BranchPush)
            .repository(RepositoryId::default())
            .build();

        let result = hook.pre_handler(&ctx);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_hook_default_post_handler() {
        use lore_revision::lore::RepositoryId;

        let hook = MinimalHook;
        let ctx = HookContext::builder()
            .correlation_id("test")
            .hook_point(HookPoint::BranchPush)
            .repository(RepositoryId::default())
            .build();

        let result = hook.post_handler(&ctx).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_hook_default_response_handler() {
        use lore_revision::lore::RepositoryId;

        let hook = MinimalHook;
        let ctx = HookContext::builder()
            .correlation_id("test")
            .hook_point(HookPoint::BranchPush)
            .repository(RepositoryId::default())
            .build();

        let result = hook.response_handler(&ctx);
        assert!(result.is_ok());
        assert!(result.unwrap().message.is_none());
    }

    struct RejectingHook;

    #[async_trait]
    impl Hook for RejectingHook {
        fn name(&self) -> &'static str {
            "rejecting"
        }

        fn hook_points(&self) -> &'static [HookPoint] {
            &[HookPoint::BranchPush]
        }

        fn pre_handler(&self, _ctx: &HookContext) -> Result<(), HookError> {
            Err(HookError::rejected(
                self.name(),
                "Operation not allowed",
                StatusCode::PermissionDenied,
            ))
        }
    }

    #[test]
    fn test_hook_pre_handler_rejection() {
        use lore_revision::lore::RepositoryId;

        let hook = RejectingHook;
        let ctx = HookContext::builder()
            .correlation_id("test")
            .hook_point(HookPoint::BranchPush)
            .repository(RepositoryId::default())
            .build();

        let result = hook.pre_handler(&ctx);
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert_eq!(err.hook_name(), "rejecting");
        assert_eq!(err.status_code(), Some(StatusCode::PermissionDenied));
    }
}
