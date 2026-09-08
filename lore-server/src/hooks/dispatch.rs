// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Hook dispatcher for executing hooks at specific hook points.
//!
//! The [`HookDispatcher`] is responsible for:
//!
//! - Maintaining a mapping from [`HookPoint`] to registered hooks
//! - Executing pre-handlers synchronously with timeout protection
//! - Spawning post-handlers asynchronously in separate tasks
//! - Isolating hook errors and panics to prevent cascade failures
//! - Logging hook execution with correlation IDs for audit trails
//!
//! # Three-Phase Execution Model
//!
//! The dispatcher provides three methods for hook execution:
//!
//! 1. **`dispatch_pre()`** - Synchronous pre-handler execution
//!    - All hooks execute in registration order
//!    - Each hook has a timeout (default 200ms)
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
//! // Dispatch pre-handlers (synchronous, can veto):
//! // dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx)?;
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
pub const DEFAULT_PRE_HANDLER_TIMEOUT: Duration = Duration::from_millis(200);

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
    /// - 200ms for pre-handler timeout
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

    /// Dispatches pre-handlers synchronously for all enabled hooks.
    ///
    /// # Execution Model
    ///
    /// 1. Executes all pre-handlers in registration order
    /// 2. Each pre-handler is wrapped with timeout and panic isolation
    /// 3. All pre-handlers execute even if earlier ones fail
    /// 4. Returns the first error encountered (enables veto)
    ///
    /// # Returns
    ///
    /// - `Ok(())` if all pre-handlers succeed
    /// - `Err(HookError)` if any pre-handler fails (first error)
    ///
    /// # Blocking Behavior
    ///
    /// This method blocks until all pre-handlers complete or timeout.
    /// Use before the operation to enable rejection/veto capability.
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
    pub fn dispatch_pre(&self, point: HookPoint, ctx: &HookContext) -> Result<(), HookError> {
        let hooks = match self.hooks_by_point.get(&point) {
            Some(hooks) if !hooks.is_empty() => hooks,
            _ => return Ok(()),
        };

        let correlation_id = ctx.correlation_id().to_string();
        let mut first_error: Option<HookError> = None;

        for hook in hooks {
            let hook_name = hook.name();
            let start = Instant::now();

            let result = self.execute_pre_handler_with_isolation(hook.clone(), ctx);

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
    fn execute_pre_handler_with_isolation(
        &self,
        hook: Arc<dyn Hook>,
        ctx: &HookContext,
    ) -> Result<(), HookError> {
        let hook_name = hook.name().to_string();
        let timeout = self.pre_handler_timeout;

        let start = Instant::now();

        let result = std::panic::catch_unwind(AssertUnwindSafe(|| hook.pre_handler(ctx)));

        let elapsed = start.elapsed();

        if elapsed > timeout {
            warn!(
                hook_name = %hook_name,
                timeout_ms = timeout.as_millis(),
                actual_ms = elapsed.as_millis(),
                "Pre-handler exceeded timeout"
            );
            return Err(HookError::Timeout { hook_name, timeout });
        }

        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(hook_error)) => Err(hook_error),
            Err(panic_err) => {
                let panic_msg = extract_panic_message(panic_err);
                error!(
                    hook_name = %hook_name,
                    panic_message = %panic_msg,
                    "Pre-handler panicked"
                );
                Err(HookError::Panic {
                    hook_name,
                    message: panic_msg,
                })
            }
        }
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
        let timeout = self.pre_handler_timeout;

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

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use async_trait::async_trait;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_revision::lore::RepositoryId;

    use super::*;

    fn test_execution_context() -> std::sync::Arc<lore_revision::interface::ExecutionContext> {
        crate::util::setup_execution("test", "test".to_string(), "test-user".to_string())
    }

    struct SuccessHook {
        name: &'static str,
        points: &'static [HookPoint],
        pre_count: Arc<AtomicUsize>,
        post_count: Arc<AtomicUsize>,
    }

    impl SuccessHook {
        fn new(name: &'static str, points: &'static [HookPoint]) -> Self {
            Self {
                name,
                points,
                pre_count: Arc::new(AtomicUsize::new(0)),
                post_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn with_counters(
            mut self,
            pre_count: Arc<AtomicUsize>,
            post_count: Arc<AtomicUsize>,
        ) -> Self {
            self.pre_count = pre_count;
            self.post_count = post_count;
            self
        }
    }

    #[async_trait]
    impl Hook for SuccessHook {
        fn name(&self) -> &'static str {
            self.name
        }

        fn hook_points(&self) -> &'static [HookPoint] {
            self.points
        }

        fn pre_handler(&self, _ctx: &HookContext) -> Result<(), HookError> {
            self.pre_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn post_handler(&self, _ctx: &HookContext) -> Result<(), HookError> {
            self.post_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct RejectingHook {
        name: &'static str,
        points: &'static [HookPoint],
        pre_count: Arc<AtomicUsize>,
    }

    impl RejectingHook {
        fn new(name: &'static str, points: &'static [HookPoint]) -> Self {
            Self {
                name,
                points,
                pre_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn with_counter(mut self, counter: Arc<AtomicUsize>) -> Self {
            self.pre_count = counter;
            self
        }
    }

    #[async_trait]
    impl Hook for RejectingHook {
        fn name(&self) -> &'static str {
            self.name
        }

        fn hook_points(&self) -> &'static [HookPoint] {
            self.points
        }

        fn pre_handler(&self, _ctx: &HookContext) -> Result<(), HookError> {
            self.pre_count.fetch_add(1, Ordering::SeqCst);
            Err(HookError::rejected(
                self.name,
                "Operation rejected",
                crate::hooks::traits::StatusCode::PermissionDenied,
            ))
        }
    }

    struct FailingHook {
        name: &'static str,
        points: &'static [HookPoint],
        execute_count: Arc<AtomicUsize>,
    }

    impl FailingHook {
        fn new(name: &'static str, points: &'static [HookPoint]) -> Self {
            Self {
                name,
                points,
                execute_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn with_counter(mut self, counter: Arc<AtomicUsize>) -> Self {
            self.execute_count = counter;
            self
        }
    }

    #[async_trait]
    impl Hook for FailingHook {
        fn name(&self) -> &'static str {
            self.name
        }

        fn hook_points(&self) -> &'static [HookPoint] {
            self.points
        }

        fn pre_handler(&self, _ctx: &HookContext) -> Result<(), HookError> {
            self.execute_count.fetch_add(1, Ordering::SeqCst);
            Err(HookError::execution_failed(self.name, "Hook failed"))
        }
    }

    struct SlowPreHook {
        name: &'static str,
        points: &'static [HookPoint],
        delay: Duration,
    }

    impl SlowPreHook {
        fn new(name: &'static str, points: &'static [HookPoint], delay: Duration) -> Self {
            Self {
                name,
                points,
                delay,
            }
        }
    }

    #[async_trait]
    impl Hook for SlowPreHook {
        fn name(&self) -> &'static str {
            self.name
        }

        fn hook_points(&self) -> &'static [HookPoint] {
            self.points
        }

        fn pre_handler(&self, _ctx: &HookContext) -> Result<(), HookError> {
            std::thread::sleep(self.delay);
            Ok(())
        }
    }

    struct SlowPostHook {
        name: &'static str,
        points: &'static [HookPoint],
        delay: Duration,
        post_count: Arc<AtomicUsize>,
    }

    impl SlowPostHook {
        fn new(name: &'static str, points: &'static [HookPoint], delay: Duration) -> Self {
            Self {
                name,
                points,
                delay,
                post_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn with_counter(mut self, counter: Arc<AtomicUsize>) -> Self {
            self.post_count = counter;
            self
        }
    }

    #[async_trait]
    impl Hook for SlowPostHook {
        fn name(&self) -> &'static str {
            self.name
        }

        fn hook_points(&self) -> &'static [HookPoint] {
            self.points
        }

        async fn post_handler(&self, _ctx: &HookContext) -> Result<(), HookError> {
            tokio::time::sleep(self.delay).await;
            self.post_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct PanicPreHook {
        name: &'static str,
        points: &'static [HookPoint],
    }

    impl PanicPreHook {
        fn new(name: &'static str, points: &'static [HookPoint]) -> Self {
            Self { name, points }
        }
    }

    #[async_trait]
    impl Hook for PanicPreHook {
        fn name(&self) -> &'static str {
            self.name
        }

        fn hook_points(&self) -> &'static [HookPoint] {
            self.points
        }

        fn pre_handler(&self, _ctx: &HookContext) -> Result<(), HookError> {
            panic!("Pre-handler panicked!");
        }
    }

    struct PanicPostHook {
        name: &'static str,
        points: &'static [HookPoint],
    }

    impl PanicPostHook {
        fn new(name: &'static str, points: &'static [HookPoint]) -> Self {
            Self { name, points }
        }
    }

    #[async_trait]
    impl Hook for PanicPostHook {
        fn name(&self) -> &'static str {
            self.name
        }

        fn hook_points(&self) -> &'static [HookPoint] {
            self.points
        }

        async fn post_handler(&self, _ctx: &HookContext) -> Result<(), HookError> {
            panic!("Post-handler panicked!");
        }
    }

    /// Hook that reads context values (demonstrates read-only access)
    struct ContextReadingHook {
        name: &'static str,
        points: &'static [HookPoint],
        observed_user: Arc<parking_lot::Mutex<Option<String>>>,
    }

    impl ContextReadingHook {
        fn new(name: &'static str, points: &'static [HookPoint]) -> Self {
            Self {
                name,
                points,
                observed_user: Arc::new(parking_lot::Mutex::new(None)),
            }
        }
    }

    #[async_trait]
    impl Hook for ContextReadingHook {
        fn name(&self) -> &'static str {
            self.name
        }

        fn hook_points(&self) -> &'static [HookPoint] {
            self.points
        }

        fn pre_handler(&self, ctx: &HookContext) -> Result<(), HookError> {
            // Read-only access to context
            *self.observed_user.lock() = ctx.user().map(|s| s.to_string());
            Ok(())
        }
    }

    fn create_test_context() -> HookContext {
        HookContext::builder()
            .correlation_id("test-correlation-123")
            .hook_point(HookPoint::BranchPush)
            .repository(RepositoryId::default())
            .build()
    }

    #[test]
    fn test_empty_dispatcher() {
        let dispatcher = HookDispatcher::empty();

        assert_eq!(dispatcher.hook_count(HookPoint::BranchPush), 0);
        assert!(!dispatcher.has_hooks(HookPoint::BranchPush));
        assert_eq!(dispatcher.total_hook_registrations(), 0);
        assert_eq!(
            dispatcher.pre_handler_timeout(),
            DEFAULT_PRE_HANDLER_TIMEOUT
        );
        assert_eq!(
            dispatcher.post_handler_timeout(),
            DEFAULT_POST_HANDLER_TIMEOUT
        );
    }

    /// A dispatcher whose handler timeouts are far longer than any handler here
    /// needs.
    ///
    /// `dispatch_pre` measures a handler after it returns and reports
    /// `HookError::Timeout` if it took longer than the limit, discarding what
    /// the handler actually produced. With the 200 ms default that turns a
    /// loaded machine into a test failure: the handler's real result -- a
    /// panic, a rejection, a success -- is replaced by a timeout that says
    /// nothing about the behaviour under test. Tests that are about the timeout
    /// set a short one deliberately.
    fn test_dispatcher(hooks: Vec<(String, Box<dyn Hook>)>) -> HookDispatcher {
        HookDispatcher::new(hooks, Duration::from_secs(60), Duration::from_secs(60))
    }

    #[test]
    fn test_dispatcher_from_hooks_default() {
        let hooks: Vec<(String, Box<dyn Hook>)> = vec![(
            "hook1".to_string(),
            Box::new(SuccessHook::new("hook1", &[HookPoint::BranchPush])),
        )];

        let dispatcher = HookDispatcher::from_hooks_default(hooks);

        assert_eq!(
            dispatcher.pre_handler_timeout(),
            DEFAULT_PRE_HANDLER_TIMEOUT
        );
        assert_eq!(
            dispatcher.post_handler_timeout(),
            DEFAULT_POST_HANDLER_TIMEOUT
        );
    }

    #[test]
    fn test_dispatch_pre_no_hooks() {
        let dispatcher = HookDispatcher::empty();
        let ctx = create_test_context();

        let result = LORE_CONTEXT.sync_scope(test_execution_context(), || {
            dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx)
        });
        assert!(result.is_ok());
    }

    #[test]
    fn test_dispatch_pre_success() {
        let pre_count = Arc::new(AtomicUsize::new(0));
        let post_count = Arc::new(AtomicUsize::new(0));

        let hooks: Vec<(String, Box<dyn Hook>)> = vec![(
            "test".to_string(),
            Box::new(
                SuccessHook::new("test", &[HookPoint::BranchPush])
                    .with_counters(pre_count.clone(), post_count.clone()),
            ),
        )];

        let dispatcher = test_dispatcher(hooks);
        let ctx = create_test_context();

        let result = LORE_CONTEXT.sync_scope(test_execution_context(), || {
            dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx)
        });
        assert!(result.is_ok());
        assert_eq!(pre_count.load(Ordering::SeqCst), 1);
        assert_eq!(post_count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_dispatch_pre_multiple_hooks() {
        let pre_count1 = Arc::new(AtomicUsize::new(0));
        let pre_count2 = Arc::new(AtomicUsize::new(0));

        let hooks: Vec<(String, Box<dyn Hook>)> = vec![
            (
                "hook1".to_string(),
                Box::new(
                    SuccessHook::new("hook1", &[HookPoint::BranchPush])
                        .with_counters(pre_count1.clone(), Arc::new(AtomicUsize::new(0))),
                ),
            ),
            (
                "hook2".to_string(),
                Box::new(
                    SuccessHook::new("hook2", &[HookPoint::BranchPush])
                        .with_counters(pre_count2.clone(), Arc::new(AtomicUsize::new(0))),
                ),
            ),
        ];

        let dispatcher = test_dispatcher(hooks);
        let ctx = create_test_context();

        let result = LORE_CONTEXT.sync_scope(test_execution_context(), || {
            dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx)
        });
        assert!(result.is_ok());
        assert_eq!(pre_count1.load(Ordering::SeqCst), 1);
        assert_eq!(pre_count2.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_dispatch_pre_wrong_hook_point() {
        let pre_count = Arc::new(AtomicUsize::new(0));

        let hooks: Vec<(String, Box<dyn Hook>)> = vec![(
            "test".to_string(),
            Box::new(
                SuccessHook::new("test", &[HookPoint::BranchPush])
                    .with_counters(pre_count.clone(), Arc::new(AtomicUsize::new(0))),
            ),
        )];

        let dispatcher = test_dispatcher(hooks);
        let ctx = create_test_context();

        let result = LORE_CONTEXT.sync_scope(test_execution_context(), || {
            dispatcher.dispatch_pre(HookPoint::BranchDelete, &ctx)
        });
        assert!(result.is_ok());
        assert_eq!(pre_count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_dispatch_pre_rejection() {
        let hooks: Vec<(String, Box<dyn Hook>)> = vec![(
            "rejecting".to_string(),
            Box::new(RejectingHook::new("rejecting", &[HookPoint::BranchPush])),
        )];

        let dispatcher = test_dispatcher(hooks);
        let ctx = create_test_context();

        let result = LORE_CONTEXT.sync_scope(test_execution_context(), || {
            dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx)
        });
        assert!(result.is_err());

        match result.unwrap_err() {
            HookError::Rejected {
                hook_name, status, ..
            } => {
                assert_eq!(hook_name, "rejecting");
                assert_eq!(status, crate::hooks::traits::StatusCode::PermissionDenied);
            }
            _ => panic!("Expected Rejected error"),
        }
    }

    #[test]
    fn test_dispatch_pre_error_isolation() {
        let counter1 = Arc::new(AtomicUsize::new(0));
        let counter2 = Arc::new(AtomicUsize::new(0));

        let hooks: Vec<(String, Box<dyn Hook>)> = vec![
            (
                "failing".to_string(),
                Box::new(
                    FailingHook::new("failing", &[HookPoint::BranchPush])
                        .with_counter(counter1.clone()),
                ),
            ),
            (
                "success".to_string(),
                Box::new(
                    SuccessHook::new("success", &[HookPoint::BranchPush])
                        .with_counters(counter2.clone(), Arc::new(AtomicUsize::new(0))),
                ),
            ),
        ];

        let dispatcher = test_dispatcher(hooks);
        let ctx = create_test_context();

        let result = LORE_CONTEXT.sync_scope(test_execution_context(), || {
            dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx)
        });

        assert!(result.is_err());

        assert_eq!(counter1.load(Ordering::SeqCst), 1);
        assert_eq!(counter2.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_dispatch_pre_returns_first_error() {
        let counter1 = Arc::new(AtomicUsize::new(0));
        let counter2 = Arc::new(AtomicUsize::new(0));

        let hooks: Vec<(String, Box<dyn Hook>)> = vec![
            (
                "fail1".to_string(),
                Box::new(
                    RejectingHook::new("fail1", &[HookPoint::BranchPush])
                        .with_counter(counter1.clone()),
                ),
            ),
            (
                "fail2".to_string(),
                Box::new(
                    RejectingHook::new("fail2", &[HookPoint::BranchPush])
                        .with_counter(counter2.clone()),
                ),
            ),
        ];

        let dispatcher = test_dispatcher(hooks);
        let ctx = create_test_context();

        let result = LORE_CONTEXT.sync_scope(test_execution_context(), || {
            dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx)
        });

        match result.unwrap_err() {
            HookError::Rejected { hook_name, .. } => {
                assert_eq!(hook_name, "fail1");
            }
            _ => panic!("Expected Rejected error"),
        }

        assert_eq!(counter1.load(Ordering::SeqCst), 1);
        assert_eq!(counter2.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_dispatch_pre_timeout() {
        let hooks: Vec<(String, Box<dyn Hook>)> = vec![(
            "slow".to_string(),
            Box::new(SlowPreHook::new(
                "slow",
                &[HookPoint::BranchPush],
                Duration::from_millis(500),
            )),
        )];

        let dispatcher = HookDispatcher::new(
            hooks,
            Duration::from_millis(50),
            DEFAULT_POST_HANDLER_TIMEOUT,
        );
        let ctx = create_test_context();

        let result = LORE_CONTEXT.sync_scope(test_execution_context(), || {
            dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx)
        });

        match result.unwrap_err() {
            HookError::Timeout { hook_name, .. } => {
                assert_eq!(hook_name, "slow");
            }
            _ => panic!("Expected Timeout error"),
        }
    }

    #[test]
    fn test_dispatch_pre_panic_isolation() {
        // Temporarily set a silent panic hook to suppress the custom panic output
        // from urc-core's execution_initialize(). The panic is still caught by
        // catch_unwind, but we don't want it printing to stderr during tests.
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {
            // Intentionally silent - this test verifies panic isolation
        }));

        let hooks: Vec<(String, Box<dyn Hook>)> = vec![(
            "panic".to_string(),
            Box::new(PanicPreHook::new("panic", &[HookPoint::BranchPush])),
        )];

        let dispatcher = test_dispatcher(hooks);
        let ctx = create_test_context();

        let result = LORE_CONTEXT.sync_scope(test_execution_context(), || {
            dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx)
        });

        // Restore previous panic hook before assertions (so assertion failures are visible)
        std::panic::set_hook(prev_hook);

        match result.unwrap_err() {
            HookError::Panic { hook_name, message } => {
                assert_eq!(hook_name, "panic");
                assert!(message.contains("panicked"));
            }
            _ => panic!("Expected Panic error"),
        }
    }

    #[test]
    fn test_dispatch_pre_panic_does_not_affect_other_hooks() {
        // Temporarily set a silent panic hook to suppress the custom panic output
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));

        let counter = Arc::new(AtomicUsize::new(0));

        let hooks: Vec<(String, Box<dyn Hook>)> = vec![
            (
                "panic".to_string(),
                Box::new(PanicPreHook::new("panic", &[HookPoint::BranchPush])),
            ),
            (
                "success".to_string(),
                Box::new(
                    SuccessHook::new("success", &[HookPoint::BranchPush])
                        .with_counters(counter.clone(), Arc::new(AtomicUsize::new(0))),
                ),
            ),
        ];

        let dispatcher = test_dispatcher(hooks);
        let ctx = create_test_context();

        let result = LORE_CONTEXT.sync_scope(test_execution_context(), || {
            dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx)
        });

        // Restore previous panic hook before assertions
        std::panic::set_hook(prev_hook);

        assert!(result.is_err());

        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_dispatch_pre_context_read_only() {
        // Test that hooks can read context values
        let reader = ContextReadingHook::new("reader", &[HookPoint::BranchPush]);
        let observed = reader.observed_user.clone();

        let hooks: Vec<(String, Box<dyn Hook>)> = vec![("reader".to_string(), Box::new(reader))];

        let dispatcher = test_dispatcher(hooks);
        let ctx = HookContext::builder()
            .correlation_id("test")
            .hook_point(HookPoint::BranchPush)
            .repository(RepositoryId::default())
            .user("test_user")
            .build();

        let result = LORE_CONTEXT.sync_scope(test_execution_context(), || {
            dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx)
        });
        assert!(result.is_ok());

        // Hook should have read the user value
        assert_eq!(*observed.lock(), Some("test_user".to_string()));
    }

    #[tokio::test]
    async fn test_spawn_post_no_hooks() {
        LORE_CONTEXT
            .scope(test_execution_context(), async {
                let dispatcher = HookDispatcher::empty();
                let ctx = create_test_context();

                dispatcher.spawn_post(HookPoint::BranchPush, ctx);
            })
            .await;
    }

    #[tokio::test]
    async fn test_spawn_post_executes_hooks() {
        LORE_CONTEXT
            .scope(test_execution_context(), async {
                let post_count = Arc::new(AtomicUsize::new(0));

                let hooks: Vec<(String, Box<dyn Hook>)> = vec![(
                    "test".to_string(),
                    Box::new(
                        SuccessHook::new("test", &[HookPoint::BranchPush])
                            .with_counters(Arc::new(AtomicUsize::new(0)), post_count.clone()),
                    ),
                )];

                let dispatcher = test_dispatcher(hooks);
                let ctx = create_test_context();

                dispatcher.spawn_post(HookPoint::BranchPush, ctx);

                tokio::time::sleep(Duration::from_millis(50)).await;

                assert_eq!(post_count.load(Ordering::SeqCst), 1);
            })
            .await;
    }

    #[tokio::test]
    async fn test_spawn_post_returns_immediately() {
        LORE_CONTEXT
            .scope(test_execution_context(), async {
                let post_count = Arc::new(AtomicUsize::new(0));

                let hooks: Vec<(String, Box<dyn Hook>)> = vec![(
                    "slow".to_string(),
                    Box::new(
                        SlowPostHook::new(
                            "slow",
                            &[HookPoint::BranchPush],
                            Duration::from_millis(200),
                        )
                        .with_counter(post_count.clone()),
                    ),
                )];

                let dispatcher = test_dispatcher(hooks);
                let ctx = create_test_context();

                let start = Instant::now();
                dispatcher.spawn_post(HookPoint::BranchPush, ctx);
                let elapsed = start.elapsed();

                assert!(elapsed < Duration::from_millis(50));

                assert_eq!(post_count.load(Ordering::SeqCst), 0);

                tokio::time::sleep(Duration::from_millis(300)).await;
                assert_eq!(post_count.load(Ordering::SeqCst), 1);
            })
            .await;
    }

    #[tokio::test]
    async fn test_spawn_post_multiple_hooks() {
        LORE_CONTEXT
            .scope(test_execution_context(), async {
                let post_count1 = Arc::new(AtomicUsize::new(0));
                let post_count2 = Arc::new(AtomicUsize::new(0));

                let hooks: Vec<(String, Box<dyn Hook>)> = vec![
                    (
                        "hook1".to_string(),
                        Box::new(
                            SuccessHook::new("hook1", &[HookPoint::BranchPush])
                                .with_counters(Arc::new(AtomicUsize::new(0)), post_count1.clone()),
                        ),
                    ),
                    (
                        "hook2".to_string(),
                        Box::new(
                            SuccessHook::new("hook2", &[HookPoint::BranchPush])
                                .with_counters(Arc::new(AtomicUsize::new(0)), post_count2.clone()),
                        ),
                    ),
                ];

                let dispatcher = test_dispatcher(hooks);
                let ctx = create_test_context();

                dispatcher.spawn_post(HookPoint::BranchPush, ctx);

                tokio::time::sleep(Duration::from_millis(50)).await;

                assert_eq!(post_count1.load(Ordering::SeqCst), 1);
                assert_eq!(post_count2.load(Ordering::SeqCst), 1);
            })
            .await;
    }

    #[tokio::test]
    async fn test_spawn_post_wrong_hook_point() {
        LORE_CONTEXT
            .scope(test_execution_context(), async {
                let post_count = Arc::new(AtomicUsize::new(0));

                let hooks: Vec<(String, Box<dyn Hook>)> = vec![(
                    "test".to_string(),
                    Box::new(
                        SuccessHook::new("test", &[HookPoint::BranchPush])
                            .with_counters(Arc::new(AtomicUsize::new(0)), post_count.clone()),
                    ),
                )];

                let dispatcher = test_dispatcher(hooks);
                let ctx = create_test_context();

                dispatcher.spawn_post(HookPoint::BranchDelete, ctx);

                tokio::time::sleep(Duration::from_millis(50)).await;

                assert_eq!(post_count.load(Ordering::SeqCst), 0);
            })
            .await;
    }

    #[tokio::test]
    async fn test_spawn_post_panic_isolation() {
        // Temporarily set a silent panic hook to suppress the custom panic output
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));

        LORE_CONTEXT
            .scope(test_execution_context(), async {
                let post_count = Arc::new(AtomicUsize::new(0));

                let hooks: Vec<(String, Box<dyn Hook>)> = vec![
                    (
                        "panic".to_string(),
                        Box::new(PanicPostHook::new("panic", &[HookPoint::BranchPush])),
                    ),
                    (
                        "success".to_string(),
                        Box::new(
                            SuccessHook::new("success", &[HookPoint::BranchPush])
                                .with_counters(Arc::new(AtomicUsize::new(0)), post_count.clone()),
                        ),
                    ),
                ];

                let dispatcher = test_dispatcher(hooks);
                let ctx = create_test_context();

                dispatcher.spawn_post(HookPoint::BranchPush, ctx);

                tokio::time::sleep(Duration::from_millis(50)).await;

                // Restore previous panic hook before assertions
                std::panic::set_hook(prev_hook);

                assert_eq!(post_count.load(Ordering::SeqCst), 1);
            })
            .await;
    }

    #[tokio::test]
    async fn test_spawn_post_timeout_isolation() {
        LORE_CONTEXT
            .scope(test_execution_context(), async {
                let slow_count = Arc::new(AtomicUsize::new(0));
                let fast_count = Arc::new(AtomicUsize::new(0));

                let hooks: Vec<(String, Box<dyn Hook>)> = vec![
                    (
                        "slow".to_string(),
                        Box::new(
                            SlowPostHook::new(
                                "slow",
                                &[HookPoint::BranchPush],
                                Duration::from_secs(10),
                            )
                            .with_counter(slow_count.clone()),
                        ),
                    ),
                    (
                        "fast".to_string(),
                        Box::new(
                            SuccessHook::new("fast", &[HookPoint::BranchPush])
                                .with_counters(Arc::new(AtomicUsize::new(0)), fast_count.clone()),
                        ),
                    ),
                ];

                let dispatcher = HookDispatcher::new(
                    hooks,
                    DEFAULT_PRE_HANDLER_TIMEOUT,
                    Duration::from_millis(50),
                );
                let ctx = create_test_context();

                dispatcher.spawn_post(HookPoint::BranchPush, ctx);

                tokio::time::sleep(Duration::from_millis(200)).await;

                assert_eq!(fast_count.load(Ordering::SeqCst), 1);

                assert_eq!(slow_count.load(Ordering::SeqCst), 0);
            })
            .await;
    }

    #[test]
    fn test_dispatch_pre_performance_many_hooks() {
        let mut hooks: Vec<(String, Box<dyn Hook>)> = Vec::new();

        for i in 0..100 {
            hooks.push((
                format!("hook_{i}"),
                Box::new(SuccessHook::new(
                    Box::leak(format!("hook_{i}").into_boxed_str()),
                    &[HookPoint::BranchPush],
                )),
            ));
        }

        let dispatcher = test_dispatcher(hooks);
        let ctx = create_test_context();

        let start = std::time::Instant::now();
        let result = LORE_CONTEXT.sync_scope(test_execution_context(), || {
            dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx)
        });
        let elapsed = start.elapsed();

        assert!(result.is_ok());
        assert!(elapsed < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn test_spawn_post_performance_many_hooks() {
        LORE_CONTEXT
            .scope(test_execution_context(), async {
                let mut hooks: Vec<(String, Box<dyn Hook>)> = Vec::new();
                let counters: Vec<Arc<AtomicUsize>> =
                    (0..100).map(|_| Arc::new(AtomicUsize::new(0))).collect();

                for (i, counter) in counters.iter().enumerate() {
                    hooks.push((
                        format!("hook_{i}"),
                        Box::new(
                            SuccessHook::new(
                                Box::leak(format!("hook_{i}").into_boxed_str()),
                                &[HookPoint::BranchPush],
                            )
                            .with_counters(Arc::new(AtomicUsize::new(0)), counter.clone()),
                        ),
                    ));
                }

                let dispatcher = test_dispatcher(hooks);
                let ctx = create_test_context();

                let start = std::time::Instant::now();
                dispatcher.spawn_post(HookPoint::BranchPush, ctx);
                let spawn_elapsed = start.elapsed();

                assert!(spawn_elapsed < Duration::from_millis(50));

                tokio::time::sleep(Duration::from_millis(100)).await;

                for counter in &counters {
                    assert_eq!(counter.load(Ordering::SeqCst), 1);
                }
            })
            .await;
    }

    // ==================== dispatch_response tests ====================

    #[test]
    fn test_dispatch_response_no_hooks() {
        let dispatcher = HookDispatcher::empty();
        let ctx = create_test_context();

        let response = dispatcher.dispatch_response(HookPoint::BranchPush, &ctx);
        assert!(response.message.is_none());
    }

    #[test]
    fn test_dispatch_response_single_hook_with_message() {
        struct MessageHook;

        #[async_trait]
        impl Hook for MessageHook {
            fn name(&self) -> &'static str {
                "message_hook"
            }
            fn hook_points(&self) -> &'static [HookPoint] {
                &[HookPoint::BranchPush]
            }
            fn response_handler(&self, _ctx: &HookContext) -> Result<HookResponse, HookError> {
                Ok(HookResponse::with_message("hello from hook"))
            }
        }

        let hooks: Vec<(String, Box<dyn Hook>)> =
            vec![("message_hook".to_string(), Box::new(MessageHook))];
        let dispatcher = test_dispatcher(hooks);
        let ctx = create_test_context();

        let response = dispatcher.dispatch_response(HookPoint::BranchPush, &ctx);
        assert_eq!(response.message, Some("hello from hook".to_string()));
    }

    #[test]
    fn test_dispatch_response_no_message() {
        struct NoMessageHook;

        #[async_trait]
        impl Hook for NoMessageHook {
            fn name(&self) -> &'static str {
                "no_msg"
            }
            fn hook_points(&self) -> &'static [HookPoint] {
                &[HookPoint::BranchPush]
            }
            fn response_handler(&self, _ctx: &HookContext) -> Result<HookResponse, HookError> {
                Ok(HookResponse::empty())
            }
        }

        let hooks: Vec<(String, Box<dyn Hook>)> =
            vec![("no_msg".to_string(), Box::new(NoMessageHook))];
        let dispatcher = test_dispatcher(hooks);
        let ctx = create_test_context();

        let response = dispatcher.dispatch_response(HookPoint::BranchPush, &ctx);
        assert!(response.message.is_none());
    }

    #[test]
    fn test_dispatch_response_multiple_hooks_merge_messages() {
        struct HookA;
        struct HookB;

        #[async_trait]
        impl Hook for HookA {
            fn name(&self) -> &'static str {
                "hook_a"
            }
            fn hook_points(&self) -> &'static [HookPoint] {
                &[HookPoint::BranchPush]
            }
            fn response_handler(&self, _ctx: &HookContext) -> Result<HookResponse, HookError> {
                Ok(HookResponse::with_message("message A"))
            }
        }

        #[async_trait]
        impl Hook for HookB {
            fn name(&self) -> &'static str {
                "hook_b"
            }
            fn hook_points(&self) -> &'static [HookPoint] {
                &[HookPoint::BranchPush]
            }
            fn response_handler(&self, _ctx: &HookContext) -> Result<HookResponse, HookError> {
                Ok(HookResponse::with_message("message B"))
            }
        }

        let hooks: Vec<(String, Box<dyn Hook>)> = vec![
            ("hook_a".to_string(), Box::new(HookA)),
            ("hook_b".to_string(), Box::new(HookB)),
        ];
        let dispatcher = test_dispatcher(hooks);
        let ctx = create_test_context();

        let response = dispatcher.dispatch_response(HookPoint::BranchPush, &ctx);
        assert_eq!(response.message, Some("message A\nmessage B".to_string()));
    }

    #[test]
    fn test_dispatch_response_error_is_non_fatal() {
        struct FailingHook;
        struct GoodHook;

        #[async_trait]
        impl Hook for FailingHook {
            fn name(&self) -> &'static str {
                "failing"
            }
            fn hook_points(&self) -> &'static [HookPoint] {
                &[HookPoint::BranchPush]
            }
            fn response_handler(&self, _ctx: &HookContext) -> Result<HookResponse, HookError> {
                Err(HookError::execution_failed("failing", "something broke"))
            }
        }

        #[async_trait]
        impl Hook for GoodHook {
            fn name(&self) -> &'static str {
                "good"
            }
            fn hook_points(&self) -> &'static [HookPoint] {
                &[HookPoint::BranchPush]
            }
            fn response_handler(&self, _ctx: &HookContext) -> Result<HookResponse, HookError> {
                Ok(HookResponse::with_message("still works"))
            }
        }

        let hooks: Vec<(String, Box<dyn Hook>)> = vec![
            ("failing".to_string(), Box::new(FailingHook)),
            ("good".to_string(), Box::new(GoodHook)),
        ];
        let dispatcher = test_dispatcher(hooks);
        let ctx = create_test_context();

        let response = dispatcher.dispatch_response(HookPoint::BranchPush, &ctx);
        assert_eq!(response.message, Some("still works".to_string()));
    }

    #[test]
    fn test_dispatch_response_panic_isolation() {
        struct PanicHook;
        struct SafeHook;

        #[async_trait]
        impl Hook for PanicHook {
            fn name(&self) -> &'static str {
                "panic"
            }
            fn hook_points(&self) -> &'static [HookPoint] {
                &[HookPoint::BranchPush]
            }
            fn response_handler(&self, _ctx: &HookContext) -> Result<HookResponse, HookError> {
                panic!("response handler panic");
            }
        }

        #[async_trait]
        impl Hook for SafeHook {
            fn name(&self) -> &'static str {
                "safe"
            }
            fn hook_points(&self) -> &'static [HookPoint] {
                &[HookPoint::BranchPush]
            }
            fn response_handler(&self, _ctx: &HookContext) -> Result<HookResponse, HookError> {
                Ok(HookResponse::with_message("safe message"))
            }
        }

        let hooks: Vec<(String, Box<dyn Hook>)> = vec![
            ("panic".to_string(), Box::new(PanicHook)),
            ("safe".to_string(), Box::new(SafeHook)),
        ];
        let dispatcher = test_dispatcher(hooks);
        let ctx = create_test_context();

        let response = dispatcher.dispatch_response(HookPoint::BranchPush, &ctx);
        assert_eq!(response.message, Some("safe message".to_string()));
    }

    #[test]
    fn test_dispatch_response_wrong_hook_point() {
        struct MessageHook;

        #[async_trait]
        impl Hook for MessageHook {
            fn name(&self) -> &'static str {
                "msg"
            }
            fn hook_points(&self) -> &'static [HookPoint] {
                &[HookPoint::BranchCreate]
            }
            fn response_handler(&self, _ctx: &HookContext) -> Result<HookResponse, HookError> {
                Ok(HookResponse::with_message("should not appear"))
            }
        }

        let hooks: Vec<(String, Box<dyn Hook>)> = vec![("msg".to_string(), Box::new(MessageHook))];
        let dispatcher = test_dispatcher(hooks);
        let ctx = create_test_context();

        let response = dispatcher.dispatch_response(HookPoint::BranchPush, &ctx);
        assert!(response.message.is_none());
    }
}
