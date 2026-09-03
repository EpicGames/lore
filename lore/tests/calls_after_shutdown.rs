// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A call that arrives after `lore_shutdown()` fails. It does not hang.
//!
//! This test is in its own binary because shutdown is terminal for the process.
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;

use lore::interface::LoreRepositoryStatusArgs;
use lore::interface::LoreString;
use lore_error_set::FfiError;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreEventCallbackConfig;
use lore_revision::interface::LoreGlobalArgs;

/// Long enough that a loaded machine does not report a hang, short enough that
/// a real hang does not wait out the harness timeout.
const CALL_WAIT: Duration = Duration::from_secs(30);

/// The events the C callback received, in order, as `"Complete(status)"` and
/// `"End"`. A `static` because the callback is a real `extern "C"` function
/// pointer and this binary runs one test.
static EVENTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

unsafe extern "C" fn record(event: &LoreEvent, _user_context: u64) {
    let recorded = match event {
        LoreEvent::Complete(data) => format!("Complete({})", data.status),
        LoreEvent::End(_) => "End".to_string(),
        _ => return,
    };
    EVENTS.lock().unwrap().push(recorded);
}

fn callback() -> LoreEventCallbackConfig {
    LoreEventCallbackConfig {
        user_context: 0,
        func: Some(record),
    }
}

fn status_args() -> LoreRepositoryStatusArgs {
    LoreRepositoryStatusArgs {
        staged: 0,
        scan: 0,
        check_dirty: 0,
        reset: 0,
        sync_point: 0,
        revision_only: 1,
        count: 0,
        paths: LoreArray::default(),
    }
}

/// Run `call` on its own thread and return what it returned, failing the test
/// rather than hanging it if the call never comes back.
fn call_with_deadline<T: Send + 'static>(
    what: &str,
    call: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name(format!("call-{what}"))
        .spawn(move || {
            let _ = sender.send(call());
        })
        .expect("the calling thread must spawn");

    receiver
        .recv_timeout(CALL_WAIT)
        .unwrap_or_else(|_| panic!("{what} never returned after shutdown; it must fail, not hang"))
}

fn drain_events() -> Vec<String> {
    std::mem::take(&mut *EVENTS.lock().unwrap())
}

#[test]
fn calls_after_shutdown_fail_instead_of_hanging() {
    let expected = lore_base::error::ShutDown.ffi_code();
    assert_ne!(expected, 0, "the shut-down status must not read as success");

    // Build the core runtime for real before tearing it down. Under an ambient
    // tokio runtime every call would resolve `Handle::try_current()` to that one
    // instead, `CORE_RUNTIME` would stay unbuilt, and `runtime_shutdown_timeout`
    // would return without shutting anything down, so the test would not be
    // testing a shut-down library at all.
    let warmup = lore::interface::lore_repository_status(
        &LoreGlobalArgs::default(),
        &status_args(),
        callback(),
    );
    assert_ne!(
        warmup, expected,
        "the warm-up call must run against a live runtime"
    );
    drain_events();

    assert!(lore::shutdown(), "the first shutdown must run the teardown");

    // A synchronous call reports on both surfaces: the return value and the
    // `Complete` event, which agree.
    let returned = call_with_deadline("lore_repository_status", || {
        lore::interface::lore_repository_status(
            &LoreGlobalArgs::default(),
            &status_args(),
            callback(),
        )
    });
    assert_eq!(returned, expected, "the synchronous call must fail");
    assert_eq!(
        drain_events(),
        [format!("Complete({expected})"), "End".to_string()],
        "the synchronous call must report the failure and end the stream"
    );

    // The asynchronous entry point returns nothing, so the `Complete` event is
    // the only channel for the status.
    call_with_deadline("lore_repository_status_async", || {
        lore::interface::lore_repository_status_async(
            &LoreGlobalArgs::default(),
            &status_args(),
            callback(),
        );
    });
    assert_eq!(
        drain_events(),
        [format!("Complete({expected})"), "End".to_string()],
        "the asynchronous call must report the failure through Complete"
    );

    // A call that would have failed argument validation fails as shut down
    let returned = call_with_deadline("lore_repository_status (invalid text)", || {
        lore::interface::lore_repository_status(
            &LoreGlobalArgs {
                repository_path: LoreString::from_bytes(&[b'a', 0xff, 0xfe]),
                ..LoreGlobalArgs::default()
            },
            &status_args(),
            callback(),
        )
    });
    assert_eq!(
        returned, expected,
        "a malformed call must fail as shut down rather than hang in the rejection path"
    );
    assert_eq!(
        drain_events(),
        [format!("Complete({expected})"), "End".to_string()]
    );

    // Shutting down again fails as well. Every repeat reports the same status,
    // and concurrent repeats all lose the claim rather than one of them reading
    // the library as up and reporting success for a teardown it never ran.
    let returned = call_with_deadline("lore_shutdown", || lore::interface::lore_shutdown());
    assert_eq!(returned, expected, "a repeated shutdown must not hang");

    let racers: Vec<_> = (0..8)
        .map(|_| std::thread::spawn(|| lore::interface::lore_shutdown()))
        .collect();
    for racer in racers {
        assert_eq!(
            racer.join().expect("the racing caller joins"),
            expected,
            "no concurrent repeat may report success"
        );
    }
}
