// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use lore_error_set::prelude::*;
use lore_revision::lore_debug;

use crate::remote::network::UdsStream;
use crate::remote::network::uds_supported;

#[error_set]
pub enum ServiceProcessError {}

/// How long `ensure_running` waits for a freshly spawned service to bind its
/// socket before giving up.
const START_TIMEOUT: Duration = Duration::from_secs(10);
const START_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// How long `wait_until_stopped` gives the service to release its socket. The
/// service itself bounds its shutdown at five seconds, so this allows for that
/// plus the time to unwind.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Set by the service process itself, holding the flag its accept loop watches.
/// Its presence is also what tells a command handler that it is executing
/// inside the service rather than in a client.
static SHUTDOWN_FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// Called by the service process at startup to publish the flag that stops its
/// accept loop, so that a `ServiceStop` arriving over IPC can trip it.
///
/// Supports a single service per process: the `OnceLock` keeps the first flag,
/// so a second `service_main` in the same process would run an accept loop
/// watching a flag this never trips, and could not be stopped. The CLI runs
/// `service run` once per process, so this only constrains a future embedder or
/// in-process test that starts the service more than once.
pub fn register_shutdown_flag(flag: Arc<AtomicBool>) {
    let _ = SHUTDOWN_FLAG.set(flag);
}

pub fn running_as_service() -> bool {
    SHUTDOWN_FLAG.get().is_some()
}

/// Stops this process's own service loop. The accept loop is blocked in
/// `accept`, so it also needs a connection to wake it before it can observe the
/// flag; a failure to make that connection leaves the loop parked, so it is
/// returned rather than ignored.
///
/// Shutdown therefore depends on this wake-up connection succeeding. A caller
/// that polls afterwards (the client `service stop`, via `wait_until_stopped`)
/// self-corrects, because its probes are themselves connections that wake the
/// loop. The termination-signal path does not poll and discards this error to a
/// detached process's null stderr, so on the rare failure — connecting to one's
/// own listening socket essentially only fails under backlog exhaustion or a
/// removed socket file — exit is delayed until another connection arrives.
pub fn request_shutdown() -> Result<(), ServiceProcessError> {
    let Some(flag) = SHUTDOWN_FLAG.get() else {
        return Err(ServiceProcessError::internal(
            "this process is not running as a service",
        ));
    };
    lore_debug!("Stopping Lore service process");
    flag.store(true, Ordering::SeqCst);
    UdsStream::connect().forward::<ServiceProcessError>("waking the accept loop")?;
    Ok(())
}

pub fn is_running() -> bool {
    uds_supported() && UdsStream::connect().is_ok()
}

/// The executable to relaunch as the service.
///
/// Auto-start only fires for the Lore CLI. When Lore is embedded as a library
/// the current executable is the host application, and running it with
/// `service run` would launch something arbitrary, so that case is refused and
/// the caller is told to start the service itself.
fn service_executable() -> Result<std::path::PathBuf, ServiceProcessError> {
    let path = std::env::current_exe().internal("resolving the current executable")?;
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();
    if !name.eq_ignore_ascii_case("lore") {
        return Err(ServiceProcessError::internal(format!(
            "cannot start the Lore service automatically from {}; run `lore service start` instead",
            path.display()
        )));
    }
    Ok(path)
}

/// Spawns a detached `lore service run`. The child keeps running after this
/// process exits, so it is given no console and no inherited standard streams.
pub fn spawn() -> Result<(), ServiceProcessError> {
    let executable = service_executable()?;
    lore_debug!("Starting Lore service process: {}", executable.display());

    let mut command = Command::new(&executable);
    command
        .arg("service")
        .arg("run")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(target_family = "unix")]
    {
        use std::os::unix::process::CommandExt;
        // Safety: setsid is async-signal-safe and is the documented way to
        // detach the child from the caller's session and controlling terminal.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
    }

    command
        .spawn()
        .internal_with(|| format!("spawning {}", executable.display()))?;

    Ok(())
}

/// Makes sure a service process is listening, starting one if it is not.
/// Returns `true` if a new process was spawned.
pub async fn ensure_running() -> Result<bool, ServiceProcessError> {
    if !uds_supported() {
        return Err(ServiceProcessError::internal(
            "the Lore service is not supported on this OS",
        ));
    }
    if is_running() {
        return Ok(false);
    }

    spawn()?;

    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        if is_running() {
            lore_debug!("Lore service process is listening");
            return Ok(true);
        }
        tokio::time::sleep(START_POLL_INTERVAL).await;
    }

    Err(ServiceProcessError::internal(format!(
        "the Lore service did not start listening within {} seconds",
        START_TIMEOUT.as_secs()
    )))
}

/// Waits for a stopping service to stop answering on its socket. Returns
/// `false` if it is still listening when the timeout expires.
pub async fn wait_until_stopped() -> bool {
    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline {
        if !is_running() {
            return true;
        }
        tokio::time::sleep(START_POLL_INTERVAL).await;
    }
    !is_running()
}
