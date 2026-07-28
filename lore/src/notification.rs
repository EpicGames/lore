// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Once;

use lore_macro::LoreArgs;
use lore_revision::interface::LoreGlobalArgs;
use serde::Deserialize;
use serde::Serialize;

use crate::call::repository_call_no_store;
use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(subscribe_local)]
/// Arguments for subscribing to repository notifications (no parameters).
pub struct LoreNotificationSubscribeArgs {}

/// Subscribe to repository notifications, delivering push events to the callback until a corresponding unsubscribe call.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Notification Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::NotificationSubscribed`](crate::interface::LoreEvent::NotificationSubscribed) | Emitted when successfully subscribed to repository notifications |
/// | [`LoreEvent::NotificationBranchCreated`](crate::interface::LoreEvent::NotificationBranchCreated) | Emitted when a branch is created in the repository (push notification) |
/// | [`LoreEvent::NotificationBranchDeleted`](crate::interface::LoreEvent::NotificationBranchDeleted) | Emitted when a branch is deleted in the repository (push notification) |
/// | [`LoreEvent::NotificationBranchPushed`](crate::interface::LoreEvent::NotificationBranchPushed) | Emitted when a branch is pushed to (push notification) |
/// | [`LoreEvent::NotificationResourceLocked`](crate::interface::LoreEvent::NotificationResourceLocked) | Emitted when a resource is locked (push notification) |
/// | [`LoreEvent::NotificationResourceUnlocked`](crate::interface::LoreEvent::NotificationResourceUnlocked) | Emitted when a resource is unlocked (push notification) |
pub async fn subscribe(
    globals: LoreGlobalArgs,
    args: LoreNotificationSubscribeArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, subscribe_local).await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, LoreArgs)]
#[handler(unsubscribe_local)]
/// Arguments for unsubscribing from repository notifications (no parameters).
pub struct LoreNotificationUnsubscribeArgs {}

/// Unsubscribe from repository notifications, stopping the delivery of push events to the callback.
///
/// # Events
///
/// ## Standard Events
///
/// These events are emitted by all interface functions:
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::Log`](crate::interface::LoreEvent::Log) | Diagnostic messages throughout execution |
/// | [`LoreEvent::Error`](crate::interface::LoreEvent::Error) | Emitted for a non-fatal error during the operation |
/// | [`LoreEvent::Complete`](crate::interface::LoreEvent::Complete) | Always emitted at the end; `status` is `0` on success or the error code on failure |
/// | [`LoreEvent::End`](crate::interface::LoreEvent::End) | Always emitted after `Complete` to signal callback termination |
///
/// ## Notification Events
///
/// | Event | Description |
/// |-------|-------------|
/// | [`LoreEvent::NotificationUnsubscribed`](crate::interface::LoreEvent::NotificationUnsubscribed) | Emitted when successfully unsubscribed from repository notifications |
pub async fn unsubscribe(
    globals: LoreGlobalArgs,
    args: LoreNotificationUnsubscribeArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, unsubscribe_local).await
}

static INITIALIZER: Once = Once::new();

/// Subscribe to events for the repository. The callback will receive events until a corresponding
/// call to unsubscribe on the same repository.
async fn subscribe_local(
    globals: LoreGlobalArgs,
    args: LoreNotificationSubscribeArgs,
    callback: LoreEventCallback,
) -> i32 {
    INITIALIZER.call_once(|| {
        lore_notification::initialize();
    });

    repository_call_no_store(
        globals,
        callback,
        args,
        subscribe,
        move |repository, _args| lore_revision::notification::subscribe(repository),
    )
    .await
}

/// Unsubscribe to events for the repository.
async fn unsubscribe_local(
    globals: LoreGlobalArgs,
    args: LoreNotificationUnsubscribeArgs,
    callback: LoreEventCallback,
) -> i32 {
    repository_call_no_store(
        globals,
        callback,
        args,
        unsubscribe,
        move |repository, _args| lore_revision::notification::unsubscribe(repository),
    )
    .await
}
