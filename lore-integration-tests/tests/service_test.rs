// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Integration tests for the service C API.
//!
//! These call the exported entry points the way an embedder does, rather than
//! driving the `lore` CLI as the Python suite does. That difference is the
//! point: an embedded Lore runs inside a host application, which is not a thing
//! to relaunch as the service. A service is only ever started from an
//! executable someone named — the `executable` field, or the configured
//! default — and the code returned when there is none is what such a caller
//! branches on.
//!
//! `lore-integration-tests` points the service socket at one of its own (see
//! the constructor in `integration.rs`), so nothing here reaches a service the
//! developer is running.

#[cfg(test)]
mod service_api_tests {
    use std::sync::Mutex;

    use lore::interface::LoreEvent;
    use lore::interface::LoreEventCallbackConfig;
    use lore::interface::LoreGlobalArgs;
    use lore::interface::LoreServiceSetExecutableArgs;
    use lore::interface::LoreServiceStartArgs;
    use lore::interface::LoreServiceStopArgs;
    use lore::interface::LoreString;
    use lore::interface::lore_service_set_executable;
    use lore::interface::lore_service_start;
    use lore::interface::lore_service_stop;

    /// `LoreError::ServiceUnavailable`, the code an embedder branches on to
    /// decide it must start a service of its own. Written out rather than
    /// imported so that a change to the value has to be made here too.
    const SERVICE_UNAVAILABLE: i32 = 50;

    #[derive(Default)]
    struct Collected(Mutex<Vec<String>>);

    /// Records what each event reports about a failure.
    ///
    /// A local call reports through the detail on its `Complete` event; a call
    /// routed to the service also emits an `Error` event. Both are collected,
    /// so a test reads the same text either way.
    ///
    /// # Safety
    ///
    /// `user_context` must be the address of a `Collected` that outlives every
    /// event this call produces.
    unsafe extern "C" fn collect_errors(event: &LoreEvent, user_context: u64) {
        let collected = unsafe { &*(usize::try_from(user_context).unwrap() as *const Collected) };
        let message = match event {
            LoreEvent::Error(data) => String::from_utf8_lossy(data.error_inner.as_bytes()),
            LoreEvent::Complete(data) => String::from_utf8_lossy(data.error.message.as_bytes()),
            _ => return,
        };
        if !message.is_empty() {
            collected
                .0
                .lock()
                .expect("collector lock")
                .push(message.into_owned());
        }
    }

    /// Runs one call with its error messages captured, returning the status the
    /// caller sees and everything it reported.
    ///
    /// The collector is leaked rather than borrowed from the stack: the event
    /// channel is what decides when the last event is delivered, and a test is
    /// the wrong place to assume that is before the call returns. A leak per
    /// call costs a test binary nothing.
    fn capturing(call: impl FnOnce(LoreEventCallbackConfig) -> i32) -> (i32, String) {
        let collected: &'static Collected = Box::leak(Box::new(Collected::default()));
        let config = LoreEventCallbackConfig {
            user_context: (std::ptr::from_ref(collected) as usize) as u64,
            func: Some(collect_errors),
        };

        let status = call(config);
        let messages = collected.0.lock().expect("collector lock").join("\n");
        (status, messages)
    }

    fn start(executable: &str) -> (i32, String) {
        capturing(|config| {
            let globals = LoreGlobalArgs::default();
            let args = LoreServiceStartArgs {
                executable: LoreString::from(executable),
            };
            lore_service_start(&globals, &args, config)
        })
    }

    fn set_executable(executable: &str) -> (i32, String) {
        capturing(|config| {
            let globals = LoreGlobalArgs::default();
            let args = LoreServiceSetExecutableArgs {
                executable: LoreString::from(executable),
            };
            lore_service_set_executable(&globals, &args, config)
        })
    }

    /// Serializes the tests that depend on `service_executable`. The setting is
    /// one file shared by every test in this process, so one test writing it
    /// would otherwise decide another's answer. Poisoning is ignored: a failing
    /// test leaves the setting cleared or not, and the next test writes what it
    /// needs before reading.
    static CONFIGURED_EXECUTABLE: Mutex<()> = Mutex::new(());

    /// An executable that exits at once instead of listening, for the case
    /// where the caller names something that is not a Lore binary.
    fn exits_immediately() -> &'static str {
        if cfg!(windows) {
            "cmd.exe /c exit"
        } else {
            "/usr/bin/true"
        }
    }

    /// The whole reason the field exists: nothing is inferred from the running
    /// executable, which for an embedder is the host application, so a start
    /// that names nothing and has nothing configured has nothing to launch.
    #[test]
    fn starting_with_nothing_named_has_no_executable_to_launch() {
        let _guard = CONFIGURED_EXECUTABLE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        set_executable("");
        let (status, messages) = start("");

        assert_eq!(
            status, SERVICE_UNAVAILABLE,
            "a service must only be started from a named executable: {messages}"
        );
        assert!(
            messages.contains("no executable to launch"),
            "the refusal should say what is missing, got: {messages}"
        );
    }

    /// Distinct from the refusal above, which is what proves the field is
    /// honoured rather than ignored: naming a path reaches the spawn, and fails
    /// there instead of at the current-executable rule.
    #[test]
    fn starting_from_a_missing_executable_fails_at_the_spawn() {
        let missing = std::env::temp_dir().join("lore-integration-tests-no-such-binary");
        let (status, messages) = start(&missing.to_string_lossy());

        assert_eq!(
            status, SERVICE_UNAVAILABLE,
            "a named executable that does not exist cannot start a service: {messages}"
        );
        assert!(
            messages.contains("spawning") && messages.contains("no-such-binary"),
            "the failure should name the executable it could not spawn, got: {messages}"
        );
        assert!(
            !messages.contains("no executable to launch"),
            "a named executable must be the one launched: {messages}"
        );
    }

    /// A real executable that is not Lore starts and exits without listening.
    /// The caller sees the same actionable code, and sees it at once rather
    /// than after the start timeout.
    #[test]
    fn starting_from_an_executable_that_never_listens_reports_it_exiting() {
        let started = std::time::Instant::now();
        let (status, messages) = start(exits_immediately());

        assert_eq!(
            status, SERVICE_UNAVAILABLE,
            "an executable that exits instead of listening cannot start a service: {messages}"
        );
        assert!(
            messages.contains("instead of listening"),
            "the failure should say the process exited, got: {messages}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "an exit should be reported without waiting out the start timeout"
        );
    }

    /// What `service_executable` is for: an embedder that configures it once at
    /// install time, so that the routed calls carrying no executable of their
    /// own can still start a service.
    ///
    /// A missing path is enough to show the precedence — reaching the spawn at
    /// all means the current-executable rule was not what answered — and it
    /// costs no real service process to prove.
    #[test]
    fn a_configured_executable_is_used_when_the_call_names_none() {
        let _guard = CONFIGURED_EXECUTABLE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let missing = std::env::temp_dir().join("lore-integration-tests-configured-binary");
        let (set_status, set_messages) = set_executable(&missing.to_string_lossy());
        assert_eq!(set_status, 0, "configuring the executable: {set_messages}");

        let (status, messages) = start("");
        set_executable("");

        assert_eq!(
            status, SERVICE_UNAVAILABLE,
            "a configured executable that does not exist cannot start a service: {messages}"
        );
        assert!(
            messages.contains("spawning") && messages.contains("configured-binary"),
            "the configured executable should be the one launched, got: {messages}"
        );
        assert!(
            !messages.contains("no executable to launch"),
            "a configured executable is one to launch: {messages}"
        );
    }

    /// Stopping is idempotent, so an embedder can call it on shutdown without
    /// tracking whether it ever started one.
    #[test]
    fn stopping_when_none_is_running_succeeds() {
        let (status, messages) = capturing(|config| {
            let globals = LoreGlobalArgs::default();
            let args = LoreServiceStopArgs {};
            lore_service_stop(&globals, &args, config)
        });

        assert_eq!(
            status, 0,
            "stopping a service that is not running is the outcome asked for: {messages}"
        );
    }
}
