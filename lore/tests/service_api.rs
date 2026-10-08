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
    use std::sync::mpsc;
    use std::time::Duration;
    use std::time::Instant;

    use lore::interface::LoreArray;
    use lore::interface::LoreEvent;
    use lore::interface::LoreEventCallback;
    use lore::interface::LoreString;
    use lore::revision::LoreRevisionCherryPickArgs;
    use lore::service::LoreServiceSetExecutableArgs;
    use lore::service::LoreServiceSetUseAutomaticallyArgs;
    use lore::service::LoreServiceStartArgs;
    use lore::service::LoreServiceStatusArgs;
    use lore::service::LoreServiceStopArgs;
    use lore::shared_store::LoreSharedStoreListArgs;
    use lore_base::error::ServiceUnavailable;
    use lore_error_set::FfiError;
    use lore_revision::interface::LoreEventCallbackConfig;
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

    async fn status(callback: LoreEventCallback) -> i32 {
        lore::service::status(globals(), LoreServiceStatusArgs {}, callback).await
    }

    /// The code a caller branches on when a call did not run because no service
    /// could be reached or started.
    fn unavailable_code() -> i32 {
        ServiceUnavailable {
            reason: String::new(),
        }
        .ffi_code()
    }

    /// Collects the failure message a call reports on its `Complete` event, so a
    /// test can assert on what a reader is told and not only on the code they
    /// branch on.
    fn capturing() -> (Arc<Mutex<Vec<String>>>, LoreEventCallback) {
        let collected: Arc<Mutex<Vec<String>>> = Arc::default();
        let recorder = Arc::clone(&collected);
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::Complete(data) = event
                && !data.error.message.is_empty()
            {
                recorder
                    .lock()
                    .expect("the collector lock is not poisoned")
                    .push(data.error.message.to_string());
            }
        }));
        (collected, callback)
    }

    /// An event a C callback received, reduced to what a caller reads from it.
    #[derive(Debug, PartialEq)]
    enum Delivered {
        Error,
        Complete {
            status: i32,
            error_code: i32,
            has_message: bool,
            has_trace: bool,
        },
        End,
    }

    /// Where [`record`] sends what it receives. Set by [`delivered`], which the
    /// `#[serial]` lock every test here holds keeps to one caller at a time.
    static RECORDED: Mutex<Option<mpsc::Sender<Delivered>>> = Mutex::new(None);

    unsafe extern "C" fn record(event: &LoreEvent, _user_context: u64) {
        let delivered = match event {
            LoreEvent::Error(_) => Delivered::Error,
            LoreEvent::Complete(data) => Delivered::Complete {
                status: data.status,
                error_code: data.error.error_code,
                has_message: !data.error.message.is_empty(),
                has_trace: !data.error.trace_locations.as_slice().is_empty(),
            },
            LoreEvent::End(_) => Delivered::End,
            _ => return,
        };
        if let Some(sender) = RECORDED
            .lock()
            .expect("the sender lock is not poisoned")
            .as_ref()
        {
            let _ = sender.send(delivered);
        }
    }

    /// Runs `call` with a C callback and returns what the callback received, up
    /// to and including `End`.
    fn delivered(call: impl FnOnce(LoreEventCallbackConfig)) -> Vec<Delivered> {
        let (sender, receiver) = mpsc::channel();
        *RECORDED.lock().expect("the sender lock is not poisoned") = Some(sender);
        call(LoreEventCallbackConfig {
            user_context: 0,
            func: Some(record),
        });

        let mut received = Vec::new();
        while received.last() != Some(&Delivered::End) {
            received.push(
                receiver
                    .recv_timeout(Duration::from_secs(30))
                    .expect("every call ends with an End event"),
            );
        }
        received
    }

    /// What a call that failed with `code` delivers: one `Complete` carrying the
    /// code and the detail, then `End`.
    fn failed_with(code: i32) -> [Delivered; 2] {
        [
            Delivered::Complete {
                status: code,
                error_code: code,
                has_message: true,
                has_trace: true,
            },
            Delivered::End,
        ]
    }

    /// Turns relaying on and names an executable that does not exist, so a
    /// relayed call reaches no service and none is started.
    fn relaying_to_no_service(prefix: &str) -> TempDir {
        let settings = machine_settings(prefix);
        let missing = settings.path().join("no-such-lore");
        lore::runtime().block_on(async {
            assert_eq!(set_use_automatically(true).await, 0);
            assert_eq!(set_executable(&missing.to_string_lossy()).await, 0);
        });
        settings
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

    /// `lore_link_list_staged` decides at the entry whether to relay, so with
    /// relaying on it goes to the service, which holds the repository, rather
    /// than opening the repository here.
    ///
    /// No service can be reached, and the directory named holds no repository, so
    /// a call that ran here would fail differently.
    #[test]
    #[serial]
    fn the_link_list_staged_entry_point_relays_to_the_service() {
        let settings = machine_settings("service-api-link-list-staged-");
        let missing = settings.path().join("no-such-lore");
        lore::runtime().block_on(async {
            assert_eq!(set_use_automatically(true).await, 0);
            assert_eq!(set_executable(&missing.to_string_lossy()).await, 0);
        });

        let globals = LoreGlobalArgs {
            repository_path: settings.path().display().to_string().into(),
            ..LoreGlobalArgs::default()
        };

        assert_eq!(
            lore::interface::lore_link_list_staged(
                &globals,
                &lore::link::LoreLinkListStagedArgs {},
                no_callback()
            ),
            unavailable_code(),
            "the entry point must relay the call rather than run it here"
        );
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

    /// `lore_shared_store_list` decides at the entry whether to relay, so with
    /// relaying on it goes to the service rather than reading the registry here.
    ///
    /// No service can be reached, and the registry here is empty, so a call that
    /// ran here would succeed.
    #[test]
    #[serial]
    fn the_shared_store_list_entry_point_relays_to_the_service() {
        let settings = machine_settings("service-api-shared-store-list-");
        let missing = settings.path().join("no-such-lore");
        lore::runtime().block_on(async {
            assert_eq!(set_use_automatically(true).await, 0);
            assert_eq!(set_executable(&missing.to_string_lossy()).await, 0);
        });

        assert_eq!(
            lore::interface::lore_shared_store_list(
                &globals(),
                &LoreSharedStoreListArgs::default(),
                no_callback()
            ),
            unavailable_code(),
            "the entry point must relay the call rather than run it here"
        );
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

    /// `lore_revision_cherry_pick` decides at the entry whether to relay, so with
    /// relaying on it goes to the service rather than cherry-picking here.
    ///
    /// No service can be reached, and the directory named holds no repository, so
    /// a call that ran here would fail differently.
    #[test]
    #[serial]
    fn the_cherry_pick_entry_point_relays_to_the_service() {
        let settings = machine_settings("service-api-cherry-pick-");
        let missing = settings.path().join("no-such-lore");
        lore::runtime().block_on(async {
            assert_eq!(set_use_automatically(true).await, 0);
            assert_eq!(set_executable(&missing.to_string_lossy()).await, 0);
        });

        let globals = LoreGlobalArgs {
            repository_path: settings.path().display().to_string().into(),
            offline: 1,
            ..LoreGlobalArgs::default()
        };
        let args = LoreRevisionCherryPickArgs {
            revision: LoreString::from("main@1"),
            message: LoreString::default(),
            no_commit: 0,
            inherit_metadata: LoreArray::default(),
        };

        assert_eq!(
            lore::interface::lore_revision_cherry_pick(&globals, &args, no_callback()),
            unavailable_code(),
            "the entry point must relay the call rather than run it here"
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

    /// A status with no service running succeeds with running=false, rather than
    /// failing with a connection error. This is the expected behavior when the
    /// service is not started.
    #[test]
    #[serial]
    fn status_without_service_returns_not_running() {
        let _settings = machine_settings("service-api-status-none-");

        let status_events: Arc<Mutex<Vec<lore_revision::event::LoreServiceStatusEventData>>> =
            Arc::default();
        let recorder = Arc::clone(&status_events);
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::ServiceStatus(data) = event {
                recorder
                    .lock()
                    .expect("the collector lock is not poisoned")
                    .push(data.clone());
            }
        }));

        let exit_code = lore::runtime().block_on(status(callback));

        assert_eq!(
            exit_code, 0,
            "status with no service running should succeed"
        );

        let events = status_events
            .lock()
            .expect("the collector lock is not poisoned");
        assert_eq!(events.len(), 1, "exactly one ServiceStatus event expected");
        assert_eq!(
            events[0].running, 0,
            "running should be 0 when service is not running"
        );
        assert!(
            events[0].binary_path.is_empty(),
            "binary_path should be empty when not running"
        );
        assert_eq!(
            events[0].uptime_ms, 0,
            "uptime_ms should be 0 when not running"
        );
        assert_eq!(
            events[0].connection_count, 0,
            "connection_count should be 0 when not running"
        );
        assert_eq!(
            events[0].swfs_mount_count, 0,
            "swfs_mount_count should be 0 when not running"
        );
    }

    /// A relayed call that reaches no service reports as a failing command does:
    /// the code on the return value and on one `Complete` carrying the detail,
    /// then `End`, with no legacy `Error` event.
    #[test]
    #[serial]
    fn a_relayed_call_that_reaches_no_service_completes_with_the_failure() {
        let _settings = relaying_to_no_service("service-api-relay-complete-");

        let mut status = 0;
        let received = delivered(|callback| {
            status = lore::interface::lore_revision_info(
                &globals(),
                &lore::revision::LoreRevisionInfoArgs::default(),
                callback,
            );
        });

        assert_eq!(received, failed_with(unavailable_code()));
        assert_eq!(status, unavailable_code(), "the return value is the status");
    }

    /// The asynchronous entry point returns nothing, so `Complete` is the only
    /// place a relayed call's failure reaches its caller.
    #[test]
    #[serial]
    fn an_asynchronous_relayed_call_that_reaches_no_service_completes_with_the_failure() {
        let _settings = relaying_to_no_service("service-api-relay-complete-async-");

        let received = delivered(|callback| {
            lore::interface::lore_revision_info_async(
                &globals(),
                &lore::revision::LoreRevisionInfoArgs::default(),
                callback,
            );
        });

        assert_eq!(received, failed_with(unavailable_code()));
    }
}
