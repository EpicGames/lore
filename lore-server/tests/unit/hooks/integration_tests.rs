// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use async_trait::async_trait;
use lore_base::runtime::LORE_CONTEXT;
use lore_revision::lore::RepositoryId;
use lore_server::hooks::*;

fn test_execution_context() -> std::sync::Arc<lore_revision::interface::ExecutionContext> {
    lore_server::util::setup_execution("test", "test".to_string(), "test-user".to_string())
}

/// Mock hook for integration testing with two-phase execution
struct TestHook {
    name: &'static str,
    points: &'static [HookPoint],
    pre_count: Arc<AtomicUsize>,
    post_count: Arc<AtomicUsize>,
}

impl TestHook {
    fn new(name: &'static str, points: &'static [HookPoint]) -> Self {
        Self {
            name,
            points,
            pre_count: Arc::new(AtomicUsize::new(0)),
            post_count: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl Hook for TestHook {
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

struct TestHookFactory {
    name: &'static str,
    points: &'static [HookPoint],
}

impl HookFactory for TestHookFactory {
    fn name(&self) -> &'static str {
        self.name
    }

    fn create(&self, _config: &toml::Value) -> Result<Box<dyn Hook>, HookError> {
        Ok(Box::new(TestHook::new(self.name, self.points)))
    }
}

#[test]
fn test_full_workflow() {
    // 1. Create registry
    let mut registry = HookRegistry::new();

    // 2. Register hook factories
    registry.register_hook(Box::new(TestHookFactory {
        name: "test_hook",
        points: &[HookPoint::BranchPush, HookPoint::BranchCreate],
    }));

    // 3. Verify registration
    assert!(registry.has_hook("test_hook"));
    assert_eq!(registry.list_hooks().len(), 1);
}

#[tokio::test]
async fn test_end_to_end_two_phase_dispatch() {
    LORE_CONTEXT
        .scope(test_execution_context(), async {
            // 1. Create registry and register hooks
            let mut registry = HookRegistry::new();
            registry.register_hook(Box::new(TestHookFactory {
                name: "e2e_hook",
                points: &[HookPoint::BranchPush],
            }));

            // 2. Create enabled hooks from settings
            let mut settings = std::collections::HashMap::new();
            settings.insert(
                "e2e_hook".to_string(),
                HookSettings {
                    enabled: true,
                    config: toml::Value::Table(toml::map::Map::new()),
                },
            );

            let enabled_hooks = registry.create_enabled_hooks(&settings).unwrap();
            assert_eq!(enabled_hooks.len(), 1);

            // 3. Create dispatcher with default timeouts
            let dispatcher = HookDispatcher::from_hooks_default(enabled_hooks);
            assert!(dispatcher.has_hooks(HookPoint::BranchPush));

            // 4. Create context
            let ctx = HookContext::builder()
                .correlation_id("e2e-test-123")
                .hook_point(HookPoint::BranchPush)
                .repository(RepositoryId::default())
                .user("test_user")
                .build();

            // 5. Phase 1: Dispatch pre-handlers (awaited)
            let result = dispatcher.dispatch_pre(HookPoint::BranchPush, &ctx).await;
            assert!(result.is_ok());

            // 6. Phase 2: Spawn post-handlers (asynchronous)
            dispatcher.spawn_post(HookPoint::BranchPush, ctx);

            // Give post-handlers time to execute
            tokio::time::sleep(Duration::from_millis(50)).await;
        })
        .await;
}

#[tokio::test]
async fn test_hook_veto_workflow() {
    LORE_CONTEXT
        .scope(test_execution_context(), async {
            struct VetoHook;

            #[async_trait]
            impl Hook for VetoHook {
                fn name(&self) -> &'static str {
                    "veto"
                }

                fn hook_points(&self) -> &'static [HookPoint] {
                    &[HookPoint::BranchPush]
                }

                async fn pre_handler(&self, ctx: &HookContext) -> Result<(), HookError> {
                    // Veto operations from blocked users
                    if let Some(user) = ctx.user()
                        && user.starts_with("blocked_")
                    {
                        return Err(HookError::rejected(
                            self.name(),
                            format!("User '{user}' is blocked"),
                            StatusCode::PermissionDenied,
                        ));
                    }
                    Ok(())
                }
            }

            struct VetoHookFactory;

            impl HookFactory for VetoHookFactory {
                fn name(&self) -> &'static str {
                    "veto"
                }

                fn create(&self, _config: &toml::Value) -> Result<Box<dyn Hook>, HookError> {
                    Ok(Box::new(VetoHook))
                }
            }

            // Setup
            let mut registry = HookRegistry::new();
            registry.register_hook(Box::new(VetoHookFactory));

            let mut settings = std::collections::HashMap::new();
            settings.insert(
                "veto".to_string(),
                HookSettings {
                    enabled: true,
                    config: toml::Value::Table(toml::map::Map::new()),
                },
            );

            let enabled_hooks = registry.create_enabled_hooks(&settings).unwrap();
            let dispatcher = HookDispatcher::from_hooks_default(enabled_hooks);

            // Test: Allowed user
            let allowed_ctx = HookContext::builder()
                .correlation_id("allowed")
                .hook_point(HookPoint::BranchPush)
                .repository(RepositoryId::default())
                .user("normal_user")
                .build();

            let result = dispatcher
                .dispatch_pre(HookPoint::BranchPush, &allowed_ctx)
                .await;
            assert!(result.is_ok());

            // Test: Blocked user
            let blocked_ctx = HookContext::builder()
                .correlation_id("blocked")
                .hook_point(HookPoint::BranchPush)
                .repository(RepositoryId::default())
                .user("blocked_user")
                .build();

            let result = dispatcher
                .dispatch_pre(HookPoint::BranchPush, &blocked_ctx)
                .await;
            assert!(result.is_err());

            match result.unwrap_err() {
                HookError::Rejected {
                    hook_name,
                    message,
                    status,
                } => {
                    assert_eq!(hook_name, "veto");
                    assert!(message.contains("blocked_user"));
                    assert_eq!(status, StatusCode::PermissionDenied);
                }
                _ => panic!("Expected Rejected error"),
            }
        })
        .await;
}
