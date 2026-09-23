// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The service API as an embedder calls it, rather than as the client does.
//!
//! `scripts/test/test_service.py` drives the same behaviours through `lore
//! service ...`, so it covers the CLI's wrapper and not the crate entry points a
//! program linking `liblore` uses. These are those entry points.
//!
//! What is here is what runs without a service listening: the settings, the
//! decision they feed, a stop with nothing to stop, and the error a relayed call
//! fails with when no service can be reached. Starting one, and stopping one that
//! is running, need a bound socket and stay with the smoke suite.
//!
//! One process, so `LORE_GLOBAL_PATH`, the socket and the process-wide decision
//! are shared: every test is `#[serial]`, and each sets the settings it depends
//! on rather than inheriting them from whichever ran first.
#![allow(clippy::disallowed_methods)]

mod test_util;

mod tests {
    use std::sync::Arc;
    use std::sync::LazyLock;
    use std::sync::Mutex;
    use std::time::Duration;
    use std::time::Instant;

    use lore::interface::LoreEvent;
    use lore::interface::LoreEventCallback;
    use lore::interface::LoreString;
    use lore::service::LoreServiceSetExecutableArgs;
    use lore::service::LoreServiceSetUseAutomaticallyArgs;
    use lore::service::LoreServiceStartArgs;
    use lore::service::LoreServiceStopArgs;
    use lore_base::error::ServiceUnavailable;
    use lore_error_set::FfiError;
    use lore_revision::interface::LoreGlobalArgs;
    use rand::distr::Alphanumeric;
    use rand::distr::SampleString;
    use serial_test::serial;

    use super::test_util::TempDir;

    /// Names the executable for one call, which is what these tests set rather
    /// than writing the config the machine shares.
    const EXECUTABLE_VAR: &str = "LORE_SERVICE_EXECUTABLE";

    /// Bound on reporting a service that exited instead of listening. Above the
    /// two-second grace `connect_or_spawn_service` allows once the process it
    /// started has gone, and well under the ten-second start timeout, so a wait
    /// that ran to that timeout instead fails this.
    const REPORTED_WITHIN: Duration = Duration::from_secs(6);

    /// A socket of this process's own, so that a `stop` here cannot end a service
    /// a developer has running.
    ///
    /// `stop` connects to whatever is listening whether or not calls relay, so
    /// turning relaying off is no protection from it: on the default per-user
    /// socket this target would stop a developer's service and report success.
    /// `.cargo/config.toml` moves everything cargo runs off that socket already;
    /// this makes the name unique so that two runs of this target at once do not
    /// stop each other's.
    ///
    /// Nothing is left behind to clean up: no test here starts a service — the one
    /// that relays names an executable that does not exist — and a name no service
    /// ever bound has no socket to remove.
    static SOCKET_NAME: LazyLock<String> = LazyLock::new(|| {
        let name = format!(
            "lore_service-api-test-{}",
            Alphanumeric.sample_string(&mut rand::rng(), 12)
        );
        // Safety: runs once, under the `#[serial]` lock every test in this target
        // holds, before that test makes any call.
        unsafe { std::env::set_var(lore::remote::LORE_SERVICE_SOCKET_VAR, &name) };
        name
    });

    /// Names this process's own socket, returning it, having set it.
    ///
    /// Separate from reading it back so the setting happens first: forcing
    /// [`SOCKET_NAME`] is what sets the variable, and doing that inside an
    /// assertion left the first test to run comparing against the name the
    /// variable held before it was forced.
    fn use_a_socket_of_our_own() -> &'static str {
        SOCKET_NAME.as_str()
    }

    /// Points this process at a global config and a socket of its own, so the
    /// settings under test are the ones written here and not the machine's, and no
    /// call here can reach the machine's service.
    fn machine_settings(prefix: &str) -> TempDir {
        let ours = use_a_socket_of_our_own();
        let directory = TempDir::new(prefix);
        // Safety: `#[serial]` on every test in this target, and the runtime is
        // not reading the environment concurrently.
        unsafe {
            std::env::set_var("LORE_GLOBAL_PATH", directory.path());
            // The two per-call overrides, cleared so that only the config decides.
            // `.cargo/config.toml` sets the first for everything cargo runs.
            std::env::remove_var("LORE_USE_SERVICE");
            std::env::remove_var(EXECUTABLE_VAR);
        }

        // Asserted rather than assumed, and asserted before any call: a name that
        // did not take effect leaves every test here pointed at the machine's
        // service, where the one that stops a service would end it.
        assert_eq!(
            lore::remote::service_socket_name(),
            ours,
            "this target must act on a socket of its own"
        );
        directory
    }

    fn no_callback() -> lore_revision::interface::LoreEventCallbackConfig {
        lore_revision::interface::LoreEventCallbackConfig {
            user_context: 0,
            func: None,
        }
    }

    fn globals() -> LoreGlobalArgs {
        LoreGlobalArgs::default()
    }

    async fn set_executable(executable: &str) -> i32 {
        lore::service::set_executable(
            globals(),
            LoreServiceSetExecutableArgs {
                executable: LoreString::from(executable),
            },
            lore_revision::event::convert_event_callback(no_callback()),
        )
        .await
    }

    async fn set_use_automatically(enabled: bool) -> i32 {
        lore::service::set_use_automatically(
            globals(),
            LoreServiceSetUseAutomaticallyArgs {
                enabled: u8::from(enabled),
            },
            lore_revision::event::convert_event_callback(no_callback()),
        )
        .await
    }

    async fn stop() -> i32 {
        lore::service::stop(
            globals(),
            LoreServiceStopArgs {},
            lore_revision::event::convert_event_callback(no_callback()),
        )
        .await
    }

    async fn start(callback: LoreEventCallback) -> i32 {
        lore::service::start(globals(), LoreServiceStartArgs {}, callback).await
    }

    /// The code a caller branches on when a call did not run because no service
    /// could be reached or started.
    fn unavailable_code() -> i32 {
        ServiceUnavailable {
            reason: String::new(),
        }
        .ffi_code()
    }

    /// Collects the failure messages a call reports, so a test can assert on
    /// what a reader is told and not only on the code they branch on.
    ///
    /// A local call reports through the detail on its `Complete` event; a routed
    /// one also emits `Error`. Both are collected, so the text reads the same
    /// either way.
    fn capturing() -> (Arc<Mutex<Vec<String>>>, LoreEventCallback) {
        let collected: Arc<Mutex<Vec<String>>> = Arc::default();
        let recorder = Arc::clone(&collected);
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            let message = match event {
                LoreEvent::Error(data) => data.error_inner.to_string(),
                LoreEvent::Complete(data) => data.error.message.to_string(),
                _ => return,
            };
            if !message.is_empty() {
                recorder
                    .lock()
                    .expect("the collector lock is not poisoned")
                    .push(message);
            }
        }));
        (collected, callback)
    }

    /// An executable that exits at once rather than listening, which is what a
    /// path that is not a Lore binary does.
    ///
    /// Started as `<executable> service run`, so it has to be one program that
    /// exits when handed those two arguments. `where.exe` searches for them,
    /// finds nothing, and exits.
    fn exits_immediately() -> &'static str {
        if cfg!(windows) {
            "where.exe"
        } else {
            "/usr/bin/true"
        }
    }

    /// A stop asks for no service to be running. With none running that is
    /// already so, which is a success and not a failure to find one.
    #[test]
    #[serial]
    fn stopping_when_no_service_runs_succeeds() {
        let _settings = machine_settings("service-api-stop-none-");

        assert_eq!(
            lore::runtime().block_on(stop()),
            0,
            "a stop with no service running asks for a state that already holds"
        );
    }

    /// The `use_automatically` setting alone turns relaying on. An executable is
    /// only needed to start a service when none is running; a running service can
    /// be used without one configured.
    #[test]
    #[serial]
    fn use_automatically_alone_turns_relaying_on() {
        let _settings = machine_settings("service-api-use-automatically-");

        lore::runtime().block_on(async {
            assert_eq!(set_use_automatically(true).await, 0);
            assert!(
                lore::will_use_service(),
                "the setting alone must relay: a running service can be used \
                 without an executable configured"
            );
        });
    }

    /// The executable alone asks for nothing. Naming one says which build would
    /// serve the machine, not that anything should be relayed to it.
    #[test]
    #[serial]
    fn the_executable_alone_does_not_turn_relaying_on() {
        let _settings = machine_settings("service-api-executable-alone-");

        lore::runtime().block_on(async {
            assert_eq!(set_executable("/opt/lore/1.9/bin/lore").await, 0);
            assert!(!lore::will_use_service());
        });
    }

    /// Clearing the executable does not turn relaying off. An executable is only
    /// needed to start a service; relaying stays on so that a running service can
    /// still be used.
    #[test]
    #[serial]
    fn clearing_the_executable_does_not_turn_relaying_off() {
        let _settings = machine_settings("service-api-clear-executable-");

        lore::runtime().block_on(async {
            assert_eq!(set_use_automatically(true).await, 0);
            assert_eq!(set_executable("/opt/lore/1.9/bin/lore").await, 0);
            assert!(lore::will_use_service(), "relaying is on");

            assert_eq!(set_executable("").await, 0);
            assert!(
                lore::will_use_service(),
                "clearing the executable must not turn relaying off: a running \
                 service can still be used without an executable configured"
            );
        });
    }

    /// Turning the setting off turns relaying off within the process, for the
    /// same reason.
    #[test]
    #[serial]
    fn turning_the_setting_off_turns_relaying_off_within_the_process() {
        let _settings = machine_settings("service-api-setting-off-");

        lore::runtime().block_on(async {
            assert_eq!(set_use_automatically(true).await, 0);
            assert_eq!(set_executable("/opt/lore/1.9/bin/lore").await, 0);
            assert!(lore::will_use_service());

            assert_eq!(set_use_automatically(false).await, 0);
            assert!(!lore::will_use_service());
        });
    }

    /// A relayed call that reaches no service fails with the code that says so,
    /// distinctly from the failures of the work it would have done.
    ///
    /// An embedder deciding what to do about an unreachable service — report it,
    /// tell someone to start one — needs to tell that apart from the call having
    /// run and failed. The executable named here does not exist, so nothing can
    /// be started and nothing can be reached.
    #[test]
    #[serial]
    fn a_relayed_call_with_no_reachable_service_reports_that_distinctly() {
        let settings = machine_settings("service-api-unavailable-");
        let missing = settings.path().join("no-such-lore");

        let status = lore::runtime().block_on(async {
            assert_eq!(set_use_automatically(true).await, 0);
            assert_eq!(
                set_executable(&missing.to_string_lossy()).await,
                0,
                "naming an executable that does not exist is a valid setting; \
                 what it names is discovered when one is started"
            );
            assert!(lore::will_use_service());

            // Any verb the dispatch routes will do: what is asserted is where it
            // was sent, and it never arrives anywhere to be carried out.
            lore::revision::info(
                globals(),
                lore::revision::LoreRevisionInfoArgs::default(),
                lore_revision::event::convert_event_callback(no_callback()),
            )
            .await
        });

        assert_eq!(
            status,
            unavailable_code(),
            "an unreachable service must report as one, not as the call having \
             run and failed"
        );
    }

    /// A service that exits instead of listening is reported once it has, rather
    /// than after the whole start timeout.
    ///
    /// The shortened wait is otherwise uncovered: a caller that named a path
    /// which is not a Lore binary would sit out the full timeout to be told only
    /// that nothing started listening.
    #[test]
    #[serial]
    fn a_service_that_exits_instead_of_listening_is_reported_promptly() {
        let _settings = machine_settings("service-api-exits-");
        // Safety: `#[serial]` on every test in this target, and the runtime is
        // not reading the environment concurrently.
        unsafe { std::env::set_var(EXECUTABLE_VAR, exits_immediately()) };

        let (messages, callback) = capturing();
        let started = Instant::now();
        let status = lore::runtime().block_on(start(callback));
        let elapsed = started.elapsed();
        let reported = messages
            .lock()
            .expect("the collector lock is not poisoned")
            .join("\n");

        assert_eq!(
            status,
            unavailable_code(),
            "an executable that exits instead of listening starts no service: {reported}"
        );
        assert!(
            elapsed < REPORTED_WITHIN,
            "an exit must be reported without waiting out the start timeout, took {elapsed:.1?}"
        );
        assert!(
            reported.contains("exited"),
            "the failure must separate a service that exited from one that never \
             answered: {reported}"
        );
    }

    /// The failure names the executable it could not start, which is what says
    /// whether the path a caller named is the problem.
    #[test]
    #[serial]
    fn a_service_that_cannot_be_started_names_the_executable() {
        let settings = machine_settings("service-api-missing-");
        let missing = settings.path().join("no-such-lore");
        // Safety: as above.
        unsafe { std::env::set_var(EXECUTABLE_VAR, &missing) };

        let (messages, callback) = capturing();
        let status = lore::runtime().block_on(start(callback));
        let reported = messages
            .lock()
            .expect("the collector lock is not poisoned")
            .join("\n");

        assert_eq!(
            status,
            unavailable_code(),
            "an executable that does not exist starts no service: {reported}"
        );
        assert!(
            reported.contains("no-such-lore"),
            "the failure must name the executable it could not start: {reported}"
        );
    }
}
