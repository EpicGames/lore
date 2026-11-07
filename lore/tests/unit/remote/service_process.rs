// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use lore::remote::service_process::*;

#[test]
fn no_executable_set_reports_both_places_to_set_one() {
    let error = resolve_service_executable(None, None).expect_err("no executable set must fail");

    assert!(
        error.to_string().contains(SERVICE_EXECUTABLE_SETTING)
            && error.to_string().contains(SERVICE_EXECUTABLE_VAR),
        "the failure must name both places one can be set: {error}"
    );
}

#[test]
fn the_executable_from_config_resolves() {
    let from_config = "/opt/lore/1.9/bin/lore";

    let resolved = resolve_service_executable(None, Some(from_config))
        .expect("the executable from config resolves");

    assert_eq!(resolved, PathBuf::from(from_config));
}

/// A config value of nothing but space would otherwise be started as a path
/// made of spaces, which fails with an error about a file no one set.
#[test]
fn a_config_value_of_only_space_is_treated_as_unset() {
    let error =
        resolve_service_executable(None, Some("   ")).expect_err("a blank config value must fail");

    assert!(
        error.to_string().contains(SERVICE_EXECUTABLE_SETTING),
        "the failure must name where one can be set: {error}"
    );
}

/// What [`executable_to_start`] picks with no executable named: this program's own only for a
/// start that allows it, in a program that runs the service, and the naming failure otherwise.
#[test]
fn with_none_named_only_a_start_that_allows_it_uses_a_program_that_serves() {
    let own = PathBuf::from("/opt/lore/bin/lore");
    for (start, serves, starts_own) in [
        (ServiceStart::NamedOrThisProgram, true, true),
        (ServiceStart::NamedOrThisProgram, false, false),
        (ServiceStart::Named, true, false),
        (ServiceStart::Named, false, false),
    ] {
        let none_named = resolve_service_executable(None, None);
        let picked = executable_to_start(none_named, start, serves, || Ok(own.clone()));

        if starts_own {
            let picked = picked.unwrap_or_else(|error| panic!("{start:?} starts its own: {error}"));
            assert_eq!(picked, own);
        } else {
            let error = picked.expect_err("no executable to start");
            assert!(
                error.to_string().contains(SERVICE_EXECUTABLE_SETTING),
                "{start:?}, serving {serves}: the naming failure, got {error}"
            );
        }
    }
}

/// A named executable is the one started, whatever program asks, and this program's own is not
/// looked up then.
#[test]
fn a_named_executable_is_started_in_place_of_this_program() {
    let named = PathBuf::from("/opt/lore/1.9/bin/lore");
    for start in [ServiceStart::Named, ServiceStart::NamedOrThisProgram] {
        let picked = executable_to_start(Ok(named.clone()), start, true, || {
            panic!("this program's executable is not looked up when one is named")
        });

        assert_eq!(picked.expect("the named executable"), named);
    }
}

/// A program that serves but cannot find its own executable fails as a start does that has none,
/// naming where one can be set.
#[test]
fn a_program_that_cannot_find_its_own_executable_reports_it_as_unavailable() {
    let picked = executable_to_start(
        resolve_service_executable(None, None),
        ServiceStart::NamedOrThisProgram,
        true,
        || Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
    );

    let error = picked.expect_err("no executable to start");
    assert!(
        error.is_service_unavailable()
            && error
                .to_string()
                .contains("finding this program's executable failed")
            && error.to_string().contains(SERVICE_EXECUTABLE_SETTING),
        "the failure and where to name one instead: {error}"
    );
}

#[test]
fn an_environment_value_that_reads_as_off_turns_relaying_off() {
    // The obvious way to disable something must not enable it.
    for off in ["0", "false", "no", "off", "OFF", "False", " 0 "] {
        assert!(
            !relaying_is_asked_for(Some(OsString::from(off)), true),
            "{off} must turn relaying off even where the config turns it on"
        );
    }
}

#[test]
fn an_environment_value_that_reads_as_on_turns_relaying_on() {
    for on in ["1", "true", "yes", "on", "anything"] {
        assert!(
            relaying_is_asked_for(Some(OsString::from(on)), false),
            "{on} must turn relaying on even where the config leaves it off"
        );
    }
}

#[test]
fn no_environment_value_leaves_the_config_to_decide() {
    assert!(
        relaying_is_asked_for(None, true),
        "the config turns relaying on"
    );
    assert!(!relaying_is_asked_for(None, false), "and off");
    // Blank reads as unset, as a blank config value does.
    assert!(relaying_is_asked_for(Some(OsString::from("   ")), true));
    assert!(!relaying_is_asked_for(Some(OsString::new()), false));
}

#[test]
fn an_empty_config_value_is_treated_as_unset() {
    let error =
        resolve_service_executable(None, Some("")).expect_err("an empty config value must fail");

    assert!(
        error.to_string().contains(SERVICE_EXECUTABLE_SETTING),
        "the failure must name where one can be set: {error}"
    );
}

#[test]
fn the_executable_from_environment_overrides_the_config() {
    // One call, one build under test, without editing what the machine configures.
    let from_env = PathBuf::from("/home/dev/lore/target/debug/lore");

    let resolved = resolve_service_executable(
        Some(from_env.clone().into_os_string()),
        Some("/opt/lore/1.9/bin/lore"),
    )
    .expect("the executable from environment resolves");

    assert_eq!(resolved, from_env);
}

#[test]
fn the_executable_from_environment_is_used_as_it_stands() {
    let from_env = PathBuf::from("/opt/tools/lore-for-the-service");

    let resolved = resolve_service_executable(Some(from_env.clone().into_os_string()), None)
        .expect("the executable from environment resolves");

    assert_eq!(resolved, from_env);
}

#[test]
fn an_empty_environment_value_falls_back_to_the_config() {
    let from_config = "/opt/lore/bin/lore";

    let resolved = resolve_service_executable(Some(OsString::new()), Some(from_config))
        .expect("an empty environment value falls back to the config");

    assert_eq!(resolved, PathBuf::from(from_config));
}

#[test]
fn an_empty_environment_value_with_no_config_fails() {
    let error = resolve_service_executable(Some(OsString::new()), None)
        .expect_err("an empty environment value with no config must fail");

    assert!(
        error.to_string().contains(SERVICE_EXECUTABLE_SETTING),
        "the failure must name where one can be set: {error}"
    );
}

#[tokio::test]
async fn a_stop_requested_before_the_wait_still_ends_it() {
    let request = Arc::new(StopRequest::new());
    assert!(request.begin_start());
    request.set_running();
    let wait = ServiceStopRequest {
        request: Arc::clone(&request),
    };

    assert!(request.request_stop());

    wait.requested().await;
}

#[tokio::test]
async fn a_stop_requested_during_the_wait_ends_it() {
    let request = Arc::new(StopRequest::new());
    assert!(request.begin_start());
    request.set_running();
    let wait = ServiceStopRequest {
        request: Arc::clone(&request),
    };

    // Both run on this task: the wait parks first, then the request lands.
    tokio::join!(wait.requested(), async {
        tokio::task::yield_now().await;
        assert!(request.request_stop());
    });
}

#[test]
fn a_stop_asked_of_an_earlier_service_is_cleared_by_the_next_start() {
    let request = StopRequest::new();
    assert!(request.begin_start());
    assert!(request.request_stop());
    request.end_service();

    assert!(request.begin_start());
    assert!(
        !request.is_requested(),
        "a service started after one has stopped must not be stopped on arrival"
    );
}

#[test]
fn a_stop_after_the_start_began_is_kept() {
    let request = StopRequest::new();
    assert!(request.begin_start());

    // The service is starting and not yet running, as while it mounts.
    assert!(request.request_stop());
    assert!(request.is_requested());
}

#[test]
fn a_second_start_is_refused_and_leaves_the_first_alone() {
    let request = StopRequest::new();
    assert!(request.begin_start());
    request.set_running();
    assert!(request.request_stop());

    assert!(!request.begin_start(), "one service runs per process");
    assert!(request.is_running());
    assert!(
        request.is_requested(),
        "a refused start must not clear a stop asked of the running service"
    );
}

/// A stop that reports a service to ask is never lost to the start publishing it:
/// start and stop race, and whichever order they land in, a stop that was
/// accepted is still requested once the start has finished.
#[test]
fn an_accepted_stop_is_never_cleared_by_a_racing_start() {
    for _ in 0..2000 {
        let request = Arc::new(StopRequest::new());
        let barrier = Arc::new(std::sync::Barrier::new(2));

        let stopper = {
            let (request, barrier) = (Arc::clone(&request), Arc::clone(&barrier));
            std::thread::spawn(move || {
                barrier.wait();
                request.request_stop()
            })
        };
        barrier.wait();
        assert!(request.begin_start());
        let accepted = stopper.join().unwrap();

        assert_eq!(
            request.is_requested(),
            accepted,
            "a stop reported as accepted must survive the start"
        );
    }
}

/// The session a process belongs to, read from `/proc`. `comm` can hold
/// spaces and parentheses, so the fields after it are counted from the last
/// `)`: state, ppid, pgrp, then session.
#[cfg(target_os = "linux")]
fn session_of(pid: u32) -> i32 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    stat.rsplit_once(')')
        .map(|(_comm, rest)| rest)
        .unwrap_or_default()
        .split_whitespace()
        .nth(3)
        .and_then(|session| session.parse().ok())
        .unwrap_or(-1)
}

/// A service started here is this process's child until it is collected, so
/// one that has exited must not be left in the process table.
#[cfg(target_family = "unix")]
#[test]
fn a_started_service_that_has_exited_is_collected() {
    let mut started = Command::new("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("a stand-in process must start");
    let pid = started.id();
    // Wait for it to exit without collecting it, which is the state a
    // dropped handle would leave behind.
    while !matches!(started.try_wait(), Ok(Some(_))) {
        std::thread::sleep(Duration::from_millis(10));
    }

    // A collected process is no longer waitable, so a second wait on the pid
    // is what tells the two apart.
    remember_started_service(started);
    collect_exited_services();

    // Safety: waits on a pid this test started, writing only to `status`.
    let waited = unsafe {
        let mut status = 0;
        libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG)
    };
    assert_eq!(
        waited, -1,
        "the process must already have been collected, leaving nothing to wait on"
    );
}

/// A service has to outlive the terminal that started it, which means
/// leaving the caller's session rather than being signalled along with it.
/// Read from the started process rather than from the flags asked for, since
/// the flags being set is not the property that matters.
#[cfg(target_os = "linux")]
#[test]
fn a_started_service_leaves_the_callers_session() {
    let mut command = Command::new("sleep");
    command
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach_from_caller(&mut command);

    let mut started = command.spawn().expect("a stand-in process must start");
    let started_session = session_of(started.id());
    let _ = started.kill();
    let _ = started.wait();

    // Safety: reads no memory through pointers and cannot fail for 0.
    let our_session = unsafe { libc::getsid(0) };

    assert_ne!(started_session, -1, "the session must be readable");
    assert_ne!(
        started_session, our_session,
        "a started service must lead a session of its own"
    );
}

#[test]
fn a_process_running_no_service_has_no_stop_to_request() {
    // No test registers a service process, so nothing recorded a request.
    assert!(!service_runs_in_this_process());
    assert!(!request_service_stop());
}
