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
use lore_revision::global::GlobalConfig;
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

/// The configured default executable, from `service_executable` in the global
/// config. Read only when a service actually has to be started, which is rare
/// enough not to want the caching the routing setting needs.
///
/// A config that cannot be read leaves the choice to the caller's request and
/// then to the running executable, the same as one that names nothing: failing
/// to start a service over an unreadable config would be a worse answer than
/// starting the one the caller could still describe.
async fn configured_executable() -> Option<String> {
    GlobalConfig::load()
        .await
        .ok()
        .and_then(|config| config.service_executable().map(str::to_owned))
}

/// The executable to launch as the service: the one this call names, or failing
/// that the one the global config names.
///
/// The executable is named, never inferred. The running executable is not a
/// candidate: with Lore embedded as a library it is the host application, and
/// relaunching that with `service run` would start something arbitrary. Even
/// for the CLI, inferring it would make the answer depend on what the binary
/// happens to be called, so a service is started only from an executable
/// someone named.
///
/// A named executable is taken at its word: an absolute path, a path relative
/// to the working directory, or a bare name looked up on `PATH`, the same way a
/// shell resolves a command. It was named deliberately, so no check is made
/// that it is a Lore binary; one that is not simply fails to start listening,
/// which [`ensure_running`] reports. That holds for the configured one too — it
/// is the same choice, made once for the machine rather than per call.
fn service_executable(
    requested: Option<&str>,
    configured: Option<&str>,
) -> Result<std::path::PathBuf, ServiceProcessError> {
    requested
        .into_iter()
        .chain(configured)
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .ok_or_else(|| {
            ServiceProcessError::internal(
                "cannot start the Lore service: no executable to launch. Name one for this \
                 start with `lore service start --executable <path>`, or for every start \
                 with `lore service set-executable <path>`",
            )
        })
}

/// Spawns a detached `lore service run`, returning the child so that a caller
/// waiting for it to listen can notice it exiting instead. The child keeps
/// running after this process exits, so it is given no console and no inherited
/// standard streams.
pub fn spawn(executable: &std::path::Path) -> Result<std::process::Child, ServiceProcessError> {
    lore_debug!("Starting Lore service process: {}", executable.display());

    let mut command = Command::new(executable);
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

    let child = command
        .spawn()
        .internal_with(|| format!("spawning {}", executable.display()))?;

    Ok(child)
}

/// Makes sure a service process is listening, starting one if it is not.
/// Returns `true` if a new process was spawned. `requested` names the executable
/// to launch; see [`service_executable`] for how it is resolved and what
/// happens without one.
pub async fn ensure_running(requested: Option<&str>) -> Result<bool, ServiceProcessError> {
    if !uds_supported() {
        return Err(ServiceProcessError::internal(
            "the Lore service is not supported on this OS",
        ));
    }
    if is_running() {
        return Ok(false);
    }

    let executable = service_executable(requested, configured_executable().await.as_deref())?;
    let mut child = spawn(&executable)?;

    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        if is_running() {
            lore_debug!("Lore service process is listening");
            return Ok(true);
        }
        // An executable that is not a Lore binary, or one that fails on
        // startup, exits instead of listening. Reporting that beats waiting out
        // the timeout to say only that nothing started listening.
        if let Ok(Some(status)) = child.try_wait() {
            // Unless a service started elsewhere took the socket first, which
            // makes ours exit for the very reason the caller wanted: one is
            // listening. Starting a service that is already running is success.
            if is_running() {
                return Ok(false);
            }
            return Err(ServiceProcessError::internal(format!(
                "the Lore service process exited with {status} instead of listening"
            )));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A path to stand in for a Lore binary an embedder ships. Never launched:
    /// these tests resolve the name, they do not spawn it.
    fn shipped_executable() -> &'static str {
        if cfg!(windows) {
            "C:\\ProgramData\\game\\lore.exe"
        } else {
            "/opt/game/bin/lore"
        }
    }

    /// Nothing is inferred from the running executable, so with neither a
    /// requested nor a configured executable there is nothing to launch. The
    /// test binary is the case that matters: an embedder's host application,
    /// which must never be relaunched as the service.
    #[test]
    fn with_nothing_named_there_is_no_executable_to_launch() {
        let error = service_executable(None, None)
            .expect_err("a service must only be started from a named executable");
        assert!(
            error.to_string().contains("no executable to launch"),
            "the refusal should say what is missing, got: {error}"
        );
        assert!(
            error.to_string().contains("set-executable"),
            "the refusal should point at the way out of it, got: {error}"
        );
    }

    #[test]
    fn a_requested_executable_is_used_verbatim() {
        assert_eq!(
            service_executable(Some(shipped_executable()), None)
                .expect("a named executable is taken at its word"),
            std::path::PathBuf::from(shipped_executable())
        );
    }

    /// The configured executable is what the calls carrying none of their own —
    /// every routed command that finds no service — start.
    #[test]
    fn a_configured_executable_is_used_when_the_call_names_none() {
        assert_eq!(
            service_executable(None, Some(shipped_executable()))
                .expect("the configured executable stands in for an unnamed one"),
            std::path::PathBuf::from(shipped_executable())
        );
    }

    /// The call's own executable is the more specific of the two, so it wins.
    #[test]
    fn a_requested_executable_beats_the_configured_one() {
        assert_eq!(
            service_executable(Some(shipped_executable()), Some("configured-lore"))
                .expect("a named executable wins"),
            std::path::PathBuf::from(shipped_executable())
        );
    }

    /// A bare name is left for the OS to resolve on `PATH`, the way a shell
    /// would, rather than being rejected for not being a path.
    #[test]
    fn a_bare_name_is_passed_through() {
        assert_eq!(
            service_executable(Some("lore"), None).expect("a bare name resolves at spawn time"),
            std::path::PathBuf::from("lore")
        );
    }

    /// Empty and whitespace are how "not given" arrives over the FFI, where the
    /// field is a string rather than an option. A blank request falls through to
    /// the configured value rather than being launched as a nameless command.
    #[test]
    fn a_blank_request_falls_through_to_what_is_configured() {
        for blank in ["", "   "] {
            assert!(
                service_executable(Some(blank), None).is_err(),
                "{blank:?} should be treated as no executable at all"
            );
            assert_eq!(
                service_executable(Some(blank), Some(shipped_executable()))
                    .expect("a blank request leaves the configured executable"),
                std::path::PathBuf::from(shipped_executable()),
                "{blank:?} should not shadow the configured executable"
            );
        }
    }

    /// A blank configured value is the same as none: a stored empty string must
    /// not become a nameless command.
    #[test]
    fn a_blank_configured_value_is_the_same_as_none() {
        for blank in ["", "   "] {
            assert!(
                service_executable(None, Some(blank)).is_err(),
                "a {blank:?} configured executable names nothing to launch"
            );
        }
    }
}
