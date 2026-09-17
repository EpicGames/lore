// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::io::Write;

use lore_base::error::ServiceUnavailable;
use lore_base::log::LoreLogLevel;
use lore_error_set::prelude::*;
use lore_revision::event::EventError;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreError;
use lore_revision::relay::EventDispatcher;

use crate::args::LoreArgs;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::remote::message::MessageToClient;
use crate::remote::message::MessageToServer;
use crate::remote::message::SerializationType;
use crate::remote::message::V1Header;
use crate::remote::message::blocking_read_v1_message;
use crate::remote::message::write_v1_message;
use crate::remote::network::UdsStream;
use crate::remote::network::uds_supported;
use crate::remote::service_process::connect_or_spawn_service;

#[error_set]
pub enum ServiceCallError {
    ServiceUnavailable,
}

impl EventError for ServiceCallError {
    fn translated(&self) -> LoreError {
        match self {
            // Carried through from resolving the service, and the only failure
            // here that means the call never reached one. Everything else went
            // wrong while talking to a service that was there.
            Self::ServiceUnavailable(_) => LoreError::ServiceUnavailable,
            Self::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
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
pub async fn service_call<ArgsType: LoreArgs + Clone + Send + 'static>(
    globals: LoreGlobalArgs,
    args: ArgsType,
    callback: LoreEventCallback,
) -> i32 {
    run_service_call(None, globals, args, callback).await
}

/// Runs the call over a connection the caller already holds, rather than one
/// resolved here.
///
/// A caller that acts on the service only when one is already running connects
/// itself, so that its check for a service and the call it makes cannot
/// disagree about whether there was one to act on.
pub async fn service_call_over<ArgsType: LoreArgs + Clone + Send + 'static>(
    connection: UdsStream,
    globals: LoreGlobalArgs,
    args: ArgsType,
    callback: LoreEventCallback,
) -> i32 {
    run_service_call(Some(connection), globals, args, callback).await
}

async fn run_service_call<ArgsType: LoreArgs + Clone + Send + 'static>(
    connection: Option<UdsStream>,
    mut globals: LoreGlobalArgs,
    args: ArgsType,
    callback: LoreEventCallback,
) -> i32 {
    fill_working_directory(&mut globals);
    let mut event_dispatcher = EventDispatcher::new(callback);

    let status = service_call_impl(&mut event_dispatcher, globals, args, connection)
        .await
        .unwrap_or_else(|err| {
            // The failure's own code, not a flat 1: a caller has to be able to
            // tell a call that never reached a service from one the service ran
            // and reported on, and 1 is also the CLI's own general failure, which
            // would make a routed failure indistinguishable from any other.
            let status = err.ffi_code();
            event_dispatcher.send(LoreEvent::Log(EventDispatcher::make_log(
                LoreLogLevel::Error,
                format!("Failed to send command to Lore service because: {err}"),
            )));
            event_dispatcher.send_error(err);
            status
        });

    // The read loop returns on the result, leaving events queued for this
    // process's forwarder. Drained so a caller reading what its callback
    // collected sees all of it; a local call's `complete` does the same.
    event_dispatcher.drain().await;

    status
}

pub async fn service_call_impl<ArgsType: LoreArgs + Clone + Send + 'static>(
    event_dispatcher: &mut EventDispatcher,
    globals: LoreGlobalArgs,
    args: ArgsType,
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
            command: args.to_command(),
        };

        let message_bytes = write_v1_message(message, SerializationType::Json)
            .forward::<ServiceCallError>("serializing message")?;

        connection
            .writer()
            .write_all(&message_bytes)
            .internal("sending message")?;
        Ok::<UdsStream, ServiceCallError>(connection)
    })
    .await
    .internal("joining connection task")??;

    'read_from_stream: loop {
        let mut connection = connection.try_clone().internal("cloning connection")?;
        let message: Option<(V1Header, MessageToClient)> =
            lore_base::lore_spawn_blocking!(move || blocking_read_v1_message(connection.reader()))
                .await
                .internal("joining receive task")?
                .forward::<ServiceCallError>("receiving message")?;
        match message {
            Some((_header, message)) => {
                if let Some(api_result) = handle_message(event_dispatcher, message)? {
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

pub fn handle_message(
    event_dispatcher: &mut EventDispatcher,
    message: MessageToClient,
) -> Result<Option<i32>, ServiceCallError> {
    match message {
        MessageToClient::Event(event) => {
            event_dispatcher.send(event);
            Ok(None)
        }
        MessageToClient::ApiResult(api_result) => Ok(Some(api_result)),
    }
}
