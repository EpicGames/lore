// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use bytes::Bytes;
use lore_base::env::CallEnvironment;
use lore_base::error::ServiceUnavailable;
use lore_base::log::LoreLogLevel;
use lore_error_set::prelude::*;
use lore_revision::event::LoreErrorDetail;
use lore_revision::event::LoreEvent;
use lore_revision::relay::EventDispatcher;

use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::remote::command::LoreCommand;
use crate::remote::message::MessageToClient;
use crate::remote::message::MessageToServer;
use crate::remote::message::blocking_read_message;
use crate::remote::message::write_message;
use crate::remote::network::UdsStream;
use crate::remote::network::uds_supported;
use crate::remote::service_process::connect_or_spawn_service;

#[error_set]
pub enum ServiceCallError {
    ServiceUnavailable,
}

/// Records the directory the service resolves this call's relative paths
/// against, when the caller left it unset. The service runs in a directory
/// unrelated to the caller's, so without this a relative path would resolve
/// there rather than where the caller ran. A caller that set the field, such as
/// an installed tool that runs from a fixed directory but wants relative paths
/// resolved elsewhere, keeps its value.
#[allow(clippy::disallowed_methods)]
fn fill_working_directory(globals: &mut LoreGlobalArgs) {
    if globals.working_directory().is_some() {
        return;
    }
    if let Ok(directory) = std::env::current_dir() {
        globals.working_directory = directory.display().to_string().into();
    }
}

/// Runs the call on the service, starting one when none is running.
pub async fn service_call(
    globals: LoreGlobalArgs,
    command: LoreCommand,
    callback: LoreEventCallback,
) -> i32 {
    run_service_call(None, globals, command, callback).await
}

/// Runs the call over a connection the caller already holds, rather than one
/// resolved here.
///
/// A caller that acts on the service only when one is already running connects
/// itself, so that its check for a service and the call it makes cannot
/// disagree about whether there was one to act on.
pub async fn service_call_over(
    connection: UdsStream,
    globals: LoreGlobalArgs,
    command: LoreCommand,
    callback: LoreEventCallback,
) -> i32 {
    run_service_call(Some(connection), globals, command, callback).await
}

async fn run_service_call(
    connection: Option<UdsStream>,
    mut globals: LoreGlobalArgs,
    command: LoreCommand,
    callback: LoreEventCallback,
) -> i32 {
    fill_working_directory(&mut globals);
    let mut event_dispatcher = EventDispatcher::new(callback);

    match service_call_impl(&mut event_dispatcher, globals, command, connection).await {
        Ok(status) => {
            // The read loop returns on the result, leaving events queued for this
            // process's forwarder. Drained so a caller reading what its callback
            // collected sees all of it; a local call's `complete` does the same.
            event_dispatcher.drain().await;
            status
        }
        Err(err) => {
            // No service completed the command, so it completes here as a failing
            // command does, with the failure's own code: a caller has to be able
            // to tell a call that never reached a service from one the service
            // ran and reported on.
            event_dispatcher.send(LoreEvent::Log(EventDispatcher::make_log(
                LoreLogLevel::Error,
                format!("Failed to send command to Lore service because: {err}"),
            )));
            event_dispatcher
                .complete(LoreErrorDetail::from_error(&err))
                .await
        }
    }
}

pub async fn service_call_impl(
    event_dispatcher: &mut EventDispatcher,
    globals: LoreGlobalArgs,
    command: LoreCommand,
    connection: Option<UdsStream>,
) -> Result<i32, ServiceCallError> {
    if !uds_supported() {
        // No service can be reached here at all, which is the same answer for a
        // caller as one that could not be started.
        return Err(ServiceUnavailable {
            reason: "OS doesn't support IPC".to_string(),
        }
        .into());
    }

    let connection = match connection {
        Some(connection) => connection,
        None => connect_or_spawn_service()
            .await
            .forward::<ServiceCallError>("reaching a Lore service")?,
    };

    let connection = lore_base::lore_spawn_blocking!(move || {
        let mut connection = connection;

        let message = MessageToServer {
            globals,
            command,
            environment: CallEnvironment::capture(),
        };
        write_message(connection.writer(), &message)
            .forward::<ServiceCallError>("sending message")?;
        Ok::<UdsStream, ServiceCallError>(connection)
    })
    .await
    .internal("joining connection task")??;

    'read_from_stream: loop {
        let mut connection = connection.try_clone().internal("cloning connection")?;
        let message: Option<(MessageToClient, Bytes)> =
            lore_base::lore_spawn_blocking!(move || blocking_read_message(connection.reader()))
                .await
                .internal("joining receive task")?
                .forward::<ServiceCallError>("receiving message")?;
        match message {
            Some((message, payload)) => {
                if let Some(api_result) = handle_message(event_dispatcher, message, payload)? {
                    return Ok(api_result);
                }
            }
            None => {
                break 'read_from_stream;
            }
        }
    }

    Err(ServiceCallError::internal(
        "Lore service closed connection without sending a result",
    ))
}

/// Hands an event to the callback, keeping `payload`, which holds any `LoreBytes` it carries,
/// alive until the callback has run.
pub fn handle_message(
    event_dispatcher: &mut EventDispatcher,
    message: MessageToClient,
    payload: Bytes,
) -> Result<Option<i32>, ServiceCallError> {
    match message {
        MessageToClient::Event(event) => {
            event_dispatcher.send_with_bytes(event, payload);
            Ok(None)
        }
        MessageToClient::ApiResult(api_result) => Ok(Some(api_result)),
    }
}
