// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use lore_base::runtime::LORE_CONTEXT;
use lore_revision::lore::RepositoryId;
use lore_server::hooks::context::HookContext;
use lore_server::hooks::dispatch::*;
use lore_server::hooks::traits::Hook;
use lore_server::hooks::traits::HookError;
use lore_server::hooks::traits::HookPoint;
use lore_server::hooks::traits::HookResponse;

fn test_execution_context() -> std::sync::Arc<lore_revision::interface::ExecutionContext> {
    lore_server::util::setup_execution("test", "test".to_string(), "test-user".to_string())
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

    fn with_counters(mut self, pre_count: Arc<AtomicUsize>, post_count: Arc<AtomicUsize>) -> Self {
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

    async fn pre_handler(&self, _ctx: &HookContext) -> Result<(), HookError> {
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

    async fn pre_handler(&self, _ctx: &HookContext) -> Result<(), HookError> {
        self.pre_count.fetch_add(1, Ordering::SeqCst);
        Err(HookError::rejected(
            self.name,
            "Operation rejected",
            lore_server::hooks::traits::StatusCode::PermissionDenied,
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

    async fn pre_handler(&self, _ctx: &HookContext) -> Result<(), HookError> {
        self.execute_count.fetch_add(1, Ordering::SeqCst);
        Err(HookError::execution_failed(self.name, "Hook failed"))
    }
}

/// Sleeps for `delay` in its pre-handler and counts the pre-handlers that ran
/// to completion.
struct SlowPreHook {
    name: &'static str,
    points: &'static [HookPoint],
    delay: Duration,
    timeout: Option<Duration>,
    completed: Arc<AtomicUsize>,
}

impl SlowPreHook {
    fn new(name: &'static str, points: &'static [HookPoint], delay: Duration) -> Self {
        Self {
            name,
            points,
            delay,
            timeout: None,
            completed: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
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

    async fn pre_handler(&self, _ctx: &HookContext) -> Result<(), HookError> {
        tokio::time::sleep(self.delay).await;
        self.completed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn pre_handler_timeout(&self) -> Option<Duration> {
        self.timeout
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

    async fn pre_handler(&self, _ctx: &HookContext) -> Result<(), HookError> {
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

    async fn pre_handler(&self, ctx: &HookContext) -> Result<(), HookError> {
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
/// needs, so a loaded machine cannot turn a handler's result into a timeout.
/// Tests that are about the timeout set a short one deliberately.
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

#[tokio::test]
async fn test_dispatch_pre_no_hooks() {
    let dispatcher = HookDispatcher::empty();
    let ctx = create_test_context();

    let result = LORE_CONTEXT
        .scope(
            test_execution_context(),
            dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx),
        )
        .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_dispatch_pre_success() {
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

    let result = LORE_CONTEXT
        .scope(
            test_execution_context(),
            dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx),
        )
        .await;
    assert!(result.is_ok());
    assert_eq!(pre_count.load(Ordering::SeqCst), 1);
    assert_eq!(post_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_dispatch_pre_multiple_hooks() {
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

    let result = LORE_CONTEXT
        .scope(
            test_execution_context(),
            dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx),
        )
        .await;
    assert!(result.is_ok());
    assert_eq!(pre_count1.load(Ordering::SeqCst), 1);
    assert_eq!(pre_count2.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_dispatch_pre_wrong_hook_point() {
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

    let result = LORE_CONTEXT
        .scope(
            test_execution_context(),
            dispatcher.dispatch_pre(HookPoint::BranchDelete, &ctx),
        )
        .await;
    assert!(result.is_ok());
    assert_eq!(pre_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_dispatch_pre_rejection() {
    let hooks: Vec<(String, Box<dyn Hook>)> = vec![(
        "rejecting".to_string(),
        Box::new(RejectingHook::new("rejecting", &[HookPoint::BranchPush])),
    )];

    let dispatcher = test_dispatcher(hooks);
    let ctx = create_test_context();

    let result = LORE_CONTEXT
        .scope(
            test_execution_context(),
            dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx),
        )
        .await;
    assert!(result.is_err());

    match result.unwrap_err() {
        HookError::Rejected {
            hook_name, status, ..
        } => {
            assert_eq!(hook_name, "rejecting");
            assert_eq!(
                status,
                lore_server::hooks::traits::StatusCode::PermissionDenied
            );
        }
        _ => panic!("Expected Rejected error"),
    }
}

#[tokio::test]
async fn test_dispatch_pre_error_isolation() {
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

    let result = LORE_CONTEXT
        .scope(
            test_execution_context(),
            dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx),
        )
        .await;

    assert!(result.is_err());

    assert_eq!(counter1.load(Ordering::SeqCst), 1);
    assert_eq!(counter2.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_dispatch_pre_returns_first_error() {
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

    let result = LORE_CONTEXT
        .scope(
            test_execution_context(),
            dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx),
        )
        .await;

    match result.unwrap_err() {
        HookError::Rejected { hook_name, .. } => {
            assert_eq!(hook_name, "fail1");
        }
        _ => panic!("Expected Rejected error"),
    }

    assert_eq!(counter1.load(Ordering::SeqCst), 1);
    assert_eq!(counter2.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_dispatch_pre_timeout_cancels_handler() {
    let delay = Duration::from_millis(500);
    let hook = SlowPreHook::new("slow", &[HookPoint::BranchPush], delay);
    let completed = hook.completed.clone();
    let hooks: Vec<(String, Box<dyn Hook>)> = vec![("slow".to_string(), Box::new(hook))];

    let dispatcher = HookDispatcher::new(
        hooks,
        Duration::from_millis(50),
        DEFAULT_POST_HANDLER_TIMEOUT,
    );
    let ctx = create_test_context();

    let start = Instant::now();
    let result = LORE_CONTEXT
        .scope(
            test_execution_context(),
            dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx),
        )
        .await;
    let elapsed = start.elapsed();

    match result.unwrap_err() {
        HookError::Timeout { hook_name, .. } => {
            assert_eq!(hook_name, "slow");
        }
        _ => panic!("Expected Timeout error"),
    }
    assert!(
        elapsed < delay,
        "dispatch waited {elapsed:?} for a {delay:?} handler"
    );

    // Past the handler's own delay: a handler left running would have
    // completed by now.
    tokio::time::sleep(delay + Duration::from_millis(100)).await;
    assert_eq!(completed.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_dispatch_pre_hook_timeout_extends_default() {
    let hook = SlowPreHook::new("slow", &[HookPoint::BranchPush], Duration::from_millis(100))
        .with_timeout(Duration::from_secs(60));
    let completed = hook.completed.clone();
    let hooks: Vec<(String, Box<dyn Hook>)> = vec![("slow".to_string(), Box::new(hook))];

    let dispatcher = HookDispatcher::new(
        hooks,
        Duration::from_millis(10),
        DEFAULT_POST_HANDLER_TIMEOUT,
    );
    let ctx = create_test_context();

    let result = LORE_CONTEXT
        .scope(
            test_execution_context(),
            dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx),
        )
        .await;

    assert!(result.is_ok());
    assert_eq!(completed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_dispatch_pre_hook_timeout_shortens_default() {
    let hook = SlowPreHook::new("slow", &[HookPoint::BranchPush], Duration::from_secs(60))
        .with_timeout(Duration::from_millis(50));
    let hooks: Vec<(String, Box<dyn Hook>)> = vec![("slow".to_string(), Box::new(hook))];

    let dispatcher = test_dispatcher(hooks);
    let ctx = create_test_context();

    let result = LORE_CONTEXT
        .scope(
            test_execution_context(),
            dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx),
        )
        .await;

    match result.unwrap_err() {
        HookError::Timeout { timeout, .. } => {
            assert_eq!(timeout, Duration::from_millis(50));
        }
        _ => panic!("Expected Timeout error"),
    }
}

#[tokio::test]
async fn test_dispatch_pre_panic_isolation() {
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

    let result = LORE_CONTEXT
        .scope(
            test_execution_context(),
            dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx),
        )
        .await;

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

#[tokio::test]
async fn test_dispatch_pre_panic_does_not_affect_other_hooks() {
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

    let result = LORE_CONTEXT
        .scope(
            test_execution_context(),
            dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx),
        )
        .await;

    // Restore previous panic hook before assertions
    std::panic::set_hook(prev_hook);

    assert!(result.is_err());

    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_dispatch_pre_context_read_only() {
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

    let result = LORE_CONTEXT
        .scope(
            test_execution_context(),
            dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx),
        )
        .await;
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
                    SlowPostHook::new("slow", &[HookPoint::BranchPush], Duration::from_millis(200))
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

#[tokio::test]
async fn test_dispatch_pre_performance_many_hooks() {
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
    let result = LORE_CONTEXT
        .scope(
            test_execution_context(),
            dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx),
        )
        .await;
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

    let hooks: Vec<(String, Box<dyn Hook>)> = vec![("no_msg".to_string(), Box::new(NoMessageHook))];
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
