// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fmt::Display;
use std::fmt::Formatter;
use std::io::Write;

use lore_error_set::prelude::*;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::interface::LoreEvent;
use crate::remote::message::MessageToClient;
use crate::remote::message::MessageToServer;
use crate::remote::message::SerializationType;
use crate::remote::message::V1Header;
use crate::remote::message::blocking_read_v1_message;
use crate::remote::message::write_v1_message;
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
pub fn serve_connection(id: ConnectionId, connection: UdsStream) {
    lore_base::lore_spawn!(IpcConnection::new(id, connection).handle_connection());
}

struct IpcConnection {
    id: ConnectionId,
    connection: UdsStream,
}

impl IpcConnection {
    fn new(id: ConnectionId, connection: UdsStream) -> Self {
        Self { id, connection }
    }

    async fn send_message(
        mut stream: UdsStream,
        message: MessageToClient,
        serialization_type: SerializationType,
    ) -> Result<(), ConnectionError> {
        let message_bytes = write_v1_message(message, serialization_type)
            .forward::<ConnectionError>("writing message")?;
        lore_base::lore_spawn_blocking!(move || stream
            .writer()
            .write_all(message_bytes.as_slice()))
        .await
        .internal("failed writing")?
        .internal("io")?;
        Ok(())
    }

    async fn handle_connection(self) {
        let id = self.id;
        if let Err(error) = self.handle_connection_impl().await {
            eprintln!(
                "Error in connection: {}",
                ConnectionErrorWithId::new(error, id)
            );
        }
    }

    async fn handle_connection_impl(self) -> Result<(), ConnectionError> {
        let mut connection = self.connection.try_clone().internal("cloning connection")?;
        // Parks a core blocking thread until the peer sends or hangs up, so live
        // connections consume core's blocking pool one thread apiece.
        let message: Option<(V1Header, MessageToServer)> =
            lore_base::lore_spawn_blocking!(move || blocking_read_v1_message(connection.reader()))
                .await
                .internal("failed reading")?
                .forward::<ConnectionError>("reading message")?;

        let Some((header, command)) = message else {
            return Ok(());
        };

        //TODO(UCS-16094): Determine if this should be unbounded or bounded
        // Create a channel so the callback task can send messages to this network thread, so they
        // can be forwarded to the client.
        let (to_client_sender, mut to_client_receiver) =
            mpsc::unbounded_channel::<(MessageToClient, SerializationType)>();

        lore_base::lore_spawn!(async move {
            let sender = to_client_sender.clone();

            // Note: this callback is intentionally NOT wrapped with .with_defaults().
            // It is the server-side event forwarder that must pass every LoreEvent
            // (including Error and Log) to the remote client so the client's own
            // wrapped callback can handle them. Wrapping here would swallow those
            // events on the server side and they would never reach the remote.
            let cli_result = command
                .invoke(Some(Box::new(move |event: &LoreEvent| {
                    if let Err(error) = to_client_sender.send((
                        MessageToClient::Event(event.clone()),
                        header.serialization_type,
                    )) {
                        eprintln!("Failed to send Event message to connection task: {error}");
                    }
                })))
                .await;

            if let Err(error) = sender.send((
                MessageToClient::ApiResult(cli_result),
                header.serialization_type,
            )) {
                eprintln!("Failed to send ApiResult message to connection task: {error}");
            }
        });

        while let Some((message, serialization_type)) = to_client_receiver.recv().await {
            let stream = self.connection.try_clone().internal("cloning connection")?;
            if let Err(error) = Self::send_message(stream, message, serialization_type).await {
                eprintln!(
                    "Failed to send message to client: {}",
                    ConnectionErrorWithId::new(error, self.id)
                );
            }
        }

        Ok(())
    }
}
