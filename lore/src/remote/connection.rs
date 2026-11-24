// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fmt::Display;
use std::fmt::Formatter;
use std::pin::pin;
use std::sync::Arc;

use bytes::Bytes;
use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use lore_revision::service_state::ServiceStateImpl;
use parking_lot::Mutex;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::args::InvokableLoreArgs;
use crate::interface::LoreEvent;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::interface::LoreLogLevel;
use crate::remote::message::MessageToClient;
use crate::remote::message::MessageToServer;
use crate::remote::message::Opening;
use crate::remote::message::Schemas;
use crate::remote::message::blocking_read_message;
use crate::remote::message::blocking_read_opening;
use crate::remote::message::encode_message;
use crate::remote::message::schemas_differ;
use crate::remote::message::write_hello;
use crate::remote::message::write_payload;
use crate::remote::message::write_refusal;
use crate::remote::message::write_stop_result;
use crate::remote::network::UdsStream;
use crate::service::LoreServiceStopArgs;
use crate::service::initialization::push_service_log;

#[derive(Debug, Copy, Clone)]
pub struct ConnectionId(pub usize);

impl Display for ConnectionId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[error_set]
pub enum ConnectionError {}

#[derive(Debug, Error)]
#[error("connection {connection_id}: {error}")]
pub struct ConnectionErrorWithId {
    connection_id: ConnectionId,
    error: String,
}

impl ConnectionErrorWithId {
    pub fn new(error: ConnectionError, connection_id: ConnectionId) -> Self {
        Self {
            connection_id,
            error: error.to_string(),
        }
    }
}

/// Serves the command a client sends over `connection` on a task of its own, relaying the
/// command's events and then its status back over the same connection.
pub fn serve_connection(
    accept_state: Arc<ServiceStateImpl>,
    id: ConnectionId,
    connection: UdsStream,
) {
    let counted = accept_state.increment_connections();
    let connection_state = Arc::clone(&accept_state);
    lore_spawn!(async move {
        // Moved into the task so the count falls when the task
        // ends, including on a cancellation that skips the body.
        let _counted = counted;
        let connection = IpcConnection::new(id, connection, connection_state.clone());
        if let Err(err) = connection.handle_connection().await {
            connection_state.push_log(
                LoreLogLevel::Error,
                format!("Failed to handle connection: {err}"),
            );
        }
    });
}

/// What a client asks for over a connection.
#[allow(
    clippy::large_enum_variant,
    reason = "made once per connection and taken apart at once, where a box would allocate"
)]
enum Request {
    /// The client closed the connection before it asked for anything.
    Closed,
    /// A call, beside the payload its `LoreBytes` view.
    Call(MessageToServer, Bytes),
    /// A stop, which this service honours whatever the client's schemas.
    Stop { force: bool },
    /// A call from a client this service refused, for the reason given.
    Refused(String),
}

/// Reads what the client asks for, answering a call's hello first: with this build's schemas
/// where they equal the client's, so that the call can be decoded, or else with a refusal.
fn blocking_read_request(connection: &mut UdsStream) -> Result<Request, ConnectionError> {
    let opening =
        blocking_read_opening(connection.reader()).forward::<ConnectionError>("reading a hello")?;
    let reason = match opening {
        None => return Ok(Request::Closed),
        Some(Opening::Stop { force }) => return Ok(Request::Stop { force }),
        Some(Opening::Hello(schemas)) if schemas == Schemas::THIS_BUILD => {
            write_hello(connection.writer()).forward::<ConnectionError>("answering a hello")?;
            return Ok(
                match blocking_read_message(connection.reader())
                    .forward::<ConnectionError>("reading message")?
                {
                    Some((command, payload)) => Request::Call(command, payload),
                    None => Request::Closed,
                },
            );
        }
        Some(Opening::Hello(schemas)) => schemas_differ(Schemas::THIS_BUILD, schemas),
        Some(Opening::OtherProtocol(protocol)) => {
            format!(
                "the Lore service reads protocol version 3, and the client wrote another, whose \
                 first byte is {protocol}"
            )
        }
    };
    write_refusal(connection.writer(), &reason).forward::<ConnectionError>("refusing a call")?;
    Ok(Request::Refused(reason))
}

struct IpcConnection {
    id: ConnectionId,
    connection: UdsStream,
    service_state: Arc<ServiceStateImpl>,
}

impl IpcConnection {
    fn new(id: ConnectionId, connection: UdsStream, service_state: Arc<ServiceStateImpl>) -> Self {
        Self {
            id,
            connection,
            service_state,
        }
    }

    /// Stops this service as a relayed `LoreServiceStopArgs` call does, and answers with its status
    /// and the message of its failure.
    ///
    /// Boxed by its caller: a stop is rare, and its future would otherwise set the size of the
    /// task of every connection.
    async fn stop(self, force: bool) -> Result<(), ConnectionError> {
        let globals = LoreGlobalArgs {
            force: u8::from(force),
            ..LoreGlobalArgs::default()
        };
        let failure = Arc::new(Mutex::new(String::new()));
        let captured = Arc::clone(&failure);
        // The stop's log, its failure's trace included, goes to this service's log: the client
        // may be of a build that can't read this one's events.
        let log = Arc::clone(&self.service_state);
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::Complete(complete) = event {
                *captured.lock() = complete.error.message.to_string();
            }
            push_service_log(&log, event);
        }));
        let status = pin!(LoreServiceStopArgs {}.invoke_local(globals, callback)).await;
        let message = std::mem::take(&mut *failure.lock());
        let mut stream = self.connection;
        lore_base::lore_spawn_blocking!(move || write_stop_result(
            stream.writer(),
            status,
            &message
        ))
        .await
        .internal("failed writing")?
        .forward::<ConnectionError>("answering a stop")
    }

    async fn send_message(mut stream: UdsStream, payload: Vec<u8>) -> Result<(), ConnectionError> {
        lore_base::lore_spawn_blocking!(move || write_payload(stream.writer(), &payload))
            .await
            .internal("failed writing")?
            .forward::<ConnectionError>("writing message")?;
        Ok(())
    }

    async fn handle_connection(self) -> Result<(), ConnectionError> {
        let mut connection = self.connection.try_clone().internal("cloning connection")?;
        // Parks a core blocking thread until the peer sends or hangs up, so live
        // connections consume core's blocking pool one thread apiece.
        let (command, payload) = match lore_base::lore_spawn_blocking!(move || {
            blocking_read_request(&mut connection)
        })
        .await
        .internal("failed reading")??
        {
            Request::Closed => return Ok(()),
            Request::Call(command, payload) => (command, payload),
            Request::Stop { force } => return Box::pin(self.stop(force)).await,
            Request::Refused(reason) => {
                // A potential issue, not a failure of this service: a client of another build
                // keeps failing until its user replaces the service.
                self.service_state
                    .push_log(LoreLogLevel::Warn, format!("Refused a client: {reason}"));
                return Ok(());
            }
        };

        //TODO(UCS-16094): Determine if this should be unbounded or bounded
        // Create a channel so the callback task can send messages to this network thread, so they
        // can be forwarded to the client.
        let (to_client_sender, mut to_client_receiver) = mpsc::unbounded_channel::<Vec<u8>>();

        let invoke_state = Arc::clone(&self.service_state);
        lore_base::lore_spawn!(async move {
            let sender = to_client_sender.clone();
            let callback_state = Arc::clone(&invoke_state);

            // Note: this callback is intentionally NOT wrapped with .with_defaults().
            // It is the server-side event forwarder that must pass every LoreEvent
            // (including Error and Log) to the remote client so the client's own
            // wrapped callback can handle them. Wrapping here would swallow those
            // events on the server side and they would never reach the remote.
            //
            // The event is encoded here rather than where it is sent: a `LoreBytes` it carries
            // is valid only while the callback runs.
            let handler = pin!(command.invoke(Some(Box::new(move |event: &LoreEvent| {
                if let Err(error) =
                    to_client_sender.send(encode_message(&MessageToClient::Event(event)))
                {
                    callback_state.push_log(
                        LoreLogLevel::Error,
                        format!("Failed to send Event message to connection task: {error}"),
                    );
                }
            }))));
            let cli_result = handler.await;
            // Holds the views of any `LoreBytes` the command carried.
            drop(payload);

            if let Err(error) = sender.send(encode_message(
                &MessageToClient::<LoreEvent>::ApiResult(cli_result),
            )) {
                invoke_state.push_log(
                    LoreLogLevel::Error,
                    format!("Failed to send ApiResult message to connection task: {error}"),
                );
            }
        });

        while let Some(message) = to_client_receiver.recv().await {
            let stream = self.connection.try_clone().internal("cloning connection")?;
            if let Err(error) = Self::send_message(stream, message).await {
                self.service_state.push_log(
                    LoreLogLevel::Error,
                    format!(
                        "Failed to send message to client: {}",
                        ConnectionErrorWithId::new(error, self.id)
                    ),
                );
            }
        }

        Ok(())
    }
}
