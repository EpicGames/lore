// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::io::Write;

use bytes::Bytes;
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

#[error_set]
pub enum ServiceCallError {
    ServiceUnavailable,
}

impl EventError for ServiceCallError {
    fn translated(&self) -> LoreError {
        match self {
            ServiceCallError::ServiceUnavailable(_) => LoreError::ServiceUnavailable,
            ServiceCallError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Reports that a routed call never ran because no service could be reached or
/// started, as the one failure a caller can act on. Carries the underlying
/// failure as its reason, so the detail is not lost behind the category.
fn unavailable(error: &impl std::fmt::Display) -> ServiceCallError {
    ServiceCallError::from(ServiceUnavailable {
        reason: error.to_string(),
    })
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

pub async fn service_call<ArgsType: LoreArgs + Clone + Send + 'static>(
    mut globals: LoreGlobalArgs,
    args: ArgsType,
    callback: LoreEventCallback,
) -> i32 {
    fill_working_directory(&mut globals);
    let mut event_dispatcher = EventDispatcher::new(callback);

    service_call_impl(&mut event_dispatcher, globals, args)
        .await
        .unwrap_or_else(|err| {
            event_dispatcher.send(LoreEvent::Log(EventDispatcher::make_log(
                LoreLogLevel::Error,
                format!("Failed to send command to Lore service because: {err}"),
            )));
            let code = err.ffi_code();
            event_dispatcher.send_error(err);
            code
        })
}

/// Opens a connection to the service and writes the already-serialized message,
/// returning the connection to read the reply from.
///
/// Takes [`Bytes`] rather than the buffer itself because the caller sends the
/// same message twice when it has to start a service first, and the write
/// happens on a blocking thread that needs to own what it writes. Sharing the
/// buffer keeps the second attempt from copying a payload that is as large as
/// the command — every path of a bulk `stage`, for one.
async fn connect_and_send(message_bytes: Bytes) -> Result<UdsStream, ServiceCallError> {
    lore_base::lore_spawn_blocking!(move || {
        let mut connection =
            UdsStream::connect().forward::<ServiceCallError>("connecting to local socket")?;
        connection
            .writer()
            .write_all(&message_bytes)
            .internal("sending message")?;
        Ok::<UdsStream, ServiceCallError>(connection)
    })
    .await
    .internal("joining connection task")?
}

pub async fn service_call_impl<ArgsType: LoreArgs + Clone + Send + 'static>(
    event_dispatcher: &mut EventDispatcher,
    globals: LoreGlobalArgs,
    args: ArgsType,
) -> Result<i32, ServiceCallError> {
    if !uds_supported() {
        return Err(ServiceCallError::internal("OS doesn't support IPC"));
    }

    let message = MessageToServer {
        globals,
        command: args.to_command(),
    };
    let message_bytes = Bytes::from(
        write_v1_message(message, SerializationType::Json)
            .forward::<ServiceCallError>("serializing message")?,
    );

    // Send to an existing service first, and only start one if that fails, so
    // the common path opens a single connection rather than a liveness probe
    // followed by the real one.
    let connection = if let Ok(connection) = connect_and_send(message_bytes.clone()).await {
        connection
    } else {
        // Auto-start carries no executable: the routed call has no field for
        // one, so this is the current-executable rule, which only the CLI meets.
        crate::remote::process::ensure_running(None)
            .await
            .map_err(|error| unavailable(&error))?;
        connect_and_send(message_bytes)
            .await
            .map_err(|error| unavailable(&error))?
    };

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
        "Lore service closed the connection without sending a result; \
         it may have been stopped or terminated while the request was in flight",
    ))
}

/// Sends one command to a service that is already running and returns as soon
/// as it is written, without waiting for a result. Used for requests whose
/// effect is that the service exits, where waiting for a reply would race the
/// service's own shutdown. Does not start a service.
pub async fn service_send_no_reply<ArgsType: LoreArgs + Clone + Send + 'static>(
    globals: LoreGlobalArgs,
    args: ArgsType,
) -> Result<(), ServiceCallError> {
    if !uds_supported() {
        return Err(ServiceCallError::internal("OS doesn't support IPC"));
    }

    lore_base::lore_spawn_blocking!(|| {
        let mut connection =
            UdsStream::connect().forward::<ServiceCallError>("connecting to local socket")?;

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
        Ok::<(), ServiceCallError>(())
    })
    .await
    .internal("joining send task")?
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
