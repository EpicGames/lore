// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore::interface::LoreEvent;
use lore::remote::connection::ConnectionError;
use lore::remote::connection::ConnectionErrorWithId;
use lore::remote::connection::ConnectionId;
use lore::remote::message::MessageToClient;
use lore::remote::message::MessageToServer;
use lore::remote::message::SerializationType;
use lore::remote::message::V1Header;
use lore::remote::message::blocking_read_v1_message;
use lore::remote::message::write_v1_message;
use lore::remote::network::UdsListener;
use lore::remote::network::UdsStream;
use lore::remote::network::uds_supported;
use lore::runtime;
use lore_error_set::prelude::*;
use tokio::sync::mpsc;

use crate::eprintln;
use crate::println;
use crate::util::listen_for_termination;

#[error_set]
pub enum ServiceMainError {}

/// Bounds how long shutdown waits for the accept loop to unwind, so that a
/// wake-up connection that never lands cannot keep the process alive. Anything
/// left behind is a stale socket, which the next start detects and removes.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn service_main(
    listening_signal: Option<tokio::sync::oneshot::Sender<()>>,
) -> Result<(), ServiceMainError> {
    if !uds_supported() {
        return Err(ServiceMainError::internal("IPC not supported on this OS"));
    }

    let listener: UdsListener = UdsListener::new().internal("Failed to start listener socket")?;

    if let Some(listening_signal) = listening_signal {
        listening_signal
            .send(())
            .map_err(|_err| ServiceMainError::internal("Couldn't signal listening"))?;
    }

    let shutting_down = Arc::new(AtomicBool::new(false));
    let accept_shutting_down = Arc::clone(&shutting_down);

    let accept_task = runtime().spawn_blocking(move || {
        let mut connection_id = 0;
        loop {
            match listener.accept() {
                Ok(stream) => {
                    if accept_shutting_down.load(Ordering::SeqCst) {
                        break;
                    }
                    let new_connection_id = connection_id;
                    connection_id += 1;
                    runtime().spawn(async move {
                        IpcConnection::new(ConnectionId(new_connection_id), stream)
                            .handle_connection()
                            .await;
                    });
                }
                Err(err) => {
                    if accept_shutting_down.load(Ordering::SeqCst) {
                        break;
                    }
                    eprintln!("Failed when accepting: {err}");
                }
            }
        }
    });

    match listen_for_termination(None).await {
        Ok(()) => {
            println!("Shutting down Lore service");
            shutting_down.store(true, Ordering::SeqCst);
            if let Err(error) = UdsStream::connect() {
                eprintln!("Failed to wake the accept loop: {error}");
            }
            if tokio::time::timeout(SHUTDOWN_TIMEOUT, accept_task)
                .await
                .is_err()
            {
                eprintln!("Timed out waiting for the accept loop to stop");
            }
        }
        Err(error) => {
            // Without signal handling there is no graceful path, so keep
            // serving until the process is killed.
            eprintln!("Failed to listen for termination signals: {error}");
            let _ = accept_task.await;
        }
    }

    Ok(())
}

#[allow(dead_code)]
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
        runtime()
            .spawn_blocking(move || stream.writer().write_all(message_bytes.as_slice()))
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
        let message: Option<(V1Header, MessageToServer)> = runtime()
            .spawn_blocking(move || blocking_read_v1_message(connection.reader()))
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

        runtime().spawn(async move {
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
