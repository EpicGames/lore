// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore::error_set::prelude::*;
use lore::interface::LoreEvent;
use lore::interface::LoreGlobalArgs;
use lore::lore_spawn;
use lore::lore_spawn_blocking;
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
use lore::remote::service_process::ServiceStopRequest;
use lore::remote::service_process::register_service_process;
use lore::remote::service_process::service_executable;
use lore::remote::service_socket_name;
use lore::service::initialization::initialize_service;
use lore::service::service_main::ServiceMainError;
use tokio::sync::mpsc;

use crate::eprintln;
use crate::println;
use crate::util::TerminationSignals;

/// Bounds how long shutdown waits for the accept loop to unwind, so that a
/// wake-up connection that never lands cannot keep the process alive. Anything
/// left behind is a stale socket, which the next start detects and removes.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the service keeps running after a stop request arrives over IPC, so
/// that the reply reaches the caller waiting for it before the process tears
/// itself down. The reply is a few small writes to a socket the caller is
/// already reading, so this is margin rather than a measured cost. A signal
/// carries no reply and waits for none of it.
const STOP_REPLY_DRAIN: Duration = Duration::from_millis(500);

/// Printed once the socket is bound, so that whoever started the service can
/// tell when it began accepting connections.
const LISTENING_MESSAGE: &str = "Lore service listening";

/// Where the service parks its working directory. It inherits one from whoever
/// started it, which is unrelated to the directories its callers run in, and
/// holding that directory would also keep the filesystem under it busy. Callers
/// send the directory their relative paths belong to, so the service never
/// needs one of its own.
fn detached_working_directory() -> std::path::PathBuf {
    #[cfg(target_family = "unix")]
    {
        std::path::PathBuf::from("/")
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("SystemRoot").map_or_else(
            || std::path::PathBuf::from("C:\\"),
            std::path::PathBuf::from,
        )
    }
}

pub async fn service_main(
    globals: LoreGlobalArgs,
    listening_signal: Option<tokio::sync::oneshot::Sender<()>>,
) -> Result<(), ServiceMainError> {
    if !uds_supported() {
        return Err(ServiceMainError::internal("IPC not supported on this OS"));
    }

    // Ahead of the socket, because a signal ends the process outright until a
    // handler is in place: registering after binding leaves a window in which a
    // service callers can already reach dies rather than stopping, leaving the
    // socket behind. A registration that fails leaves it stoppable over IPC
    // alone, which beats refusing to serve.
    let termination = match TerminationSignals::register() {
        Ok(signals) => Some(signals),
        Err(error) => {
            eprintln!("Failed to listen for termination signals: {error}");
            None
        }
    };

    let detached = detached_working_directory();
    if let Err(error) = std::env::set_current_dir(&detached) {
        eprintln!(
            "Failed to set working directory to {}: {error}",
            detached.display()
        );
    }

    initialize_service(globals)
        .await
        .forward::<ServiceMainError>("Failed initializing service")?;

    let listener: UdsListener = UdsListener::new(service_socket_name())
        .forward::<ServiceMainError>("Failed to start listener socket")?;
    println!("{LISTENING_MESSAGE}");
    report_build_that_is_not_the_configured_one().await;

    // Recorded once the socket is bound, so that a process which lost it never
    // claims to be the service for the moments before it exits.
    let stop_request = register_service_process();

    if let Some(listening_signal) = listening_signal {
        listening_signal
            .send(())
            .map_err(|_err| ServiceMainError::internal("Couldn't signal listening"))?;
    }

    let shutting_down = Arc::new(AtomicBool::new(false));
    let accept_shutting_down = Arc::clone(&shutting_down);

    // Parks a blocking thread for the service's lifetime; pinned to core because
    // it would occupy net's single blocking thread outright.
    let accept_task = lore_spawn_blocking!(move || {
        let mut connection_id = 0;
        loop {
            match listener.accept() {
                Ok(stream) => {
                    if accept_shutting_down.load(Ordering::SeqCst) {
                        break;
                    }
                    let new_connection_id = connection_id;
                    connection_id += 1;
                    lore_spawn!(async move {
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

    wait_for_stop(&stop_request, termination).await;

    println!("Shutting down Lore service");
    shutting_down.store(true, Ordering::SeqCst);
    if let Err(error) = UdsStream::connect(service_socket_name()) {
        eprintln!("Failed to wake the accept loop: {error}");
    }
    if tokio::time::timeout(SHUTDOWN_TIMEOUT, accept_task)
        .await
        .is_err()
    {
        eprintln!("Timed out waiting for the accept loop to stop");
    }

    Ok(())
}

/// Reports serving from a build other than the one that would be started
/// automatically, which is the one the global config names when it names any.
///
/// Running a service from a chosen build is allowed: whoever ran this command
/// picked it. What is not wanted is silence about it afterwards, when a service
/// started long ago from a stale path is serving a machine and nothing says so.
async fn report_build_that_is_not_the_configured_one() {
    let Ok(configured) = service_executable().await else {
        return;
    };
    let Ok(running) = std::env::current_exe() else {
        return;
    };

    // Compared through the filesystem where possible, so that a link or a
    // relative path to the same build does not read as a different one.
    let resolve =
        |path: &std::path::Path| std::fs::canonicalize(path).unwrap_or(path.to_path_buf());
    if resolve(&configured) == resolve(&running) {
        return;
    }

    println!(
        "Serving from {}, which is not the configured Lore service ({})",
        running.display(),
        configured.display()
    );
}

/// Waits for whichever comes first: a termination signal, or a stop asked for
/// over IPC by `lore service stop`.
///
/// `termination` is registered before the socket binds, so the handlers are in
/// place for the whole time this service is reachable. Without them there is no
/// graceful path from a signal, and the IPC request becomes the only way to stop
/// serving rather than a reason to stop.
async fn wait_for_stop(stop_request: &ServiceStopRequest, termination: Option<TerminationSignals>) {
    let stopped_by_signal = if let Some(mut termination) = termination {
        tokio::select! {
            () = termination.recv() => true,
            () = stop_request.requested() => false,
        }
    } else {
        stop_request.requested().await;
        false
    };

    // A signal carries no reply, so nothing has to drain before shutdown.
    if stopped_by_signal {
        return;
    }

    // The caller that asked for the stop is waiting for its reply on a
    // connection this process owns, so the reply drains before the teardown.
    tokio::time::sleep(STOP_REPLY_DRAIN).await;
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
        lore_spawn_blocking!(move || stream.writer().write_all(message_bytes.as_slice()))
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
            lore_spawn_blocking!(move || blocking_read_v1_message(connection.reader()))
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

        lore_spawn!(async move {
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
