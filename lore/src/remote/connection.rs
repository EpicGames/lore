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
use thiserror::Error;
use tokio::sync::mpsc;

use crate::interface::LoreEvent;
use crate::interface::LoreLogLevel;
use crate::remote::message::MessageToClient;
use crate::remote::message::MessageToServer;
use crate::remote::message::blocking_read_message;
use crate::remote::message::encode_message;
use crate::remote::message::write_payload;
use crate::remote::network::UdsStream;

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
        let message: Option<(MessageToServer, Bytes)> =
            lore_base::lore_spawn_blocking!(move || blocking_read_message(connection.reader()))
                .await
                .internal("failed reading")?
                .forward::<ConnectionError>("reading message")?;

        let Some((command, payload)) = message else {
            return Ok(());
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
