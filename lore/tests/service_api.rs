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
    use std::sync::LazyLock;

    use lore::interface::LoreString;
    use lore::service::LoreServiceSetExecutableArgs;
    use lore::service::LoreServiceSetUseAutomaticallyArgs;
    use lore::service::LoreServiceStopArgs;
    use lore_base::error::ServiceUnavailable;
    use lore_error_set::FfiError;
    use lore_revision::interface::LoreGlobalArgs;
    use rand::distr::Alphanumeric;
    use rand::distr::SampleString;
    use serial_test::serial;

    use super::test_util::TempDir;

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
            std::env::remove_var("LORE_SERVICE_EXECUTABLE");
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

    /// Both settings are stored, and both are read back by the decision the
    /// dispatch makes. Written through the API rather than into the file, since
    /// what an embedder can set is the point.
    #[test]
    #[serial]
    fn both_settings_together_turn_relaying_on() {
        let _settings = machine_settings("service-api-both-settings-");

        lore::runtime().block_on(async {
            assert_eq!(set_use_automatically(true).await, 0);
            assert!(
                !lore::will_use_service(),
                "the setting alone must not relay: the executable is unnamed, so \
                 the version serving the machine would be whichever program \
                 relayed first"
            );

            assert_eq!(set_executable("/opt/lore/1.9/bin/lore").await, 0);
            assert!(lore::will_use_service(), "with both named, calls relay");
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

    /// Clearing the executable turns relaying off again, and the setter forgets
    /// the decision this process already made so that the change takes effect
    /// without it being restarted.
    #[test]
    #[serial]
    fn clearing_the_executable_turns_relaying_off_within_the_process() {
        let _settings = machine_settings("service-api-clear-executable-");

        lore::runtime().block_on(async {
            assert_eq!(set_use_automatically(true).await, 0);
            assert_eq!(set_executable("/opt/lore/1.9/bin/lore").await, 0);
            assert!(lore::will_use_service(), "both are named");

            assert_eq!(set_executable("").await, 0);
            assert!(
                !lore::will_use_service(),
                "clearing the executable must take effect in this process, not \
                 only in the next one"
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

        let unavailable = ServiceUnavailable {
            reason: String::new(),
        }
        .ffi_code();
        assert_eq!(
            status, unavailable,
            "an unreachable service must report as one, not as the call having \
             run and failed"
        );
    }
}
