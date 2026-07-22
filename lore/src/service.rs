// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::event::EventError;
use lore_revision::global::GlobalConfig;
use lore_revision::interface::LoreGlobalArgs;
use serde::Deserialize;
use serde::Serialize;

use crate::call::no_repository_call;
use crate::call_delegation::invalidate_use_service_cache;
use crate::interface::LoreEventCallback;
use crate::remote::call::service_send_no_reply;
use crate::remote::process;

#[error_set]
pub enum ServiceError {}

impl EventError for ServiceError {}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, LoreArgs)]
#[handler(start_local)]
/// Arguments for starting the Lore service process (no parameters).
pub struct LoreServiceStartArgs {}

/// Start the Lore service process, if it is not already running.
///
/// Always runs locally rather than being dispatched to the service, which
/// cannot be asked to start itself while it is not running.
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
pub async fn start(
    globals: LoreGlobalArgs,
    args: LoreServiceStartArgs,
    callback: LoreEventCallback,
) -> i32 {
    start_local(globals, args, callback).await
}

async fn start_local(
    globals: LoreGlobalArgs,
    args: LoreServiceStartArgs,
    callback: LoreEventCallback,
) -> i32 {
    let command = async move |_args| -> Result<(), ServiceError> {
        if process::running_as_service() {
            return Ok(());
        }
        process::ensure_running()
            .await
            .forward::<ServiceError>("starting the Lore service")?;
        Ok(())
    };
    no_repository_call(globals, callback, args, "service start", command).await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, LoreArgs)]
#[handler(stop_local)]
/// Arguments for stopping the Lore service process (no parameters).
pub struct LoreServiceStopArgs {}

/// Stop the Lore service process, if it is running.
///
/// Always runs locally rather than being dispatched to the service, which would
/// start a service in order to deliver the request to stop it.
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
pub async fn stop(
    globals: LoreGlobalArgs,
    args: LoreServiceStopArgs,
    callback: LoreEventCallback,
) -> i32 {
    stop_local(globals, args, callback).await
}

async fn stop_local(
    globals: LoreGlobalArgs,
    args: LoreServiceStopArgs,
    callback: LoreEventCallback,
) -> i32 {
    let globals_for_send = globals.clone();
    let command = async move |args| -> Result<(), ServiceError> {
        if process::running_as_service() {
            process::request_shutdown().forward::<ServiceError>("stopping the Lore service")?;
            return Ok(());
        }

        if !process::is_running() {
            return Ok(());
        }

        // The service can exit between the check above and the send below —
        // another stop, or a termination signal. A failed send is then only an
        // error if the service is somehow still running; a stopped service is
        // exactly the outcome asked for.
        if let Err(error) = service_send_no_reply(globals_for_send, args).await {
            if process::is_running() {
                return Err(error).forward::<ServiceError>("sending stop to the Lore service");
            }
            return Ok(());
        }

        if !process::wait_until_stopped().await {
            return Err(ServiceError::internal(
                "the Lore service did not stop in time",
            ));
        }
        Ok(())
    };
    no_repository_call(globals, callback, args, "service stop", command).await
}

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, LoreArgs)]
#[handler(set_use_automatically_local)]
/// Arguments for setting whether Lore automatically routes calls through the service process.
pub struct LoreServiceSetUseAutomaticallyArgs {
    /// Automatically use the service process
    pub enabled: u8,
}

/// Sets whether Lore automatically routes calls through the service process.
///
/// Always runs locally rather than being dispatched to the service, which would
/// start a service only to be told that the service should no longer be used.
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
pub async fn set_use_automatically(
    globals: LoreGlobalArgs,
    args: LoreServiceSetUseAutomaticallyArgs,
    callback: LoreEventCallback,
) -> i32 {
    set_use_automatically_local(globals, args, callback).await
}

async fn set_use_automatically_local(
    globals: LoreGlobalArgs,
    args: LoreServiceSetUseAutomaticallyArgs,
    callback: LoreEventCallback,
) -> i32 {
    let command =
        async move |args: LoreServiceSetUseAutomaticallyArgs| -> Result<(), ServiceError> {
            let (mut config, lock) = GlobalConfig::load_locked()
                .await
                .internal("loading global config")?;
            if args.enabled != 0 {
                config.use_service_automatically = Some(true);
            } else {
                config.use_service_automatically = None;
            }
            config.save(lock).await.internal("saving global config")?;
            invalidate_use_service_cache();
            Ok(())
        };
    no_repository_call(
        globals,
        callback,
        args,
        "service set-use-automatically",
        command,
    )
    .await
}
