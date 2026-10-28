// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::time::Duration;

use async_trait::async_trait;
use lore_server::hooks::context::HookContext;
use lore_server::hooks::traits::*;

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

#[tokio::test]
async fn test_hook_default_pre_handler() {
    use lore_revision::lore::RepositoryId;

    let hook = MinimalHook;
    let ctx = HookContext::builder()
        .correlation_id("test")
        .hook_point(HookPoint::BranchPush)
        .repository(RepositoryId::default())
        .build();

    let result = hook.pre_handler(&ctx).await;
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

    async fn pre_handler(&self, _ctx: &HookContext) -> Result<(), HookError> {
        Err(HookError::rejected(
            self.name(),
            "Operation not allowed",
            StatusCode::PermissionDenied,
        ))
    }
}

#[tokio::test]
async fn test_hook_pre_handler_rejection() {
    use lore_revision::lore::RepositoryId;

    let hook = RejectingHook;
    let ctx = HookContext::builder()
        .correlation_id("test")
        .hook_point(HookPoint::BranchPush)
        .repository(RepositoryId::default())
        .build();

    let result = hook.pre_handler(&ctx).await;
    assert!(result.is_err());

    let err = result.unwrap_err();
    assert_eq!(err.hook_name(), "rejecting");
    assert_eq!(err.status_code(), Some(StatusCode::PermissionDenied));
}
