// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Hook dispatcher for executing hooks at specific hook points.
//!
//! The [`HookDispatcher`] is responsible for:
//!
//! - Maintaining a mapping from [`HookPoint`] to registered hooks
//! - Awaiting pre-handlers in the request, cancelled at their timeout
//! - Spawning post-handlers asynchronously in separate tasks
//! - Isolating hook errors and panics to prevent cascade failures
//! - Logging hook execution with correlation IDs for audit trails
//!
//! # Three-Phase Execution Model
//!
//! The dispatcher provides three methods for hook execution:
//!
//! 1. **`dispatch_pre()`** - Asynchronous pre-handler execution, awaited
//!    - All hooks execute in registration order, one at a time
//!    - Each hook is cancelled at its timeout (default 1s, overridable
//!      per hook)
//!    - Panics are caught and isolated
//!    - Returns first error to enable veto capability
//!
//! 2. **`dispatch_response()`** - Synchronous response handler execution
//!    - All hooks execute in registration order
//!    - Returns merged [`HookResponse`] with combined messages
//!    - Errors are logged but not propagated (non-fatal)
//!    - Same timeout and panic isolation as pre-handlers
//!
//! 3. **`spawn_post()`** - Asynchronous post-handler execution
//!    - Each hook runs in an independent tokio task
//!    - Returns immediately (non-blocking)
//!    - Errors are logged but not propagated
//!
//! # Example
//!
//! ```
//! use lore_server::hooks::{HookDispatcher, HookContext, HookPoint};
//! use std::time::Duration;
//! use lore_revision::lore::RepositoryId;
//!
//! // Create an empty dispatcher (no hooks registered)
//! let dispatcher = HookDispatcher::empty();
//!
//! // Check if hooks are registered for a point
//! assert!(!dispatcher.has_hooks(HookPoint::BranchPush));
//! assert_eq!(dispatcher.hook_count(HookPoint::BranchPush), 0);
//! assert_eq!(dispatcher.total_hook_registrations(), 0);
//!
//! // Create a dispatcher from hooks (empty in this example)
//! let hooks = vec![];
//! let dispatcher = HookDispatcher::from_hooks_default(hooks);
//!
//! // Create context for hook dispatch
//! let ctx = HookContext::builder()
//!     .correlation_id("abc-123")
//!     .hook_point(HookPoint::BranchPush)
//!     .repository(RepositoryId::default())
//!     .build();
//!
//! // Dispatch pre-handlers (awaited, can veto):
//! // dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx).await?;
//!
//! // Spawn post-handlers (asynchronous, non-blocking):
//! // dispatcher.spawn_post(HookPoint::BranchPush, ctx);
//! ```

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use futures::FutureExt;
use lore_base::lore_spawn;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::hooks::context::HookContext;
use crate::hooks::traits::Hook;
use crate::hooks::traits::HookError;
use crate::hooks::traits::HookPoint;
use crate::hooks::traits::HookResponse;

/// Default timeout for pre-handler execution.
pub const DEFAULT_PRE_HANDLER_TIMEOUT: Duration = Duration::from_secs(1);

/// Default timeout for post-handler execution.
pub const DEFAULT_POST_HANDLER_TIMEOUT: Duration = Duration::from_secs(30);

/// Dispatcher for executing hooks at specific hook points.
///
/// The dispatcher maintains a mapping from hook points to hooks and handles:
///
/// - Pre-handler execution with timeout protection
/// - Post-handler spawning as independent tasks
/// - Panic isolation between hooks
/// - Error collection and reporting
/// - Audit logging with correlation IDs
///
/// # Thread Safety
///
/// The dispatcher is `Send + Sync` and can be shared across threads via `Arc`.
/// Multiple requests can dispatch hooks concurrently.
pub struct HookDispatcher {
    /// Mapping from hook point to list of hooks that handle that point.
    /// Hooks are stored in registration order.
    hooks_by_point: HashMap<HookPoint, Vec<Arc<dyn Hook>>>,

    /// Timeout for pre-handler execution.
    pre_handler_timeout: Duration,

    /// Timeout for post-handler execution (per task).
    post_handler_timeout: Duration,
}

impl HookDispatcher {
    /// Creates a new dispatcher with the given hooks and timeouts.
    ///
    /// # Arguments
    ///
    /// * `hooks` - List of (name, hook) pairs from enabled hooks
    /// * `pre_handler_timeout` - Maximum time for each pre-handler
    /// * `post_handler_timeout` - Maximum time for each post-handler task
    ///
    /// Hooks are automatically mapped to their declared hook points.
    pub fn new(
        hooks: Vec<(String, Box<dyn Hook>)>,
        pre_handler_timeout: Duration,
        post_handler_timeout: Duration,
    ) -> Self {
        let mut hooks_by_point: HashMap<HookPoint, Vec<Arc<dyn Hook>>> = HashMap::new();

        for (name, hook) in hooks {
            let hook: Arc<dyn Hook> = Arc::from(hook);
            let points = hook.hook_points();

            info!(
                hook_name = name,
                hook_points = ?points,
                "Registering hook with dispatcher"
            );

            for &point in points {
                hooks_by_point.entry(point).or_default().push(hook.clone());
            }
        }

        Self {
            hooks_by_point,
            pre_handler_timeout,
            post_handler_timeout,
        }
    }

    /// Creates a new dispatcher with the given hooks and default timeouts.
    ///
    /// Uses:
    /// - 1s for pre-handler timeout
    /// - 30s for post-handler timeout
    pub fn from_hooks_default(hooks: Vec<(String, Box<dyn Hook>)>) -> Self {
        Self::new(
            hooks,
            DEFAULT_PRE_HANDLER_TIMEOUT,
            DEFAULT_POST_HANDLER_TIMEOUT,
        )
    }

    /// Creates an empty dispatcher with no hooks.
    pub fn empty() -> Self {
        Self {
            hooks_by_point: HashMap::new(),
            pre_handler_timeout: DEFAULT_PRE_HANDLER_TIMEOUT,
            post_handler_timeout: DEFAULT_POST_HANDLER_TIMEOUT,
        }
    }

    /// Returns the number of hooks registered for a specific hook point.
    pub fn hook_count(&self, point: HookPoint) -> usize {
        self.hooks_by_point.get(&point).map_or(0, |v| v.len())
    }

    /// Returns the total number of hook registrations across all points.
    ///
    /// Note: A hook registered for multiple points is counted multiple times.
    pub fn total_hook_registrations(&self) -> usize {
        self.hooks_by_point.values().map(|v| v.len()).sum()
    }

    /// Returns whether any hooks are registered for a specific hook point.
    pub fn has_hooks(&self, point: HookPoint) -> bool {
        self.hook_count(point) > 0
    }

    /// Returns the configured timeout for pre-handler execution.
    pub fn pre_handler_timeout(&self) -> Duration {
        self.pre_handler_timeout
    }

    /// Returns the configured timeout for post-handler execution.
    pub fn post_handler_timeout(&self) -> Duration {
        self.post_handler_timeout
    }

    /// Dispatches pre-handlers for all enabled hooks, awaiting each in turn.
    ///
    /// # Execution Model
    ///
    /// 1. Executes all pre-handlers in registration order, one at a time
    /// 2. Each pre-handler is wrapped with timeout and panic isolation
    /// 3. All pre-handlers execute even if earlier ones fail
    /// 4. Returns the first error encountered (enables veto)
    ///
    /// # Returns
    ///
    /// - `Ok(())` if all pre-handlers succeed
    /// - `Err(HookError)` if any pre-handler fails (first error)
    ///
    /// # Timeouts
    ///
    /// A pre-handler still running at its timeout is dropped at its current
    /// await and reported as [`HookError::Timeout`]. The timeout is the hook's
    /// [`pre_handler_timeout`](Hook::pre_handler_timeout) when set, the
    /// dispatcher's otherwise.
    ///
    /// # Logging
    ///
    /// Each pre-handler execution is logged with:
    /// - `correlation_id`
    /// - `hook_name`
    /// - `hook_point`
    /// - `phase` = "pre"
    /// - `duration_ms`
    /// - `result` (ok/error)
    pub async fn dispatch_pre(&self, point: HookPoint, ctx: &HookContext) -> Result<(), HookError> {
        let hooks = match self.hooks_by_point.get(&point) {
            Some(hooks) if !hooks.is_empty() => hooks,
            _ => return Ok(()),
        };

        let correlation_id = ctx.correlation_id();
        let mut first_error: Option<HookError> = None;

        for hook in hooks {
            let hook_name = hook.name();
            let start = Instant::now();

            let result = self
                .execute_pre_handler_with_isolation(hook.as_ref(), ctx)
                .await;

            let duration = start.elapsed();

            match &result {
                Ok(()) => {
                    debug!(
                        correlation_id = %correlation_id,
                        hook_name = hook_name,
                        hook_point = %point,
                        phase = "pre",
                        duration_ms = duration.as_millis(),
                        "Pre-handler executed successfully"
                    );
                }
                Err(e) => {
                    error!(
                        correlation_id = %correlation_id,
                        hook_name = hook_name,
                        hook_point = %point,
                        phase = "pre",
                        duration_ms = duration.as_millis(),
                        error = %e,
                        "Pre-handler execution failed"
                    );

                    if first_error.is_none() {
                        first_error = Some(result.unwrap_err());
                    }
                }
            }
        }

        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Executes a single pre-handler with timeout and panic isolation.
    async fn execute_pre_handler_with_isolation(
        &self,
        hook: &dyn Hook,
        ctx: &HookContext,
    ) -> Result<(), HookError> {
        let timeout = self.timeout_for(hook);

        let result = tokio::time::timeout(
            timeout,
            AssertUnwindSafe(hook.pre_handler(ctx)).catch_unwind(),
        )
        .await;

        match result {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(hook_error))) => Err(hook_error),
            Ok(Err(panic_err)) => {
                let panic_msg = extract_panic_message(panic_err);
                error!(
                    hook_name = hook.name(),
                    panic_message = %panic_msg,
                    "Pre-handler panicked"
                );
                Err(HookError::Panic {
                    hook_name: hook.name().to_string(),
                    message: panic_msg,
                })
            }
            Err(_elapsed) => {
                warn!(
                    hook_name = hook.name(),
                    timeout_ms = timeout.as_millis(),
                    "Pre-handler timed out"
                );
                Err(HookError::Timeout {
                    hook_name: hook.name().to_string(),
                    timeout,
                })
            }
        }
    }

    /// The pre-handler and response-handler timeout for `hook`.
    fn timeout_for(&self, hook: &dyn Hook) -> Duration {
        hook.pre_handler_timeout()
            .unwrap_or(self.pre_handler_timeout)
    }

    /// Dispatches response handlers synchronously for all enabled hooks.
    ///
    /// # Execution Model
    ///
    /// 1. Executes all response handlers in registration order
    /// 2. Each handler is wrapped with timeout and panic isolation
    /// 3. Errors are logged but NOT propagated (non-fatal)
    /// 4. Messages from multiple hooks are joined with newlines
    ///
    /// # Returns
    ///
    /// A merged [`HookResponse`] containing combined messages from all hooks.
    /// Always succeeds — individual hook errors are logged but not propagated.
    pub fn dispatch_response(&self, point: HookPoint, ctx: &HookContext) -> HookResponse {
        let hooks = match self.hooks_by_point.get(&point) {
            Some(hooks) if !hooks.is_empty() => hooks,
            _ => return HookResponse::empty(),
        };

        let correlation_id = ctx.correlation_id().to_string();
        let mut messages: Vec<String> = Vec::new();

        for hook in hooks {
            let hook_name = hook.name();
            let start = Instant::now();

            let result = self.execute_response_handler_with_isolation(hook.clone(), ctx);

            let duration = start.elapsed();

            match result {
                Ok(response) => {
                    debug!(
                        correlation_id = %correlation_id,
                        hook_name = hook_name,
                        hook_point = %point,
                        phase = "response",
                        duration_ms = duration.as_millis(),
                        has_message = response.message.is_some(),
                        "Response handler executed successfully"
                    );
                    if let Some(msg) = response.message {
                        messages.push(msg);
                    }
                }
                Err(e) => {
                    warn!(
                        correlation_id = %correlation_id,
                        hook_name = hook_name,
                        hook_point = %point,
                        phase = "response",
                        duration_ms = duration.as_millis(),
                        error = %e,
                        "Response handler failed (non-fatal)"
                    );
                }
            }
        }

        HookResponse {
            message: if messages.is_empty() {
                None
            } else {
                Some(messages.join("\n"))
            },
        }
    }

    /// Executes a single response handler with timeout and panic isolation.
    fn execute_response_handler_with_isolation(
        &self,
        hook: Arc<dyn Hook>,
        ctx: &HookContext,
    ) -> Result<HookResponse, HookError> {
        let hook_name = hook.name().to_string();
        let timeout = self.timeout_for(hook.as_ref());

        let start = Instant::now();

        let result = std::panic::catch_unwind(AssertUnwindSafe(|| hook.response_handler(ctx)));

        let elapsed = start.elapsed();

        if elapsed > timeout {
            warn!(
                hook_name = %hook_name,
                timeout_ms = timeout.as_millis(),
                actual_ms = elapsed.as_millis(),
                "Response handler exceeded timeout"
            );
            return Err(HookError::Timeout { hook_name, timeout });
        }

        match result {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(hook_error)) => Err(hook_error),
            Err(panic_err) => {
                let panic_msg = extract_panic_message(panic_err);
                error!(
                    hook_name = %hook_name,
                    panic_message = %panic_msg,
                    "Response handler panicked"
                );
                Err(HookError::Panic {
                    hook_name,
                    message: panic_msg,
                })
            }
        }
    }

    /// Spawns post-handlers asynchronously in separate tokio tasks.
    ///
    /// # Execution Model
    ///
    /// 1. Spawns independent tokio tasks for each hook's post-handler
    /// 2. Returns immediately without waiting for tasks to complete
    /// 3. Each task has its own timeout
    /// 4. Errors are logged but not propagated
    ///
    /// # Non-Blocking Behavior
    ///
    /// This method returns immediately. Post-handlers run in background.
    /// Use after the operation completes successfully.
    ///
    /// # Logging
    ///
    /// Each post-handler execution is logged with:
    /// - `correlation_id`
    /// - `hook_name`
    /// - `hook_point`
    /// - `phase` = "post"
    /// - `duration_ms`
    /// - `result` (ok/error)
    pub fn spawn_post(&self, point: HookPoint, ctx: HookContext) {
        let hooks = match self.hooks_by_point.get(&point) {
            Some(hooks) if !hooks.is_empty() => hooks.clone(),
            _ => return,
        };

        let correlation_id = ctx.correlation_id().to_string();
        let timeout = self.post_handler_timeout;

        for hook in hooks {
            let hook_name = hook.name().to_string();
            let ctx_clone = ctx.clone();
            let correlation_id = correlation_id.clone();

            lore_spawn!(async move {
                let start = Instant::now();

                let result =
                    execute_post_handler_with_isolation(hook.clone(), &ctx_clone, timeout).await;

                let duration = start.elapsed();

                match result {
                    Ok(()) => {
                        debug!(
                            correlation_id = %correlation_id,
                            hook_name = %hook_name,
                            hook_point = %point,
                            phase = "post",
                            duration_ms = duration.as_millis(),
                            "Post-handler executed successfully"
                        );
                    }
                    Err(e) => {
                        error!(
                            correlation_id = %correlation_id,
                            hook_name = %hook_name,
                            hook_point = %point,
                            phase = "post",
                            duration_ms = duration.as_millis(),
                            error = %e,
                            "Post-handler execution failed (non-blocking)"
                        );
                    }
                }
            });
        }
    }
}

/// Executes a post-handler with timeout and panic isolation.
async fn execute_post_handler_with_isolation(
    hook: Arc<dyn Hook>,
    ctx: &HookContext,
    timeout: Duration,
) -> Result<(), HookError> {
    let hook_name = hook.name().to_string();

    let result = tokio::time::timeout(timeout, async {
        let hook_clone = hook.clone();
        let ctx_clone = ctx.clone();

        AssertUnwindSafe(async move { hook_clone.post_handler(&ctx_clone).await })
            .catch_unwind()
            .await
    })
    .await;

    match result {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(hook_error))) => Err(hook_error),
        Ok(Err(panic_err)) => {
            let panic_msg = extract_panic_message(panic_err);
            error!(
                hook_name = %hook_name,
                panic_message = %panic_msg,
                "Post-handler panicked"
            );
            Err(HookError::Panic {
                hook_name,
                message: panic_msg,
            })
        }
        Err(_) => {
            warn!(
                hook_name = %hook_name,
                "Post-handler timed out"
            );
            Err(HookError::Timeout { hook_name, timeout })
        }
    }
}

/// Extracts a message from a panic payload.
fn extract_panic_message(panic_err: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic_err.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = panic_err.downcast_ref::<String>() {
        s.clone()
    } else {
        "Unknown panic".to_string()
    }
}

impl Default for HookDispatcher {
    fn default() -> Self {
        Self::empty()
    }
}

impl std::fmt::Debug for HookDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hooks_info: HashMap<_, _> = self
            .hooks_by_point
            .iter()
            .map(|(point, hooks)| {
                let names: Vec<_> = hooks.iter().map(|h| h.name()).collect();
                (*point, names)
            })
            .collect();

        f.debug_struct("HookDispatcher")
            .field("hooks_by_point", &hooks_info)
            .field("pre_handler_timeout", &self.pre_handler_timeout)
            .field("post_handler_timeout", &self.post_handler_timeout)
            .finish()
    }
}
