// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The service API as an embedder calls it, rather than as the client does.
//!
//! `scripts/test/test_service.py` drives the same behaviours through `lore
//! service ...`, so it covers the CLI's wrapper and not the crate entry points a
//! program linking `liblore` uses. These are those entry points.
//!
//! What is here is the settings, the decision they feed, a stop with nothing to
//! stop, and the error a relayed call fails with when no service can be reached.
//! It also runs the service in this process with `service::run` and ends it with
//! `service::quit`, which only a Rust embedder can do. Starting a service as a
//! separate process, and stopping one, stay with the smoke suite.
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
    use lore::service::LoreServiceQuitArgs;
    use lore::service::LoreServiceRunArgs;
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

    use super::test_util::Replier;
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
    /// Nothing is left behind to clean up: the tests that run a service end it
    /// before they finish, and a name no service ever bound has no socket to
    /// remove.
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

    /// Where the tests make their global data directories and the targets they move the external
    /// root to: one volume Lore has verified, as a move by rename needs.
    static SCRATCH: LazyLock<std::path::PathBuf> = LazyLock::new(|| {
        lore_io::test_util::verified_scratch_base(std::path::Path::new(env!("CARGO_TARGET_TMPDIR")))
    });

    /// Asks the service running in this process to stop as it drops, so that a test that fails
    /// leaves none running for the next.
    struct StopsTheService;

    impl Drop for StopsTheService {
        fn drop(&mut self) {
            lore::remote::service_process::request_service_stop();
        }
    }

    /// Waits up to 30 s for `pause` to stop a pass, failing the test where `task`, which is to
    /// reach it, ends first.
    async fn until_reached<T>(
        pause: &lore_revision::test_util::ArmedPause,
        task: &mut tokio::task::JoinHandle<T>,
        what: &str,
    ) {
        tokio::select! {
            () = pause.reached() => {}
            _ = task => panic!("the task ended before it reached {what}"),
            () = tokio::time::sleep(Duration::from_secs(30)) => {
                panic!("the task didn't reach {what} within 30 s")
            }
        }
    }

    /// Points this process at a global config and a socket of its own, so the
    /// settings under test are the ones written here and not the machine's, and no
    /// call here can reach the machine's service.
    fn machine_settings(prefix: &str) -> TempDir {
        let ours = use_a_socket_of_our_own();
        let directory = TempDir::new_in(&SCRATCH, prefix);
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

    async fn quit() -> i32 {
        lore::service::quit(
            globals(),
            LoreServiceQuitArgs {},
            lore_revision::event::convert_event_callback(no_callback()),
        )
        .await
    }

    /// Runs the service in this process on a task of its own, as an embedder
    /// would, and waits until it accepts connections.
    async fn run_service_here() -> tokio::task::JoinHandle<i32> {
        let service = tokio::spawn(lore::service::run(
            globals(),
            LoreServiceRunArgs {},
            lore_revision::event::convert_event_callback(no_callback()),
        ));
        assert!(
            lore::remote::service_process::wait_until_a_service_is_listening()
                .await
                .expect("waiting for the service must not fail"),
            "the service must begin listening"
        );
        service
    }

    /// Whether anything accepts connections on this target's socket.
    fn socket_is_accepting() -> bool {
        lore::remote::network::UdsStream::connect(lore::remote::service_socket_name()).is_ok()
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
        delivered_and_after(call).0
    }

    /// [`delivered`], with the receiver of whatever the callback receives after `End`.
    fn delivered_and_after(
        call: impl FnOnce(LoreEventCallbackConfig),
    ) -> (Vec<Delivered>, mpsc::Receiver<Delivered>) {
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
        (received, receiver)
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

    /// A quit reaches only a service running in this process. With none there it
    /// is a no-op that succeeds, and must not go looking for one elsewhere.
    #[test]
    #[serial]
    fn quitting_when_no_service_runs_in_this_process_succeeds() {
        let _settings = machine_settings("service-api-quit-none-");

        assert_eq!(
            lore::runtime().block_on(quit()),
            0,
            "a quit with no service in this process has nothing to do"
        );
    }

    /// A quit ends a service running in this process: `run` returns success,
    /// and the socket stops accepting.
    #[test]
    #[serial]
    fn quitting_ends_a_service_running_in_this_process() {
        let _settings = machine_settings("service-api-quit-running-");

        lore::runtime().block_on(async {
            let _stops = StopsTheService;
            let service = run_service_here().await;
            assert!(socket_is_accepting());

            assert_eq!(quit().await, 0);
            let status = tokio::time::timeout(Duration::from_secs(15), service)
                .await
                .expect("the service must end after a quit")
                .expect("the service task must not panic");
            assert_eq!(status, 0, "a service that is quit ends successfully");

            assert!(
                !socket_is_accepting(),
                "a service that has ended must not still hold its socket"
            );
        });
    }

    /// A service can be run again in the same process after one has been quit,
    /// and the earlier quit does not stop the later service on arrival.
    #[test]
    #[serial]
    fn a_service_can_run_again_after_being_quit() {
        let _settings = machine_settings("service-api-quit-restart-");

        lore::runtime().block_on(async {
            let _stops = StopsTheService;
            for round in 0..2 {
                let service = run_service_here().await;

                // Still serving shortly after it began listening, which a stop
                // left over from the previous round would not allow.
                tokio::time::sleep(Duration::from_secs(1)).await;
                assert!(
                    !service.is_finished(),
                    "round {round} must still be serving"
                );
                assert!(socket_is_accepting());

                assert_eq!(quit().await, 0);
                let status = tokio::time::timeout(Duration::from_secs(15), service)
                    .await
                    .expect("the service must end after a quit")
                    .expect("the service task must not panic");
                assert_eq!(status, 0);
            }
        });
    }

    /// A quit that arrives after `run` has begun but before the service is
    /// serving, as a signal can, is kept and ends the service rather than being
    /// lost.
    #[test]
    #[serial]
    fn a_quit_while_the_service_is_starting_is_not_lost() {
        let _settings = machine_settings("service-api-quit-starting-");

        lore::runtime().block_on(async {
            let service = lore::service::run(
                globals(),
                LoreServiceRunArgs {},
                lore_revision::event::convert_event_callback(no_callback()),
            );
            tokio::pin!(service);

            // Polled once, which takes it as far as its first wait: begun, and
            // not yet listening.
            assert!(
                tokio::time::timeout(Duration::ZERO, &mut service)
                    .await
                    .is_err(),
                "the service must still be starting after one poll"
            );
            assert_eq!(quit().await, 0);

            let status = tokio::time::timeout(Duration::from_secs(15), service)
                .await
                .expect("a quit during start must still end the service");
            assert_eq!(status, 0);
            assert!(!socket_is_accepting());
        });
    }

    /// A second `run` in a process that already runs a service fails without
    /// disturbing the first: a later quit still reaches and ends it.
    #[test]
    #[serial]
    fn a_concurrent_run_fails_and_leaves_the_service_quittable() {
        let _settings = machine_settings("service-api-run-concurrent-");

        lore::runtime().block_on(async {
            let _stops = StopsTheService;
            let service = run_service_here().await;
            // The socket accepts a moment before the service is recorded as running.
            let deadline = Instant::now() + Duration::from_secs(15);
            while !lore::remote::service_process::service_runs_in_this_process() {
                assert!(Instant::now() < deadline, "the service must be recorded");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }

            let second = lore::service::run(
                globals(),
                LoreServiceRunArgs {},
                lore_revision::event::convert_event_callback(no_callback()),
            )
            .await;
            assert_ne!(second, 0, "a second service in one process must be refused");

            assert!(
                lore::remote::service_process::service_runs_in_this_process(),
                "the refused run must not clear the running service's state"
            );
            assert_eq!(quit().await, 0);
            let status = tokio::time::timeout(Duration::from_secs(15), service)
                .await
                .expect("the service must still end after a quit")
                .expect("the service task must not panic");
            assert_eq!(status, 0);
            assert!(!socket_is_accepting());
        });
    }

    /// Dropping the `run` future before it finishes does not leave an
    /// unreachable service holding the socket.
    #[test]
    #[serial]
    fn dropping_run_releases_the_socket() {
        let _settings = machine_settings("service-api-run-dropped-");

        lore::runtime().block_on(async {
            let _stops = StopsTheService;
            let service = run_service_here().await;
            assert!(socket_is_accepting());

            service.abort();
            let _ = service.await;

            let deadline = Instant::now() + Duration::from_secs(15);
            while socket_is_accepting() {
                assert!(
                    Instant::now() < deadline,
                    "an abandoned service must stop accepting connections"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
    }

    /// Running the service leaves the process's working directory as it was,
    /// since an embedder's is not the service's to change.
    #[test]
    #[serial]
    fn running_the_service_leaves_the_working_directory_alone() {
        let _settings = machine_settings("service-api-run-cwd-");

        lore::runtime().block_on(async {
            let _stops = StopsTheService;
            let before = std::env::current_dir().expect("a working directory");
            let service = run_service_here().await;
            assert_eq!(
                std::env::current_dir().expect("a working directory"),
                before
            );

            assert_eq!(quit().await, 0);
            tokio::time::timeout(Duration::from_secs(15), service)
                .await
                .expect("the service must end after a quit")
                .expect("the service task must not panic");
        });
    }

    /// A quit with the service already gone is the state it asks for, so it
    /// succeeds again rather than reaching for a service that has ended.
    #[test]
    #[serial]
    fn quitting_twice_succeeds() {
        let _settings = machine_settings("service-api-quit-twice-");

        lore::runtime().block_on(async {
            let _stops = StopsTheService;
            let service = run_service_here().await;
            assert_eq!(quit().await, 0);
            assert_eq!(quit().await, 0);
            let status = tokio::time::timeout(Duration::from_secs(15), service)
                .await
                .expect("the service must end after a quit")
                .expect("the service task must not panic");
            assert_eq!(status, 0);

            assert_eq!(quit().await, 0, "a quit after the service ended is a no-op");
        });
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

    /// With no executable named, a program that has not said its own executable runs the
    /// service starts none, and the failure names where one can be set.
    #[test]
    #[serial]
    fn a_start_with_none_named_starts_nothing_in_a_program_that_does_not_serve() {
        let _settings = machine_settings("service-api-none-named-");
        // Safety: as above.
        unsafe { std::env::remove_var(EXECUTABLE_VAR) };

        let (messages, callback) = capturing();
        let status = lore::runtime().block_on(start(callback));
        let reported = messages
            .lock()
            .expect("the collector lock is not poisoned")
            .join("\n");

        assert_eq!(
            status,
            unavailable_code(),
            "with none named, nothing is started: {reported}"
        );
        assert!(
            reported.contains("[service] executable"),
            "the failure must name where one can be set: {reported}"
        );
        assert!(!socket_is_accepting(), "no service answers on the socket");
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

    /// A status of the service running in this process carries every field: it runs, it reads as
    /// embedded, since this program never said it serves from its own executable, it reports
    /// this process's ID and no mount, and the external root is the default location, which can
    /// be used, with no leftovers.
    #[test]
    #[serial]
    fn status_of_a_service_running_here_carries_every_field() {
        let settings = machine_settings("service-api-status-running-");

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

        lore::runtime().block_on(async {
            let _stops = StopsTheService;
            let service = run_service_here().await;
            // The socket answers before the service records that it runs here, and a status
            // asked in between is relayed to the socket instead.
            let deadline = Instant::now() + Duration::from_secs(10);
            while !lore::remote::service_process::service_runs_in_this_process() {
                assert!(Instant::now() < deadline, "the service runs here");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(status(callback).await, 0, "the status succeeds");
            assert_eq!(quit().await, 0);
            tokio::time::timeout(Duration::from_secs(15), service)
                .await
                .expect("the service must end after a quit")
                .expect("the service task must not panic");
        });

        let events = status_events
            .lock()
            .expect("the collector lock is not poisoned");
        assert_eq!(events.len(), 1, "exactly one ServiceStatus event expected");
        let event = &events[0];
        assert_eq!(event.running, 1);
        assert_eq!(event.embedded, 1, "the service runs inside this program");
        assert_eq!(event.process_id, std::process::id());
        assert!(event.mounted_paths.is_empty());
        assert!(event.mounted_global_data_directories.is_empty());
        assert_eq!(
            event.external_root.as_str(),
            settings
                .path()
                .join("data")
                .join("external")
                .display()
                .to_string()
        );
        assert_eq!(
            event.external_root_state,
            lore::interface::LoreExternalRootState::Available
        );
        assert_eq!(event.external_leftovers, 0);
    }

    /// Turns relaying on toward this target's socket, where a [`Replier`] answers every
    /// request with the move reply, and names an executable that doesn't exist, so no service is
    /// started.
    fn relaying_to_a_move(settings: &TempDir) -> Replier {
        let missing = settings.path().join("no-such-lore");
        lore::runtime().block_on(async {
            assert_eq!(set_use_automatically(true).await, 0);
            assert_eq!(set_executable(&missing.to_string_lossy()).await, 0);
        });
        Replier::move_reply(use_a_socket_of_our_own())
    }

    /// An ordinary instance of a local repository at `path`, created in this process.
    fn ordinary_instance(path: &std::path::Path) {
        std::fs::create_dir(path).expect("make the instance's directory");
        let args = lore::repository::LoreRepositoryCreateArgs {
            repository_url: LoreString::default(),
            id: LoreString::default(),
            description: LoreString::default(),
            vfs: lore::repository::LoreVfsType::None,
            use_shared_store: lore_revision::repository::LoreSharedStoreMode::Disabled,
            shared_store_path: LoreString::default(),
        };
        let globals = LoreGlobalArgs {
            repository_path: path.into(),
            offline: 1,
            ..LoreGlobalArgs::default()
        };
        lore::runtime().block_on(async {
            assert_eq!(set_use_automatically(false).await, 0);
            assert_eq!(lore::repository::create(globals, args, None).await, 0);
        });
    }

    /// A call made with the C callback it is handed.
    type CallWith<'a> = Box<dyn FnOnce(LoreEventCallbackConfig) + 'a>;

    /// The arguments of a status that reports the tracked state alone.
    fn plain_status() -> lore::repository::LoreRepositoryStatusArgs {
        lore::repository::LoreRepositoryStatusArgs {
            staged: 0,
            scan: 0,
            check_dirty: 0,
            reset: 0,
            sync_point: 0,
            revision_only: 0,
            count: 0,
            paths: LoreArray::default(),
        }
    }

    /// A relayed call in an ordinary instance that the service socket answers with the move
    /// reply runs in this process with its own callback, through each relayed path: the
    /// synchronous and the asynchronous C API, and the Rust API. Each delivers one `Complete`
    /// and one `End`, and nothing after it.
    #[test]
    #[serial]
    fn a_call_answered_with_the_move_reply_runs_in_this_process() {
        let settings = machine_settings("service-api-move-reply-runs-");
        let instance = settings.path().join("instance");
        ordinary_instance(&instance);
        let replier = relaying_to_a_move(&settings);
        let globals = LoreGlobalArgs {
            repository_path: instance.as_path().into(),
            offline: 1,
            ..LoreGlobalArgs::default()
        };
        let args = plain_status();
        let ran = vec![
            Delivered::Complete {
                status: 0,
                error_code: 0,
                has_message: false,
                has_trace: false,
            },
            Delivered::End,
        ];

        let calls: [(&str, CallWith<'_>); 3] = [
            (
                "synchronous",
                Box::new(|callback| {
                    assert_eq!(
                        lore::interface::lore_repository_status(&globals, &args, callback),
                        0
                    );
                }),
            ),
            (
                "asynchronous",
                Box::new(|callback| {
                    lore::interface::lore_repository_status_async(&globals, &args, callback);
                }),
            ),
            (
                "Rust",
                Box::new(|callback| {
                    let status = lore::runtime().block_on(lore::repository::status(
                        globals.clone(),
                        args.clone(),
                        lore_revision::event::convert_event_callback(callback),
                    ));
                    assert_eq!(status, 0);
                }),
            ),
        ];
        for (path, call) in calls {
            let (received, after) = delivered_and_after(call);
            assert_eq!(received, ran, "{path}");
            assert!(
                after.recv_timeout(Duration::from_millis(300)).is_err(),
                "{path}: nothing follows the End"
            );
        }
        assert_eq!(replier.answered(), 3, "each call reached the socket");
    }

    /// A relayed call outside any instance that the service socket answers with the move reply
    /// runs in this process, as it would with no service.
    #[test]
    #[serial]
    fn a_call_outside_an_instance_answered_with_the_move_reply_runs_in_this_process() {
        let settings = machine_settings("service-api-move-reply-outside-");
        let replier = relaying_to_a_move(&settings);

        assert_eq!(
            lore::interface::lore_shared_store_list(
                &globals(),
                &LoreSharedStoreListArgs::default(),
                no_callback()
            ),
            0,
            "the list runs here"
        );
        assert_eq!(replier.answered(), 1, "the call reached the socket");
    }

    /// A relayed call at a path that holds no Lore directory, as a virtual instance's mount
    /// point holds none while the service is stopped, fails with `ServiceUnavailable` once the
    /// service socket has answered with the move reply, in place of the open's own failure.
    #[test]
    #[serial]
    fn a_call_without_a_lore_directory_answered_with_the_move_reply_is_unavailable() {
        let settings = machine_settings("service-api-move-reply-no-lore-");
        let replier = relaying_to_a_move(&settings);
        let globals = LoreGlobalArgs {
            repository_path: settings.path().into(),
            ..LoreGlobalArgs::default()
        };

        let mut status = 0;
        let received = delivered(|callback| {
            status = lore::interface::lore_repository_status(&globals, &plain_status(), callback);
        });

        assert_eq!(received, failed_with(unavailable_code()));
        assert_eq!(status, unavailable_code());
        assert_eq!(replier.answered(), 1, "the call reached the socket");
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

    /// Names the global data directory a child of this binary holds shared access to.
    const HOLD_SHARED_ACCESS_VAR: &str = "LORE_TEST_HOLD_SHARED_ACCESS";

    /// Names how long, in milliseconds, a child of this binary runs before it exits.
    const RUN_FOR_VAR: &str = "LORE_TEST_RUN_FOR_MS";

    /// What a child of this binary prints once it holds shared access.
    const HELD_LINE: &str = "shared access held";

    /// A child of this binary running `test` alone, with `env` set.
    fn child(test: &str, env: &[(&str, &std::ffi::OsStr)]) -> std::process::Child {
        let mut command = std::process::Command::new(
            std::env::current_exe().expect("this test binary's path is known"),
        );
        command
            .args(["--exact", test, "--nocapture", "--test-threads=1"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        for (name, value) in env {
            command.env(name, value);
        }
        command.spawn().expect("the child starts")
    }

    /// A child that runs for `milliseconds`, then exits.
    fn child_running_for(milliseconds: u64) -> std::process::Child {
        child(
            "tests::a_child_that_runs_for_a_while",
            &[(RUN_FOR_VAR, milliseconds.to_string().as_ref())],
        )
    }

    /// In a child of this binary, runs for as long as [`RUN_FOR_VAR`] says. Run by the harness,
    /// does nothing.
    #[test]
    fn a_child_that_runs_for_a_while() {
        let Some(milliseconds) = std::env::var_os(RUN_FOR_VAR) else {
            return;
        };
        let milliseconds: u64 = milliseconds
            .to_string_lossy()
            .parse()
            .expect("a number of milliseconds");
        std::thread::sleep(Duration::from_millis(milliseconds));
    }

    /// In a child of this binary, holds shared access to the external root of the global data
    /// directory [`HOLD_SHARED_ACCESS_VAR`] names, as a service does, says so, and exits a second
    /// later. Run by the harness, does nothing.
    #[test]
    fn a_child_that_holds_shared_access() {
        let Some(global) = std::env::var_os(HOLD_SHARED_ACCESS_VAR) else {
            return;
        };
        lore_revision::global::external_dir::access::require_shared(std::path::Path::new(&global))
            .expect("the child takes shared access");
        println!("{HELD_LINE}");
        std::thread::sleep(Duration::from_secs(1));
    }

    /// The global data directory of `settings`, which [`machine_settings`] pointed this process
    /// at.
    fn global_data_dir(_settings: &TempDir) -> std::path::PathBuf {
        lore_revision::global::get_global_data_dir().expect("the global data directory is made")
    }

    /// A handle opened while a child runs reads as running until the child exits, then as exited.
    #[test]
    #[serial]
    fn the_process_handle_tells_when_its_process_has_exited() {
        let mut running = child_running_for(2_000);
        let mut handle = lore::service::external_root::ServiceProcess::open(running.id())
            .expect("the handle opens");
        let while_running = handle.has_exited().expect("the handle is read");
        running.wait().expect("the child exits");
        let after = handle.has_exited().expect("the handle is read");

        assert!(!while_running, "the child still runs");
        assert!(after, "the child has exited");
    }

    /// The handle that asks for the process at each check, as one opens where the kernel refuses a
    /// pidfd, tells when its process has exited.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_polling_process_handle_tells_when_its_process_has_exited() {
        let mut running = child_running_for(2_000);
        let mut handle = lore::service::external_root::ServiceProcess::polling(running.id())
            .expect("the handle opens");
        let while_running = handle.has_exited().expect("the handle is read");
        running.wait().expect("the child exits");
        let after = handle.has_exited().expect("the handle is read");

        assert!(!while_running, "the child still runs");
        assert!(after, "the child has exited");
    }

    /// The socket is held as soon as a service gives it up, and refused at once, before the bound,
    /// where it is still held once the stopped service's process has exited.
    #[test]
    #[serial]
    fn the_socket_is_held_once_freed_and_refused_while_another_holds_it() {
        let _settings = machine_settings("service-api-occupy-");
        let name = lore::remote::service_socket_name();

        let service = lore::remote::network::UdsListener::new(name).expect("the service listens");
        let stopping = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(service);
        });
        let held = lore::runtime().block_on(async {
            let socket = lore::service::external_root::occupy_socket(
                name,
                None,
                tokio::time::Instant::now() + Duration::from_secs(10),
            )
            .await;
            let held = socket.is_ok();
            if let Ok(socket) = socket {
                socket.release().await;
            }
            held
        });
        stopping.join().expect("the service stops");

        let other = lore::remote::network::UdsListener::new(name).expect("another service listens");
        let mut exited = child_running_for(0);
        exited.wait().expect("the child exits");
        let mut handle = lore::service::external_root::ServiceProcess::open(exited.id())
            .expect("the handle opens");
        let started = Instant::now();
        let refused = lore::runtime().block_on(lore::service::external_root::occupy_socket(
            name,
            Some(&mut handle),
            tokio::time::Instant::now() + Duration::from_secs(30),
        ));
        let waited = started.elapsed();
        drop(other);

        assert!(held, "the socket is held once the service gives it up");
        assert!(
            refused.as_ref().is_err_and(|error| error.is_mounts_busy()),
            "a socket another service holds is refused with MountsBusy"
        );
        assert!(
            waited < Duration::from_secs(10),
            "the refusal came after {waited:?}, not once the process had exited"
        );
    }

    /// Exclusive access is taken once a child holding shared access, as a stopped service does,
    /// has exited, and is refused at the bound while another holder keeps it.
    #[test]
    #[serial]
    fn exclusive_access_waits_for_the_stopped_process_and_refuses_a_holder() {
        let settings = machine_settings("service-api-exclusive-");
        let global = global_data_dir(&settings);
        let mut holder = child(
            "tests::a_child_that_holds_shared_access",
            &[(HOLD_SHARED_ACCESS_VAR, global.as_os_str())],
        );
        let mut handle = lore::service::external_root::ServiceProcess::open(holder.id())
            .expect("the handle opens");
        let mut output = std::io::BufReader::new(holder.stdout.take().expect("the child's output"));
        let mut line = String::new();
        while !line.contains(HELD_LINE) {
            line.clear();
            let read = std::io::BufRead::read_line(&mut output, &mut line).expect("read the child");
            assert!(read > 0, "the child ended before it held shared access");
        }

        let taken = lore::runtime().block_on(lore::service::external_root::take_exclusive_access(
            &global,
            Some(&mut handle),
            tokio::time::Instant::now() + Duration::from_secs(30),
        ));
        let child_exited = handle.has_exited().expect("the handle is read");
        let taken = taken.map(drop).is_ok();
        holder.wait().expect("the child exits");

        let other =
            lore_base::fs::lock::FSLock::try_acquire_shared_file_lock(global.join("external"))
                .expect("the lock file opens")
                .expect("shared access is free");
        let bound = Duration::from_millis(300);
        let started = Instant::now();
        let refused =
            lore::runtime().block_on(lore::service::external_root::take_exclusive_access(
                &global,
                None,
                tokio::time::Instant::now() + bound,
            ));
        let waited = started.elapsed();
        drop(other);

        assert!(taken, "exclusive access is taken once the child has gone");
        assert!(child_exited, "exclusive access waits for the child to exit");
        assert!(
            refused.as_ref().is_err_and(|error| error.is_mounts_busy()),
            "a holder is refused with MountsBusy"
        );
        assert!(
            waited >= bound,
            "the refusal came after {waited:?}, before the bound"
        );
    }

    /// Exclusive access waits for the stopped service's process as well as for the lock, and is
    /// refused at the bound while the process still runs.
    #[test]
    #[serial]
    fn exclusive_access_is_refused_at_the_bound_while_the_process_runs() {
        let settings = machine_settings("service-api-bound-");
        let global = global_data_dir(&settings);
        let mut running = child_running_for(10_000);
        let mut handle = lore::service::external_root::ServiceProcess::open(running.id())
            .expect("the handle opens");

        let bound = Duration::from_millis(300);
        let started = Instant::now();
        let refused =
            lore::runtime().block_on(lore::service::external_root::take_exclusive_access(
                &global,
                Some(&mut handle),
                tokio::time::Instant::now() + bound,
            ));
        let waited = started.elapsed();
        let _ = running.kill();
        let _ = running.wait();

        assert!(
            refused.as_ref().is_err_and(|error| error.is_mounts_busy()),
            "a process still running at the bound is refused with MountsBusy"
        );
        assert!(
            waited >= bound,
            "the refusal came after {waited:?}, before the bound"
        );
        assert!(
            waited < Duration::from_secs(5),
            "the refusal came after {waited:?}, long past the bound"
        );
    }

    /// A service whose status doesn't decode is stopped as `lore service stop` stops one, the
    /// socket is held once it stops answering, and no service is started again.
    #[test]
    #[serial]
    fn a_status_that_does_not_decode_is_stopped_with_no_restart() {
        let settings = machine_settings("service-api-other-build-");
        let global = global_data_dir(&settings);
        let stopped_reply =
            lore::remote::message::encode_message(&lore::remote::message::MessageToClient::<
                LoreEvent,
            >::ApiResult(0));
        let other_build = Replier::in_turn(
            use_a_socket_of_our_own(),
            vec![vec![0xff; 8], stopped_reply],
        );

        let ended = lore::runtime().block_on(async {
            let found = lore::service::external_root::find_service()
                .await
                .expect("the status");
            let stopped = lore::service::external_root::stop_for_move(
                &global,
                &settings.path().join("target"),
                found,
                false,
                Duration::from_secs(10),
                std::future::pending(),
            )
            .await;
            match stopped {
                Ok(stopped) => Ok(stopped.end(std::future::pending()).await),
                Err(error) => Err(error.to_string()),
            }
        });
        let answered = other_build.answered();
        drop(other_build);

        assert_eq!(
            ended,
            Ok(lore::service::external_root::Restart::NoneStopped),
            "the service is stopped and none is started again"
        );
        assert_eq!(answered, 2, "the status and the stop reach the service");
    }

    /// The reply of a service of this build that reports running as process `process_id`, from the
    /// executable `executable`.
    #[cfg(target_family = "unix")]
    fn status_reply(process_id: u32, executable: &std::path::Path) -> Vec<u8> {
        use lore::remote::message::MessageToClient;
        use lore::remote::message::encode_message;

        let status = lore_revision::event::LoreServiceStatusEventData {
            running: 1,
            binary_path: LoreString::from(executable.to_str().expect("UTF-8")),
            uptime_ms: 0,
            connection_count: 0,
            swfs_mount_count: 0,
            external_root: LoreString::default(),
            external_root_state: lore_revision::event::LoreExternalRootState::Available,
            external_leftovers: 0,
            embedded: 0,
            mounted_global_data_directories: LoreArray::default(),
            mounted_paths: LoreArray::default(),
            process_id,
        };
        let mut reply = Vec::new();
        for message in [
            MessageToClient::<LoreEvent>::Event(LoreEvent::ServiceStatus(status)),
            MessageToClient::<LoreEvent>::ApiResult(0),
        ] {
            lore::remote::message::write_payload(&mut reply, &encode_message(&message))
                .expect("frame the reply");
        }
        reply
    }

    /// The reply of a service of this build that has carried out a stop.
    #[cfg(target_family = "unix")]
    fn stopped_reply() -> Vec<u8> {
        let mut reply = Vec::new();
        lore::remote::message::write_payload(
            &mut reply,
            &lore::remote::message::encode_message(&lore::remote::message::MessageToClient::<
                LoreEvent,
            >::ApiResult(0)),
        )
        .expect("frame the reply");
        reply
    }

    /// An executable at `path` that, run as the service, makes `ran` and then waits `seconds`.
    #[cfg(target_family = "unix")]
    fn fake_service_executable(path: &std::path::Path, ran: &std::path::Path, seconds: u32) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(
            path,
            format!("#!/bin/sh\ntouch '{}'\nsleep {seconds}\n", ran.display()),
        )
        .expect("the executable");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("make it executable");
    }

    /// A cancel while the move waits for the stopped service's process to exit ends the stop at
    /// once and starts the service again from its executable.
    #[cfg(target_family = "unix")]
    #[test]
    #[serial]
    fn a_cancel_while_the_stopped_service_exits_starts_it_again() {
        let settings = machine_settings("service-api-cancel-stop-");
        let global = global_data_dir(&settings);
        let (executable, ran) = (settings.path().join("lore"), settings.path().join("ran"));
        fake_service_executable(&executable, &ran, 0);
        let mut process = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("a process that doesn't exit");
        let service = Replier::in_turn_framed(
            use_a_socket_of_our_own(),
            vec![status_reply(process.id(), &executable), stopped_reply()],
        );

        let began = Instant::now();
        let stopped = lore::runtime().block_on(async {
            let found = lore::service::external_root::find_service()
                .await
                .expect("the status");
            tokio::time::timeout(
                Duration::from_secs(15),
                lore::service::external_root::stop_for_move(
                    &global,
                    &settings.path().join("target"),
                    found,
                    false,
                    Duration::from_secs(30),
                    tokio::time::sleep(Duration::from_millis(300)),
                ),
            )
            .await
            .map(|stopped| stopped.err().map(|error| error.to_string()))
        });
        let _ = process.kill();
        let _ = process.wait();
        drop(service);

        let failure = stopped
            .expect("the cancel ends the stop")
            .expect("the cancel fails the stop");
        assert!(failure.contains("cancelled"), "{failure}");
        assert!(ran.exists(), "the service is started again");
        assert!(began.elapsed() < Duration::from_secs(15));
    }

    /// A refusal once the service has stopped, here for the shared access another holder keeps to
    /// the external root, gives the socket up and starts the service again.
    #[cfg(target_family = "unix")]
    #[test]
    #[serial]
    fn a_refusal_after_the_stop_starts_the_service_again() {
        let settings = machine_settings("service-api-refused-after-stop-");
        let global = global_data_dir(&settings);
        std::fs::create_dir_all(&global).expect("the global data directory");
        let (executable, ran) = (settings.path().join("lore"), settings.path().join("ran"));
        fake_service_executable(&executable, &ran, 0);
        let mut process = std::process::Command::new("true")
            .spawn()
            .expect("a process that exits");
        let service = Replier::in_turn_framed(
            use_a_socket_of_our_own(),
            vec![status_reply(process.id(), &executable), stopped_reply()],
        );
        let shared =
            lore_base::fs::lock::FSLock::try_acquire_shared_file_lock(global.join("external"))
                .expect("attempt")
                .expect("hold shared access");

        let stopped = lore::runtime().block_on(async {
            let found = lore::service::external_root::find_service()
                .await
                .expect("the status");
            lore::service::external_root::stop_for_move(
                &global,
                &settings.path().join("target"),
                found,
                false,
                Duration::from_secs(1),
                std::future::pending(),
            )
            .await
            .err()
            .map(|error| error.ffi_code())
        });
        drop(shared);
        let _ = process.wait();
        drop(service);

        assert_eq!(
            stopped,
            Some(code_of(lore_base::error::MountsBusy {
                paths: String::new()
            }))
        );
        assert!(ran.exists(), "the service is started again");
        assert!(
            lore::remote::network::UdsListener::claim(lore::remote::service_socket_name())
                .expect("claim the socket")
                .is_some(),
            "the socket is given up"
        );
    }

    /// A cancel while the service starts again ends the wait for it to answer.
    #[cfg(target_family = "unix")]
    #[test]
    #[serial]
    fn a_cancel_while_the_service_starts_again_ends_the_wait() {
        let settings = machine_settings("service-api-cancel-restart-");
        let global = global_data_dir(&settings);
        let (executable, ran) = (settings.path().join("lore"), settings.path().join("ran"));
        fake_service_executable(&executable, &ran, 30);
        let mut process = std::process::Command::new("true")
            .spawn()
            .expect("a process that exits");
        let service = Replier::in_turn_framed(
            use_a_socket_of_our_own(),
            vec![status_reply(process.id(), &executable), stopped_reply()],
        );

        let ended = lore::runtime().block_on(async {
            let found = lore::service::external_root::find_service()
                .await
                .expect("the status");
            let stopped = lore::service::external_root::stop_for_move(
                &global,
                &settings.path().join("target"),
                found,
                false,
                Duration::from_secs(10),
                std::future::pending(),
            )
            .await
            .expect("the stop");
            tokio::time::timeout(
                Duration::from_secs(5),
                stopped.end(tokio::time::sleep(Duration::from_millis(300))),
            )
            .await
        });
        let _ = process.wait();
        drop(service);

        assert_eq!(
            ended.expect("the cancel ends the wait"),
            lore::service::external_root::Restart::Unawaited
        );
        assert!(ran.exists(), "the service is started again");
    }

    /// A dry run cancelled while it measures the root stops measuring and fails, and changes
    /// nothing.
    #[test]
    #[serial]
    fn a_dry_run_cancelled_at_once_fails() {
        let settings = machine_settings("service-api-set-external-dry-cancel-");
        let global = global_data_dir(&settings);
        let name = default_root_with_an_instance(&global);
        for file in 0..4000 {
            std::fs::write(
                global.join("external").join(&name).join(file.to_string()),
                "",
            )
            .expect("a file to measure");
        }
        let target = settings.path().join("target");
        let messages: Arc<Mutex<Vec<String>>> = Arc::default();
        let recorder = Arc::clone(&messages);
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            let message = match event {
                LoreEvent::Log(data) => data.message.to_string(),
                LoreEvent::Complete(data) => data.error.message.to_string(),
                _ => return,
            };
            recorder
                .lock()
                .expect("the collector lock is not poisoned")
                .push(message);
        }));

        let level = lore_base::log::log_level();
        lore::log::initialize();
        let status = lore::runtime().block_on(lore::service::set_external_directory_until(
            LoreGlobalArgs {
                dry_run: 1,
                ..globals()
            },
            lore::service::LoreServiceSetExternalDirectoryArgs {
                path: LoreString::from(target.to_str().expect("UTF-8")),
                adopt: 0,
                abandon: 0,
            },
            callback,
            std::future::ready(()),
        ));
        lore_base::log::set_log_level(level);

        assert_ne!(status, 0);
        let messages = messages.lock().expect("the collector lock is not poisoned");
        assert!(
            messages.iter().any(|message| message.contains("cancelled")),
            "{messages:?}"
        );
        assert!(
            !messages
                .iter()
                .any(|message| message.contains("external directories,")),
            "the measuring stops at the cancel: {messages:?}"
        );
    }

    /// A service that closes the connection without answering the status, as no service of another
    /// build does, fails the reading of the status, rather than reading as one of another build.
    #[test]
    #[serial]
    fn a_status_closed_without_an_answer_fails() {
        let _settings = machine_settings("service-api-status-closed-");
        let service = Replier::in_turn_framed(use_a_socket_of_our_own(), vec![Vec::new()]);

        let found = lore::runtime().block_on(lore::service::external_root::find_service());
        drop(service);

        assert!(
            matches!(&found, Err(error) if error.is_internal()),
            "{:?}",
            found.map(|found| matches!(
                found,
                lore::service::external_root::FoundService::OtherBuild
            ))
        );
    }

    /// A start that fails is tried once more, and only once.
    #[test]
    fn a_failed_start_is_tried_once_more() {
        let attempts = |results: Vec<bool>| {
            let tried = std::cell::Cell::new(0);
            let result =
                lore::runtime().block_on(lore::service::external_root::start_twice(|| {
                    let attempt = tried.get();
                    tried.set(attempt + 1);
                    let started = results[attempt];
                    async move {
                        if started {
                            Ok(())
                        } else {
                            Err(
                                lore::remote::service_process::ServiceProcessError::internal(
                                    "the start failed",
                                ),
                            )
                        }
                    }
                }));
            (result.is_ok(), tried.get())
        };

        assert_eq!(
            attempts(vec![true]),
            (true, 1),
            "a start that succeeds is tried once"
        );
        assert_eq!(
            attempts(vec![false, true]),
            (true, 2),
            "a failed start is tried again"
        );
        assert_eq!(
            attempts(vec![false, false]),
            (false, 2),
            "a second failure ends the tries"
        );
    }

    /// A start again that fails twice reports that no service started, which the move reports as
    /// a warning rather than a failure.
    #[test]
    #[serial]
    fn a_start_again_that_fails_twice_reports_no_service() {
        let _settings = machine_settings("service-api-restart-fails-");

        let started = lore::runtime().block_on(lore::service::external_root::start_again(
            std::path::Path::new(exits_immediately()),
        ));

        assert!(
            !started,
            "no service starts from an executable that exits at once"
        );
        assert!(!socket_is_accepting(), "no service answers on the socket");
    }

    /// A status event of a service of this build, as a move reads it before the stop.
    fn reported(
        embedded: u8,
        globals: &[&std::path::Path],
        paths: &[&std::path::Path],
    ) -> lore_revision::event::LoreServiceStatusEventData {
        let strings = |paths: &[&std::path::Path]| {
            LoreArray::from_vec(
                paths
                    .iter()
                    .map(|path| LoreString::from(path.to_string_lossy().as_ref()))
                    .collect(),
            )
        };
        lore_revision::event::LoreServiceStatusEventData {
            running: 1,
            binary_path: LoreString::from("lore"),
            uptime_ms: 1,
            connection_count: 0,
            swfs_mount_count: 0,
            external_root: LoreString::default(),
            external_root_state: lore::interface::LoreExternalRootState::Available,
            external_leftovers: 0,
            embedded,
            mounted_global_data_directories: strings(globals),
            mounted_paths: strings(paths),
            process_id: 1,
        }
    }

    /// A move refuses to stop a service that runs inside another program or mounts virtual
    /// instances of another global data directory, and a target inside a virtual instance the
    /// service mounts.
    #[test]
    fn a_reported_service_is_refused_as_its_status_says() {
        use lore::service::external_root::refuse_reported_service;

        let global = std::path::Path::new("/data/global");
        let mounted = std::path::Path::new("/work/virtual");
        let target = std::path::Path::new("/roots/next");

        assert!(
            refuse_reported_service(&reported(0, &[global], &[mounted]), global, target).is_ok(),
            "a service of this global data directory is stopped"
        );
        assert!(
            refuse_reported_service(&reported(1, &[], &[]), global, target)
                .is_err_and(|error| error.is_mounts_busy()),
            "a service inside another program is refused"
        );
        assert!(
            refuse_reported_service(
                &reported(
                    0,
                    &[global, std::path::Path::new("/data/other")],
                    &[mounted]
                ),
                global,
                target
            )
            .is_err_and(|error| error.is_mounts_busy()),
            "a service mounting another global data directory's instances is refused"
        );
        assert!(
            refuse_reported_service(
                &reported(0, &[global], &[mounted]),
                global,
                &mounted.join("root")
            )
            .is_err_and(|error| error.is_invalid_arguments()),
            "a target inside a mounted virtual instance is refused"
        );
    }

    /// Runs `lore service set-external-directory` with `path`, `adopt` and `abandon`, `globals`
    /// and `termination`, and answers its status.
    async fn set_external_directory(
        globals: LoreGlobalArgs,
        path: &str,
        (adopt, abandon): (bool, bool),
        termination: impl Future<Output = ()> + 'static,
    ) -> i32 {
        lore::service::set_external_directory_until(
            globals,
            lore::service::LoreServiceSetExternalDirectoryArgs {
                path: LoreString::from(path),
                adopt: u8::from(adopt),
                abandon: u8::from(abandon),
            },
            lore_revision::event::convert_event_callback(no_callback()),
            termination,
        )
        .await
    }

    /// The default root of `global`, holding an external directory with a file, whose name it
    /// answers.
    fn default_root_with_an_instance(global: &std::path::Path) -> String {
        let name = lore_revision::instance::InstanceId::generate().text_encoding();
        let directory = global.join("external").join(&name);
        std::fs::create_dir_all(&directory).expect("the external directory");
        std::fs::write(directory.join("file"), "kept").expect("a file");
        name
    }

    fn code_of(error: impl FfiError) -> i32 {
        error.ffi_code()
    }

    /// The command refuses a program that runs the service itself, which it can't stop and start
    /// again, with `MountsBusy`.
    #[test]
    #[serial]
    fn set_external_directory_refuses_a_program_that_runs_the_service() {
        let settings = machine_settings("service-api-set-external-embedded-");
        let target = settings.path().join("target");
        lore::runtime().block_on(async {
            let _stops = StopsTheService;
            let service = run_service_here().await;
            let status = set_external_directory(
                globals(),
                target.to_str().expect("UTF-8"),
                (false, false),
                std::future::pending(),
            )
            .await;
            assert_eq!(quit().await, 0);
            service.await.expect("the service ends");
            assert_eq!(
                status,
                code_of(lore_base::error::MountsBusy {
                    paths: String::new()
                })
            );
        });
        assert!(!target.exists());
    }

    /// The command refuses a program whose service is still starting, after it listens on its
    /// socket and before it serves connections, as one whose service runs, naming the service
    /// this program runs.
    #[test]
    #[serial]
    fn set_external_directory_refuses_a_program_whose_service_is_starting() {
        let settings = machine_settings("service-api-set-external-starting-");
        let target = settings.path().join("target");
        lore::runtime().block_on(async {
            let listened = &*lore::service::service_main::LISTENED;
            let _listened = listened.arm_while_held();
            let _stops = StopsTheService;
            let mut service = tokio::spawn(lore::service::run(
                globals(),
                LoreServiceRunArgs {},
                lore_revision::event::convert_event_callback(no_callback()),
            ));
            until_reached(listened, &mut service, "the listen").await;
            let (messages, callback) = capturing();
            let status = tokio::time::timeout(
                Duration::from_secs(30),
                lore::service::set_external_directory_until(
                    globals(),
                    lore::service::LoreServiceSetExternalDirectoryArgs {
                        path: LoreString::from(target.to_str().expect("UTF-8")),
                        adopt: 0,
                        abandon: 0,
                    },
                    callback,
                    std::future::pending(),
                ),
            )
            .await;
            listened.resume();
            assert_eq!(quit().await, 0);
            service.await.expect("the service ends");
            let status = status.expect("the command answers while the service starts");
            assert_eq!(
                status,
                code_of(lore_base::error::MountsBusy {
                    paths: String::new()
                })
            );
            let messages = messages.lock().expect("the collector lock is not poisoned");
            assert!(
                messages
                    .iter()
                    .any(|message| message.contains("the Lore service this program runs")),
                "{messages:?}"
            );
        });
        assert!(!target.exists());
    }

    /// Runs a service in this process, holds its start after it listens on its socket and before
    /// it serves connections, and runs `call` there for up to 30 s. Answers the call's outcome,
    /// `None` where it didn't answer in time, and the service's task, once the start goes on.
    async fn in_the_listen_window<T>(
        call: impl Future<Output = T>,
    ) -> (Option<T>, tokio::task::JoinHandle<i32>) {
        let listened = &*lore::service::service_main::LISTENED;
        let _listened = listened.arm_while_held();
        let mut service = tokio::spawn(lore::service::run(
            globals(),
            LoreServiceRunArgs {},
            lore_revision::event::convert_event_callback(no_callback()),
        ));
        until_reached(listened, &mut service, "the listen").await;
        let answered = tokio::time::timeout(Duration::from_secs(30), call)
            .await
            .ok();
        listened.resume();
        (answered, service)
    }

    /// A start in a program whose service is still starting succeeds at once, as one whose
    /// service runs, and the service then serves.
    #[test]
    #[serial]
    fn a_start_while_this_programs_service_starts_succeeds() {
        let _settings = machine_settings("service-api-start-starting-");
        lore::runtime().block_on(async {
            let _stops = StopsTheService;
            let (started, service) = in_the_listen_window(start(None)).await;
            assert_eq!(
                started,
                Some(0),
                "the start answers while the service starts"
            );
            assert_eq!(quit().await, 0);
            assert_eq!(service.await.expect("the service ends"), 0);
        });
    }

    /// A status in a program whose service is still starting reports that service, running in
    /// this process, as one whose service runs.
    #[test]
    #[serial]
    fn a_status_while_this_programs_service_starts_reports_it() {
        let _settings = machine_settings("service-api-status-starting-");
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
        lore::runtime().block_on(async {
            let _stops = StopsTheService;
            let (reported, service) = in_the_listen_window(status(callback)).await;
            assert_eq!(quit().await, 0);
            service.await.expect("the service ends");
            assert_eq!(
                reported,
                Some(0),
                "the status answers while the service starts"
            );
        });
        let events = status_events
            .lock()
            .expect("the collector lock is not poisoned");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].running, 1);
        assert_eq!(events[0].process_id, std::process::id());
    }

    /// A stop in a program whose service is still starting is kept, and the service ends once it
    /// has started, with no quit.
    #[test]
    #[serial]
    fn a_stop_while_this_programs_service_starts_ends_it() {
        let _settings = machine_settings("service-api-stop-starting-");
        lore::runtime().block_on(async {
            let _stops = StopsTheService;
            let (stopped, service) = in_the_listen_window(stop()).await;
            let ended = tokio::time::timeout(Duration::from_secs(30), service).await;
            if ended.is_err() {
                quit().await;
            }
            assert_eq!(
                stopped,
                Some(0),
                "the stop answers while the service starts"
            );
            assert_eq!(
                ended
                    .expect("the service ends by the stop")
                    .expect("the service task"),
                0
            );
        });
    }

    /// A status in a program whose service start waits on a socket another holds reports no
    /// service of this program.
    #[test]
    #[serial]
    fn a_status_while_this_programs_start_is_refused_the_socket_reports_none_here() {
        let _settings = machine_settings("service-api-status-refused-");
        let held = lore::remote::network::UdsListener::claim(lore::remote::service_socket_name())
            .expect("claim the socket")
            .expect("nothing holds it");
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
        lore::runtime().block_on(async {
            let _stops = StopsTheService;
            let refused = &*lore::service::service_main::REFUSED;
            let _refused = refused.arm_while_held();
            let mut service = tokio::spawn(lore::service::run(
                globals(),
                LoreServiceRunArgs {},
                lore_revision::event::convert_event_callback(no_callback()),
            ));
            until_reached(refused, &mut service, "the refused claim").await;
            let reported = tokio::time::timeout(Duration::from_secs(30), status(callback)).await;
            refused.resume();
            assert_eq!(quit().await, 0);
            assert_ne!(
                service.await.expect("the service task"),
                0,
                "the start is refused"
            );
            assert_eq!(reported.expect("the status answers"), 0);
        });
        drop(held);
        let events = status_events
            .lock()
            .expect("the collector lock is not poisoned");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].running, 0);
    }

    /// Holds `call` where it is about to connect to the service, having found none in this
    /// process, runs a service in this process until it listens, and lets `call` connect, for up
    /// to 30 s. Answers the call's outcome, `None` where it didn't answer in time, and the
    /// service's task.
    async fn racing_this_programs_start<T: Send + 'static>(
        call: impl Future<Output = T> + Send + 'static,
    ) -> (Option<T>, tokio::task::JoinHandle<i32>) {
        let connecting = &*lore::remote::service_process::CONNECTING;
        let _connecting = connecting.arm_while_held();
        let mut call = tokio::spawn(call);
        until_reached(connecting, &mut call, "its connect").await;
        let service = run_service_here().await;
        connecting.resume();
        let answered = tokio::time::timeout(Duration::from_secs(30), call)
            .await
            .ok()
            .map(|joined| joined.expect("the call's task"));
        (answered, service)
    }

    /// Holds `call` where it is about to connect to the service, having found none in this
    /// process, starts a service in this process and holds it once it has claimed the socket,
    /// before it listens, and lets `call` connect, for up to 30 s. Then lets the service go on.
    /// Answers the call's outcome, `None` where it didn't answer in time, and the service's task.
    async fn racing_this_programs_claim<T: Send + 'static>(
        call: impl Future<Output = T> + Send + 'static,
    ) -> (Option<T>, tokio::task::JoinHandle<i32>) {
        let (connecting, claimed) = (
            &*lore::remote::service_process::CONNECTING,
            &*lore::service::service_main::CLAIMED,
        );
        let (_connecting, _claimed) = (connecting.arm_while_held(), claimed.arm_while_held());
        let mut call = tokio::spawn(call);
        until_reached(connecting, &mut call, "its connect").await;
        let mut service = tokio::spawn(lore::service::run(
            globals(),
            LoreServiceRunArgs {},
            lore_revision::event::convert_event_callback(no_callback()),
        ));
        until_reached(claimed, &mut service, "the claim").await;
        connecting.resume();
        let answered = tokio::time::timeout(Duration::from_secs(30), call)
            .await
            .ok()
            .map(|joined| joined.expect("the call's task"));
        claimed.resume();
        (answered, service)
    }

    /// A start refused a connect while this program's service, begun after the start looked for
    /// one here, has claimed the socket and not yet listens, succeeds, as one that finds it here.
    #[test]
    #[serial]
    fn a_start_racing_this_programs_claim_succeeds() {
        let _settings = machine_settings("service-api-start-racing-claim-");
        lore::runtime().block_on(async {
            let _stops = StopsTheService;
            let (started, service) = racing_this_programs_claim(start(None)).await;
            assert_eq!(started, Some(0), "the start answers");
            assert!(
                lore::remote::service_process::wait_until_a_service_is_listening()
                    .await
                    .expect("waiting for the service must not fail"),
                "the service listens"
            );
            assert_eq!(quit().await, 0);
            assert_eq!(service.await.expect("the service ends"), 0);
        });
    }

    /// A status refused a connect while this program's service, begun after the status looked for
    /// one here, has claimed the socket, reports that service, running in this process.
    #[test]
    #[serial]
    fn a_status_racing_this_programs_claim_reports_it() {
        let _settings = machine_settings("service-api-status-racing-claim-");
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
        lore::runtime().block_on(async {
            let _stops = StopsTheService;
            let (reported, service) = racing_this_programs_claim(status(callback)).await;
            assert!(
                lore::remote::service_process::wait_until_a_service_is_listening()
                    .await
                    .expect("waiting for the service must not fail"),
                "the service listens"
            );
            assert_eq!(quit().await, 0);
            service.await.expect("the service ends");
            assert_eq!(reported, Some(0), "the status answers");
        });
        let events = status_events
            .lock()
            .expect("the collector lock is not poisoned");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].running, 1);
        assert_eq!(events[0].process_id, std::process::id());
    }

    /// A stop refused a connect while this program's service, begun after the stop looked for one
    /// here, has claimed the socket, is kept and ends that service once it has started.
    #[test]
    #[serial]
    fn a_stop_racing_this_programs_claim_ends_it() {
        let _settings = machine_settings("service-api-stop-racing-claim-");
        lore::runtime().block_on(async {
            let _stops = StopsTheService;
            let (stopped, service) = racing_this_programs_claim(stop()).await;
            let ended = tokio::time::timeout(Duration::from_secs(30), service).await;
            if ended.is_err() {
                quit().await;
            }
            assert_eq!(stopped, Some(0), "the stop answers");
            assert_eq!(
                ended
                    .expect("the service ends by the stop")
                    .expect("the service task"),
                0
            );
        });
    }

    /// The command refuses a program whose service, begun after the command looked for one here,
    /// has claimed the socket and not yet listens, naming the service this program runs.
    #[test]
    #[serial]
    fn set_external_directory_racing_this_programs_claim_is_refused() {
        let settings = machine_settings("service-api-set-external-racing-claim-");
        let target = settings.path().join("target");
        let (messages, callback) = capturing();
        let path = LoreString::from(target.to_str().expect("UTF-8"));
        lore::runtime().block_on(async {
            let _stops = StopsTheService;
            let (status, service) =
                racing_this_programs_claim(lore::service::set_external_directory_until(
                    globals(),
                    lore::service::LoreServiceSetExternalDirectoryArgs {
                        path,
                        adopt: 0,
                        abandon: 0,
                    },
                    callback,
                    std::future::pending(),
                ))
                .await;
            assert!(
                lore::remote::service_process::wait_until_a_service_is_listening()
                    .await
                    .expect("waiting for the service must not fail"),
                "the service listens"
            );
            assert_eq!(quit().await, 0);
            service.await.expect("the service ends");
            assert_eq!(
                status,
                Some(code_of(lore_base::error::MountsBusy {
                    paths: String::new()
                }))
            );
        });
        let messages = messages.lock().expect("the collector lock is not poisoned");
        assert!(
            messages
                .iter()
                .any(|message| message.contains("the Lore service this program runs")),
            "{messages:?}"
        );
        assert!(!target.exists());
    }

    /// A start that connects to the service this program began to run after it looked for one
    /// here succeeds, as one that finds it here.
    #[test]
    #[serial]
    fn a_start_racing_this_programs_service_start_succeeds() {
        let _settings = machine_settings("service-api-start-racing-");
        lore::runtime().block_on(async {
            let _stops = StopsTheService;
            let (started, service) = racing_this_programs_start(start(None)).await;
            assert_eq!(started, Some(0), "the start answers");
            assert_eq!(quit().await, 0);
            assert_eq!(service.await.expect("the service ends"), 0);
        });
    }

    /// A status that connects to the service this program began to run after it looked for one
    /// here reports that service, running in this process.
    #[test]
    #[serial]
    fn a_status_racing_this_programs_service_start_reports_it() {
        let _settings = machine_settings("service-api-status-racing-");
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
        lore::runtime().block_on(async {
            let _stops = StopsTheService;
            let (reported, service) = racing_this_programs_start(status(callback)).await;
            assert_eq!(quit().await, 0);
            service.await.expect("the service ends");
            assert_eq!(reported, Some(0), "the status answers");
        });
        let events = status_events
            .lock()
            .expect("the collector lock is not poisoned");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].running, 1);
        assert_eq!(events[0].process_id, std::process::id());
    }

    /// A stop that connects to the service this program began to run after it looked for one
    /// here ends that service.
    #[test]
    #[serial]
    fn a_stop_racing_this_programs_service_start_ends_it() {
        let _settings = machine_settings("service-api-stop-racing-");
        lore::runtime().block_on(async {
            let _stops = StopsTheService;
            let (stopped, service) = racing_this_programs_start(stop()).await;
            let ended = tokio::time::timeout(Duration::from_secs(30), service).await;
            if ended.is_err() {
                quit().await;
            }
            assert_eq!(stopped, Some(0), "the stop answers");
            assert_eq!(
                ended
                    .expect("the service ends by the stop")
                    .expect("the service task"),
                0
            );
        });
    }

    /// The command refuses a program whose service began to run after the command looked for one
    /// here, naming the service this program runs.
    #[test]
    #[serial]
    fn set_external_directory_racing_this_programs_service_start_is_refused() {
        let settings = machine_settings("service-api-set-external-racing-");
        let target = settings.path().join("target");
        let (messages, callback) = capturing();
        let path = LoreString::from(target.to_str().expect("UTF-8"));
        lore::runtime().block_on(async {
            let _stops = StopsTheService;
            let (status, service) =
                racing_this_programs_start(lore::service::set_external_directory_until(
                    globals(),
                    lore::service::LoreServiceSetExternalDirectoryArgs {
                        path,
                        adopt: 0,
                        abandon: 0,
                    },
                    callback,
                    std::future::pending(),
                ))
                .await;
            assert_eq!(quit().await, 0);
            service.await.expect("the service ends");
            assert_eq!(
                status,
                Some(code_of(lore_base::error::MountsBusy {
                    paths: String::new()
                }))
            );
        });
        let messages = messages.lock().expect("the collector lock is not poisoned");
        assert!(
            messages
                .iter()
                .any(|message| message.contains("the Lore service this program runs")),
            "{messages:?}"
        );
        assert!(!target.exists());
    }

    /// A dry run checks and reports, and changes nothing.
    #[test]
    #[serial]
    fn a_dry_run_of_set_external_directory_changes_nothing() {
        let settings = machine_settings("service-api-set-external-dry-run-");
        let global = global_data_dir(&settings);
        let name = default_root_with_an_instance(&global);
        let target = settings.path().join("target");
        let dry_run = LoreGlobalArgs {
            dry_run: 1,
            ..globals()
        };

        let status = lore::runtime().block_on(set_external_directory(
            dry_run,
            target.to_str().expect("UTF-8"),
            (false, false),
            std::future::pending(),
        ));

        assert_eq!(status, 0);
        assert!(!target.exists());
        assert!(!global.join("external-root").exists());
        assert!(!global.join("external.move").exists());
        assert!(global.join("external").join(name).join("file").exists());
    }

    /// A dry run of a move that copies to a volume without the room for it is refused with
    /// `InvalidArguments`, as the move would be before anything stops, and changes nothing.
    #[test]
    #[serial]
    fn a_dry_run_of_a_copy_to_a_full_volume_is_refused() {
        let settings = machine_settings("service-api-set-external-dry-run-full-");
        let global = global_data_dir(&settings);
        let name = default_root_with_an_instance(&global);
        let target = settings.path().join("target");
        let _full = lore_revision::test_util::override_moves(
            &global,
            lore_revision::test_util::MoveOverride::CopyToFullVolume,
        );
        let dry_run = LoreGlobalArgs {
            dry_run: 1,
            ..globals()
        };

        let status = lore::runtime().block_on(set_external_directory(
            dry_run,
            target.to_str().expect("UTF-8"),
            (false, false),
            std::future::pending(),
        ));

        assert_eq!(
            status,
            code_of(lore_base::error::InvalidArguments {
                reason: String::new()
            })
        );
        assert!(!target.exists());
        assert!(global.join("external").join(name).join("file").exists());
    }

    /// A dry run of a first move by copy refuses a default root another user owns, as the move
    /// would before anything stops, and changes nothing. Runs as root, which can give the root away.
    #[cfg(target_family = "unix")]
    #[test]
    #[serial]
    fn a_dry_run_of_a_first_copy_refuses_a_root_another_user_owns() {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let settings = machine_settings("service-api-set-external-dry-run-owner-");
        let global = global_data_dir(&settings);
        let name = default_root_with_an_instance(&global);
        std::os::unix::fs::chown(global.join("external"), Some(4321), None).expect("chown");
        let target = settings.path().join("target");
        let _copying = lore_revision::test_util::override_moves(
            &global,
            lore_revision::test_util::MoveOverride::Copy,
        );
        let dry_run = LoreGlobalArgs {
            dry_run: 1,
            ..globals()
        };

        let status = lore::runtime().block_on(set_external_directory(
            dry_run,
            target.to_str().expect("UTF-8"),
            (false, false),
            std::future::pending(),
        ));

        assert_ne!(status, 0);
        assert!(!target.exists());
        assert!(global.join("external").join(name).join("file").exists());
    }

    /// A dry run while another move holds the service socket is refused with `MountsBusy`, as the
    /// move would be.
    #[test]
    #[serial]
    fn a_dry_run_while_another_move_holds_the_socket_is_refused() {
        let settings = machine_settings("service-api-set-external-dry-run-held-");
        let global = global_data_dir(&settings);
        default_root_with_an_instance(&global);
        let target = settings.path().join("target");
        let dry_run = LoreGlobalArgs {
            dry_run: 1,
            ..globals()
        };

        let status = lore::runtime().block_on(async {
            let socket = lore::service::external_root::occupy_socket(
                lore::remote::service_socket_name(),
                None,
                tokio::time::Instant::now() + Duration::from_secs(10),
            )
            .await
            .expect("the socket is free to hold");
            let status = set_external_directory(
                dry_run,
                target.to_str().expect("UTF-8"),
                (false, false),
                std::future::pending(),
            )
            .await;
            socket.release().await;
            status
        });

        assert_eq!(
            status,
            code_of(lore_base::error::MountsBusy {
                paths: String::new()
            })
        );
        assert!(!target.exists());
    }

    /// A dry run of `--adopt` refuses a link or a junction to the root, which names the root
    /// without being one, with `InvalidArguments`.
    #[test]
    #[serial]
    fn a_dry_run_adopt_of_a_link_to_the_root_is_refused() {
        let settings = machine_settings("service-api-set-external-adopt-link-");
        let global = global_data_dir(&settings);
        default_root_with_an_instance(&global);
        let (root, link) = (settings.path().join("root"), settings.path().join("link"));
        let dry_run = LoreGlobalArgs {
            dry_run: 1,
            ..globals()
        };

        let status = lore::runtime().block_on(async {
            let moved = set_external_directory(
                globals(),
                root.to_str().expect("UTF-8"),
                (false, false),
                std::future::pending(),
            )
            .await;
            assert_eq!(moved, 0);
            #[cfg(target_family = "unix")]
            std::os::unix::fs::symlink(&root, &link).expect("the link");
            #[cfg(target_family = "windows")]
            lore_io::junction::create_junction(&root, &link).expect("the junction");
            set_external_directory(
                dry_run,
                link.to_str().expect("UTF-8"),
                (true, false),
                std::future::pending(),
            )
            .await
        });

        assert_eq!(
            status,
            code_of(lore_base::error::InvalidArguments {
                reason: String::new()
            })
        );
    }

    /// `--abandon` is refused with `InvalidArguments` while `external-root` is missing, and while
    /// the root it names is available.
    #[test]
    #[serial]
    fn an_abandon_of_no_setting_or_an_available_root_is_refused() {
        let settings = machine_settings("service-api-set-external-abandon-refused-");
        let global = global_data_dir(&settings);
        default_root_with_an_instance(&global);
        let target = settings.path().join("target");
        let abandon = || {
            lore::runtime().block_on(set_external_directory(
                globals(),
                target.to_str().expect("UTF-8"),
                (false, true),
                std::future::pending(),
            ))
        };
        let refused = code_of(lore_base::error::InvalidArguments {
            reason: String::new(),
        });

        assert_eq!(abandon(), refused, "no setting");
        let id = lore_revision::global::external_dir::setting::RootId::generate();
        lore::runtime().block_on(async {
            lore_revision::global::external_dir::marker::Marker {
                id,
                generation: std::num::NonZeroU64::MIN,
            }
            .save(&global.join("external"))
            .await
            .expect("the marker");
            lore_revision::global::external_dir::setting::Setting {
                path: String::new(),
                id,
                generation: std::num::NonZeroU64::MIN,
            }
            .save(&global)
            .await
            .expect("the setting");
        });
        assert_eq!(abandon(), refused, "an available root");
        assert!(!target.exists());
    }

    /// `--adopt` with `--abandon` is refused with `InvalidArguments`.
    #[test]
    #[serial]
    fn adopt_with_abandon_is_refused() {
        let settings = machine_settings("service-api-set-external-both-");

        let status = lore::runtime().block_on(set_external_directory(
            globals(),
            settings.path().join("target").to_str().expect("UTF-8"),
            (true, true),
            std::future::pending(),
        ));

        assert_eq!(
            status,
            code_of(lore_base::error::InvalidArguments {
                reason: String::new()
            })
        );
    }

    /// A relative path resolves against the caller's working directory, and the root moves there,
    /// with no service running.
    #[test]
    #[serial]
    fn a_relative_target_resolves_against_the_caller() {
        let settings = machine_settings("service-api-set-external-relative-");
        let global = global_data_dir(&settings);
        let name = default_root_with_an_instance(&global);
        let caller = settings.path().join("caller");
        std::fs::create_dir(&caller).expect("the caller's directory");
        let at_caller = LoreGlobalArgs {
            working_directory: LoreString::from(caller.to_str().expect("UTF-8")),
            ..globals()
        };

        let status = lore::runtime().block_on(set_external_directory(
            at_caller,
            "moved",
            (false, false),
            std::future::pending(),
        ));

        assert_eq!(status, 0);
        assert!(caller.join("moved").join(&name).join("file").exists());
        assert!(
            !socket_is_accepting(),
            "no service was running, so none starts"
        );
    }

    /// A relative path that steps up resolves to a path with no `.` or `..` left, which the
    /// setting names.
    #[test]
    #[serial]
    fn a_relative_target_that_steps_up_is_set_without_its_steps() {
        let settings = machine_settings("service-api-set-external-step-up-");
        let global = global_data_dir(&settings);
        let name = default_root_with_an_instance(&global);
        let caller = settings.path().join("caller");
        std::fs::create_dir(&caller).expect("the caller's directory");
        let at_caller = LoreGlobalArgs {
            working_directory: LoreString::from(caller.to_str().expect("UTF-8")),
            ..globals()
        };

        let status = lore::runtime().block_on(set_external_directory(
            at_caller,
            &format!(
                "..{}.{}moved",
                std::path::MAIN_SEPARATOR,
                std::path::MAIN_SEPARATOR
            ),
            (false, false),
            std::future::pending(),
        ));

        assert_eq!(status, 0);
        let moved = settings.path().join("moved");
        assert!(moved.join(&name).join("file").exists());
        let setting = lore_revision::global::external_dir::setting::Setting::load(&global)
            .expect("read the setting")
            .expect("the setting");
        assert_eq!(setting.path, moved.display().to_string());
    }

    /// A cancel while the final switch waits for a held store undoes the move.
    #[test]
    #[serial]
    fn a_cancelled_set_external_directory_undoes_the_move() {
        use lore_revision::test_util::MoveStep;

        let settings = machine_settings("service-api-set-external-cancel-");
        let global = global_data_dir(&settings);
        let name = default_root_with_an_instance(&global);
        let remote = "lore://set-external-directory.invalid/repository";
        let stores = settings.path().join("stores");
        let dot_lore = global.join("external").join(&name).join(".lore");
        std::fs::create_dir_all(&dot_lore).expect("the external directory's .lore");
        std::fs::write(
            dot_lore.join("config.toml"),
            format!(
                "remote_url = \"{remote}\"\n[shared_store_to_use]\nuse_shared_store = true\n\
                 shared_store_path = {stores:?}\n"
            ),
        )
        .expect("the config");
        let store = stores
            .join(lore_revision::shared_store::shared_store_subdir_for_remote(
                remote,
            ))
            .join(lore_revision::shared_store::SHARED_STORE_DIR);
        let lock_file = store.join("immutable").join("lock");
        std::fs::create_dir_all(store.join("immutable")).expect("the store");
        std::fs::create_dir_all(store.join("mutable")).expect("the store");
        let target = settings.path().join("target");

        let status = lore::runtime().block_on(async {
            let (reached, mut steps) = tokio::sync::mpsc::unbounded_channel::<(
                MoveStep,
                tokio::sync::oneshot::Sender<bool>,
            )>();
            let (cancel, cancelled) = tokio::sync::oneshot::channel::<()>();
            let answering = tokio::spawn(async move {
                let mut held = None;
                let mut cancel = Some(cancel);
                while let Some((step, answer)) = steps.recv().await {
                    if step
                        == MoveStep::Phase(
                            lore_revision::global::external_dir::record::MovePhase::Identify,
                        )
                    {
                        held = lore_base::fs::lock::FSLock::try_acquire_exact_path(&lock_file)
                            .await
                            .expect("look at the flock");
                        assert!(held.is_some(), "the store is free to hold");
                        if let Some(cancel) = cancel.take() {
                            let _ = cancel.send(());
                        }
                    }
                    let _ = answer.send(true);
                }
                drop(held);
            });
            let status = lore_revision::test_util::step_moves(
                reached,
                set_external_directory(
                    globals(),
                    target.to_str().expect("UTF-8"),
                    (false, false),
                    async {
                        let _ = cancelled.await;
                    },
                ),
            )
            .await;
            answering.abort();
            status
        });

        assert_ne!(status, 0, "the cancelled move fails");
        assert!(!target.exists());
        assert!(!global.join("external.move").exists());
        assert!(global.join("external").join(name).join("file").exists());
    }

    /// On Windows, with no service running, the configured service executable is probed, and one
    /// that doesn't run the command's subcommand refuses the move, in a dry run too.
    #[cfg(target_os = "windows")]
    #[test]
    #[serial]
    fn a_configured_executable_without_the_subcommand_refuses_the_move() {
        let settings = machine_settings("service-api-set-external-probe-");
        let global = global_data_dir(&settings);
        default_root_with_an_instance(&global);
        let system_root = std::env::var("SystemRoot").expect("SystemRoot");
        let older = std::path::Path::new(&system_root)
            .join("System32")
            .join("whoami.exe");
        // Safety: `#[serial]` on every test in this target, and the runtime is not reading the
        // environment concurrently.
        unsafe { std::env::set_var(EXECUTABLE_VAR, &older) };
        let dry_run = LoreGlobalArgs {
            dry_run: 1,
            ..globals()
        };

        let status = lore::runtime().block_on(set_external_directory(
            dry_run,
            settings.path().join("target").to_str().expect("UTF-8"),
            (false, false),
            std::future::pending(),
        ));
        // Safety: as above.
        unsafe { std::env::remove_var(EXECUTABLE_VAR) };

        assert_ne!(status, 0, "an executable without the subcommand is refused");
    }
}
