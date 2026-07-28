// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_macro::LoreArgs;
use lore_revision::interface::LoreGlobalArgs;
use serde::Deserialize;
use serde::Serialize;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, LoreArgs)]
#[handler(start_local)]
/// Arguments for starting the Lore service process for the current repository (no parameters).
pub struct LoreServiceStartArgs {}

/// Start the Lore service process to manage the current repository.
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
#[allow(clippy::unused_async)]
pub async fn start(
    globals: LoreGlobalArgs,
    args: LoreServiceStartArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, start_local).await
}

async fn start_local(
    _globals: LoreGlobalArgs,
    _args: LoreServiceStartArgs,
    _callback: LoreEventCallback,
) -> i32 {
    // Set sentinel in repository that it is being controlled by service process

    // Attempt to connect to service process

    // If fail, try starting a new service process and connect again

    // Send a message to service the given repository

    1
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, LoreArgs)]
#[handler(stop_local)]
/// Arguments for stopping the Lore service process for the current or all repositories.
pub struct LoreServiceStopArgs {
    /// Stop all repositories rather than just the current one
    pub all: u8,
}

/// Stop the Lore service process for the current or all repositories.
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
#[allow(clippy::unused_async)]
pub async fn stop(
    globals: LoreGlobalArgs,
    args: LoreServiceStopArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, stop_local).await
}

async fn stop_local(
    _globals: LoreGlobalArgs,
    _args: LoreServiceStopArgs,
    _callback: LoreEventCallback,
) -> i32 {
    // Attempt to connect to service process

    // If successful, send a message to service the given repository

    // Remove sentinel in repository so that it is no longer being controlled by service process

    1
}
