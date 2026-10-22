use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_base::error::InvalidPath;
use lore_base::log::LoreLogLevel;
use lore_base::lore_spawn_blocking;
use lore_error_set::ForwardStrict;
use lore_error_set::error_set;
use lore_revision::event::EventError;
use lore_revision::service_state::ServiceStateImpl;

use crate::interface::LoreGlobalArgs;
use crate::remote::connection::ConnectionId;
use crate::remote::connection::serve_connection;
use crate::remote::network::UdsConnectionError;
use crate::remote::network::UdsListener;
use crate::remote::network::UdsStream;
use crate::remote::network::uds_supported;
use crate::remote::service_process::ServiceStartGuard;
use crate::remote::service_process::ServiceStopRequest;
use crate::remote::service_process::begin_service_start;
use crate::remote::service_process::register_service_process;
use crate::remote::service_process::service_executable;
use crate::remote::service_process::wait_until_a_service_is_listening;
use crate::remote::service_socket_name;
use crate::service::initialization::initialize_service;

#[error_set]
pub enum ServiceMainError {
    InvalidPath,
}

impl EventError for ServiceMainError {}

/// Bounds how long shutdown waits for the accept loop to unwind, so that a
/// wake-up connection that never lands cannot keep the process alive. Anything
/// left behind is a stale socket, which the next start detects and removes.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the service keeps running after a stop request arrives, so that the
/// reply reaches a caller waiting for it over IPC before the service tears
/// itself down. The reply is a few small writes to a socket the caller is
/// already reading, so this is margin rather than a measured cost.
const STOP_REPLY_DRAIN: Duration = Duration::from_millis(500);

/// Printed once the socket is bound, so that whoever started the service can
/// tell when it began accepting connections.
const LISTENING_MESSAGE: &str = "Lore service listening";

/// Reported by a service that exits because another one has claimed the socket.
const CLAIM_REFUSED_MESSAGE: &str = "Another Lore service has claimed the socket";

/// Runs the service in this process until a stop is requested, either over IPC
/// by `lore service stop` or in this process by `quit`.
///
/// The working directory of the process is left as it is: it is not this
/// function's to change when embedded, so the process that serves alone parks it
/// itself before calling this.
///
/// The process owns one `service_state`, handed in so that nothing below reaches
/// for the global itself.
pub async fn service_main(
    globals: LoreGlobalArgs,
    service_state: Arc<ServiceStateImpl>,
) -> Result<(), ServiceMainError> {
    if !uds_supported() {
        return Err(ServiceMainError::internal("IPC not supported on this OS"));
    }

    // Initialize uptime tracking early so all errors can be captured.
    service_state.initialize();

    // From here a stop asked for in this process is kept, so that one arriving
    // before the service is serving still ends it.
    let Some(start) = begin_service_start() else {
        return Err(ServiceMainError::internal(
            "A Lore service is already running in this process",
        ));
    };

    // Claimed ahead of initializing, which mounts every instance: a process that would lose the
    // socket must find out before it mounts anything, since its mounts die with it.
    let Some(claim) = UdsListener::claim(service_socket_name())
        .forward::<ServiceMainError>("Failed to claim the service socket")?
    else {
        return Err(refuse_claimed_socket(&start).await);
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
    register_service_process(&start);
    let stop_request = start.stop_request();

    // Declared after `start`, so that it drops first: the accept loop is told to end
    // before the process stops being one that runs a service, including when this
    // future is dropped before it finishes.
    let mut accept_shutdown = AcceptShutdown::new();
    let accept_shutting_down = accept_shutdown.flag();

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

    wait_for_stop(&stop_request).await;

    println!("Shutting down Lore service");
    if let Err(error) = accept_shutdown.shut_down() {
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

/// Ends the accept loop, which owns the listener on a blocking thread that a
/// dropped future cannot cancel. Ending it is a flag the loop checks and a
/// connection that wakes it from `accept`, done on drop as well, so that a service
/// future dropped mid-run does not leave an unreachable service holding the socket.
struct AcceptShutdown {
    shutting_down: Arc<AtomicBool>,
    done: bool,
}

impl AcceptShutdown {
    fn new() -> Self {
        Self {
            shutting_down: Arc::new(AtomicBool::new(false)),
            done: false,
        }
    }

    fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shutting_down)
    }

    /// Tells the loop to end and wakes it. Reports a failure to wake it.
    fn shut_down(&mut self) -> Result<(), UdsConnectionError> {
        self.done = true;
        self.shutting_down.store(true, Ordering::SeqCst);
        UdsStream::connect(service_socket_name()).map(drop)
    }
}

impl Drop for AcceptShutdown {
    fn drop(&mut self) {
        if !self.done {
            // Nothing to report to: the future is gone. A failed wake leaves a stale
            // socket, which the next start detects and removes.
            let _ = self.shut_down();
        }
    }
}

/// Waits for the service holding the claim to answer, then reports the refusal. The command that
/// started this process gives it a short grace once it exits, which a winner still mounting its
/// instances can outlast, so exiting at once would fail that command. A quit ends the wait.
async fn refuse_claimed_socket(start: &ServiceStartGuard) -> ServiceMainError {
    let stop_request = start.stop_request();
    let outcome = match tokio::select! {
        outcome = wait_until_a_service_is_listening() => Some(outcome),
        () = stop_request.requested() => None,
    } {
        None => ", and stopped waiting for it on a quit",
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

/// Waits for a stop, asked for over IPC by `lore service stop` or in this
/// process by `quit`.
async fn wait_for_stop(stop_request: &ServiceStopRequest) {
    stop_request.requested().await;

    // A caller that asked over IPC is waiting for its reply on a connection this
    // process owns, so the reply drains before the teardown.
    tokio::time::sleep(STOP_REPLY_DRAIN).await;
}
