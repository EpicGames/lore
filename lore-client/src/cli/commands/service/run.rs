// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore::error_set::prelude::*;
use lore::interface::LoreGlobalArgs;
use lore::interface::LoreLogLevel;
use lore::lore_spawn_blocking;
use lore::remote::connection::ConnectionId;
use lore::remote::connection::serve_connection;
use lore::remote::network::UdsListener;
use lore::remote::network::UdsStream;
use lore::remote::network::uds_supported;
use lore::remote::service_process::ServiceStopRequest;
use lore::remote::service_process::register_service_process;
use lore::remote::service_process::service_executable;
use lore::remote::service_process::wait_until_a_service_is_listening;
use lore::remote::service_socket_name;
use lore::service::initialization::initialize_service;
use lore::service::service_main::ServiceMainError;
use lore::service::state::ServiceStateImpl;

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

/// Reported by a service that exits because another one has claimed the socket.
const CLAIM_REFUSED_MESSAGE: &str = "Another Lore service has claimed the socket";

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
    service_state: Arc<ServiceStateImpl>,
) -> Result<(), ServiceMainError> {
    if !uds_supported() {
        return Err(ServiceMainError::internal("IPC not supported on this OS"));
    }

    // Initialize uptime tracking early so all errors can be captured.
    service_state.initialize();

    // Ahead of the socket, because a signal ends the process outright until a
    // handler is in place: registering after binding leaves a window in which a
    // service callers can already reach dies rather than stopping, leaving the
    // socket behind. A registration that fails leaves it stoppable over IPC
    // alone, which beats refusing to serve.
    let termination = match TerminationSignals::register() {
        Ok(signals) => Some(signals),
        Err(error) => {
            service_state.push_log(
                LoreLogLevel::Error,
                format!("Failed to listen for termination signals: {error}"),
            );
            None
        }
    };

    let detached = detached_working_directory();
    if let Err(error) = std::env::set_current_dir(&detached) {
        service_state.push_log(
            LoreLogLevel::Error,
            format!(
                "Failed to set working directory to {}: {error}",
                detached.display()
            ),
        );
    }

    // Claimed ahead of initializing, which mounts every instance: a process that would lose the
    // socket must find out before it mounts anything, since its mounts die with it.
    let Some(claim) = UdsListener::claim(service_socket_name())
        .forward::<ServiceMainError>("Failed to claim the service socket")?
    else {
        return Err(refuse_claimed_socket(termination).await);
    };

    // Brings up the mount manager and routes its events into the service state.
    initialize_service(globals, Arc::clone(&service_state))
        .await
        .forward::<ServiceMainError>("Failed initializing service")?;

    let listener: UdsListener = claim
        .listen()
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
    let accept_state = Arc::clone(&service_state);
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
                    serve_connection(
                        accept_state.clone(),
                        ConnectionId(new_connection_id),
                        stream,
                    );
                }
                Err(err) => {
                    if accept_shutting_down.load(Ordering::SeqCst) {
                        break;
                    }
                    accept_state
                        .push_log(LoreLogLevel::Error, format!("Failed when accepting: {err}"));
                }
            }
        }
    });

    wait_for_stop(&stop_request, termination).await;

    println!("Shutting down Lore service");
    shutting_down.store(true, Ordering::SeqCst);
    if let Err(error) = UdsStream::connect(service_socket_name()) {
        service_state.push_log(
            LoreLogLevel::Error,
            format!("Failed to wake the accept loop: {error}"),
        );
    }
    if tokio::time::timeout(SHUTDOWN_TIMEOUT, accept_task)
        .await
        .is_err()
    {
        service_state.push_log(
            LoreLogLevel::Error,
            "Timed out waiting for the accept loop to stop".to_string(),
        );
    }

    Ok(())
}

/// Waits for the service holding the claim to answer, then reports the refusal. The command that
/// started this process gives it a short grace once it exits, which a winner still mounting its
/// instances can outlast, so exiting at once would fail that command. A termination signal ends
/// the wait.
async fn refuse_claimed_socket(termination: Option<TerminationSignals>) -> ServiceMainError {
    let outcome = match unless_terminated(termination, wait_until_a_service_is_listening()).await {
        None => ", and stopped waiting for it on a signal",
        Some(Ok(true)) => "",
        Some(Ok(false)) => ", and it did not answer in time",
        Some(Err(error)) => {
            return ServiceMainError::internal_with_context(
                error,
                "waiting for the service holding the socket claim to answer",
            );
        }
    };
    ServiceMainError::internal(format!(
        "{CLAIM_REFUSED_MESSAGE} {}{outcome}",
        service_socket_name()
    ))
}

/// Awaits `future`, or ends the wait with `None` when `termination` receives a signal first.
async fn unless_terminated<T>(
    termination: Option<TerminationSignals>,
    future: impl Future<Output = T>,
) -> Option<T> {
    match termination {
        Some(mut termination) => tokio::select! {
            output = future => Some(output),
            () = termination.recv() => None,
        },
        None => Some(future.await),
    }
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
    let stopped_by_signal = unless_terminated(termination, stop_request.requested())
        .await
        .is_none();

    // A signal carries no reply, so nothing has to drain before shutdown.
    if stopped_by_signal {
        return;
    }

    // The caller that asked for the stop is waiting for its reply on a
    // connection this process owns, so the reply drains before the teardown.
    tokio::time::sleep(STOP_REPLY_DRAIN).await;
}
