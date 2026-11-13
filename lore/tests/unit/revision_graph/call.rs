// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use lore::interface::LoreEventCallback;
use lore::interface::LoreGlobalArgs;
use lore::revision_graph::RevisionGraphError;
use lore::revision_graph::call::revision_graph_call;
use lore::storage::close::LoreStorageCloseArgs;
use lore::storage::close::close;
use lore::storage::handle;
use lore::storage::handle::LoreStore;
use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_revision::auth::LoreAuthPendingEventData;
use lore_revision::auth::LoreAuthUrlEventData;
use lore_revision::branch::LoreBranchCreateEventData;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreErrorDetail;
use lore_revision::event::LoreEvent;
use lore_revision::event::LoreLogEventData;
use lore_revision::event::revision_graph::LoreRevisionGraphBranchEventData;
use lore_revision::event::revision_graph::LoreRevisionGraphInstanceEventData;
use lore_revision::event::revision_graph::LoreRevisionGraphLocationCompleteEventData;
use lore_revision::event::revision_graph::LoreRevisionGraphRevisionEventData;
use lore_revision::instance::InstanceId;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::interface::LoreString;
use lore_revision::logging::LoreLogLevel;
use lore_revision::lore::execution_context;
use lore_revision::relay::EventDispatcher;
use tokio::sync::oneshot;

use crate::call::test_support::recording_callback;
use crate::call::test_support::registered_store;
use crate::call::test_support::without_log_lines;

/// The `location` of the log line the command sends. Where a process-wide log callback is
/// installed, the call's own log lines reach the callback too, so [`kinds`] counts only this one.
const COMMAND_LOG_LOCATION: &str = "revision graph call test";

/// The kinds of `events`, in order, without the log lines the command didn't send.
fn kinds(events: &Mutex<Vec<LoreEvent>>) -> Vec<&'static str> {
    events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            LoreEvent::Log(data) if data.location.as_str() != COMMAND_LOG_LOCATION => None,
            LoreEvent::Log(_) => Some("log"),
            LoreEvent::AuthUrl(_) => Some("auth url"),
            LoreEvent::AuthPending(_) => Some("auth pending"),
            LoreEvent::RevisionGraphBranch(_) => Some("branch"),
            LoreEvent::RevisionGraphInstance(_) => Some("instance"),
            LoreEvent::RevisionGraphRevision(_) => Some("revision"),
            LoreEvent::RevisionGraphLocationComplete(_) => Some("location complete"),
            LoreEvent::Complete(_) => Some("complete"),
            LoreEvent::End(_) => Some("end"),
            _ => Some("other"),
        })
        .collect()
}

/// One event of each kind a revision graph call passes, after the events it drops: one a core
/// sends and those of an interactive login.
fn events_a_verb_sends() -> [LoreEvent; 8] {
    [
        LoreEvent::BranchCreate(LoreBranchCreateEventData {
            name: LoreString::from("core"),
            latest: Hash::default(),
            is_commit: 0,
        }),
        LoreEvent::AuthUrl(LoreAuthUrlEventData {
            url: LoreString::default(),
            user_code: LoreString::default(),
        }),
        LoreEvent::AuthPending(LoreAuthPendingEventData {
            elapsed_secs: 0,
            interval_secs: 0,
            remaining_secs: 0,
        }),
        LoreEvent::Log(LoreLogEventData {
            location: LoreString::from(COMMAND_LOG_LOCATION),
            ..EventDispatcher::make_log(LoreLogLevel::Info, "graph".to_string())
        }),
        LoreEvent::RevisionGraphBranch(LoreRevisionGraphBranchEventData {
            id: 0,
            location: LoreBranchLocation::Local,
            branch: BranchId::default(),
            name: LoreString::default(),
            category: LoreString::default(),
            latest: Hash::default(),
            stack: LoreArray::default(),
            creator: LoreString::default(),
            created: 0,
            protect: 0,
            archived: 0,
            divergent: 0,
        }),
        LoreEvent::RevisionGraphInstance(LoreRevisionGraphInstanceEventData {
            id: 0,
            location: LoreBranchLocation::Local,
            instance_id: InstanceId::default(),
            path: LoreString::default(),
            branch: BranchId::default(),
            revision: Hash::default(),
            stale: 0,
        }),
        LoreEvent::RevisionGraphRevision(LoreRevisionGraphRevisionEventData {
            id: 0,
            location: LoreBranchLocation::Local,
            revision_hash: Hash::default(),
            revision_number: 0,
            branch: BranchId::default(),
        }),
        LoreEvent::RevisionGraphLocationComplete(LoreRevisionGraphLocationCompleteEventData {
            id: 0,
            location: LoreBranchLocation::Local,
            records: 0,
            error: LoreErrorDetail::default(),
        }),
    ]
}

/// The call's own events and its log lines reach the callback, and neither an event a core sends
/// nor an interactive login's does; `Complete` and `End` follow them.
#[tokio::test]
async fn call_drops_core_events_and_passes_log_and_complete() {
    let (_store, store_handle) = registered_store("graph-call-events").await;
    let (events, callback) = recording_callback();

    let status = revision_graph_call(
        LoreGlobalArgs::default(),
        callback,
        store_handle,
        (),
        "graph_call_events",
        |_store, _args: ()| async move {
            for event in events_a_verb_sends() {
                event.send();
            }
            Ok::<_, RevisionGraphError>(())
        },
    )
    .await;
    handle::unregister(store_handle);

    assert_eq!(status, 0);
    assert_eq!(
        kinds(&events),
        [
            "log",
            "branch",
            "instance",
            "revision",
            "location complete",
            "complete",
            "end",
        ]
    );
}

/// An unknown handle completes the call with `InvalidArguments` without running the command.
#[tokio::test]
async fn call_completes_invalid_arguments_on_an_unknown_handle() {
    let (events, callback) = recording_callback();
    let ran = Arc::new(Mutex::new(false));
    let command_ran = ran.clone();

    let status = revision_graph_call(
        LoreGlobalArgs::default(),
        callback,
        LoreStore::INVALID,
        (),
        "graph_call_unknown_handle",
        move |_store, _args: ()| async move {
            *command_ran.lock().unwrap() = true;
            Ok::<_, RevisionGraphError>(())
        },
    )
    .await;

    let expected = LoreErrorCode::InvalidArguments as i32;
    assert_eq!(status, expected);
    assert!(!*ran.lock().unwrap(), "the command must not run");
    let recorded = events.lock().unwrap();
    let events = without_log_lines(&recorded);
    let [LoreEvent::Complete(complete), LoreEvent::End(_)] = events.as_slice() else {
        panic!("expected Complete and End, got {} events", events.len());
    };
    assert_eq!(
        (complete.status, complete.error.error_code),
        (expected, expected)
    );
}

/// The command sees `dry_run` and `force` cleared and the rest of the globals as given. The
/// globals carry a correlation id, since an execution context given none makes one.
#[tokio::test]
async fn the_command_runs_with_dry_run_and_force_cleared() {
    let (_store, store_handle) = registered_store("graph-call-globals").await;
    let given = LoreGlobalArgs {
        dry_run: 1,
        force: 1,
        offline: 1,
        correlation_id: LoreString::from("graph-call-globals"),
        ..LoreGlobalArgs::default()
    };
    let seen = Arc::new(Mutex::new(None));
    let slot = seen.clone();

    let status = revision_graph_call(
        given.clone(),
        None,
        store_handle,
        (),
        "graph_call_globals",
        move |_store, _args: ()| async move {
            *slot.lock().unwrap() = Some(execution_context().globals().clone());
            Ok::<_, RevisionGraphError>(())
        },
    )
    .await;
    handle::unregister(store_handle);

    assert_eq!(status, 0);
    assert_eq!(
        *seen.lock().unwrap(),
        Some(LoreGlobalArgs {
            dry_run: 0,
            force: 0,
            ..given
        })
    );
}

/// `lore_storage_close` waits for a revision graph call in flight on the handle. The call's
/// `Complete` reaches its callback while the call still counts as in flight, and close's after the
/// count drains.
#[allow(clippy::disallowed_methods)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_close_waits_for_an_in_flight_graph_call() {
    let (store, store_handle) = registered_store("graph-call-close").await;
    let completed: Arc<Mutex<Vec<(&'static str, u64)>>> = Arc::default();
    let on_complete = |name: &'static str| -> LoreEventCallback {
        let completed = completed.clone();
        let store = store.clone();
        Some(Box::new(move |event: &LoreEvent| {
            if matches!(event, LoreEvent::Complete(_)) {
                let in_flight = store.in_flight.load(Ordering::Acquire);
                completed.lock().unwrap().push((name, in_flight));
            }
        }))
    };
    let (entered, command_entered) = oneshot::channel::<()>();
    let (release, command_released) = oneshot::channel::<()>();

    let call = tokio::spawn(revision_graph_call(
        LoreGlobalArgs::default(),
        on_complete("graph call"),
        store_handle,
        (),
        "graph_call_close",
        move |_store, _args: ()| async move {
            let _ = entered.send(());
            let _ = command_released.await;
            Ok::<_, RevisionGraphError>(())
        },
    ));
    command_entered.await.expect("the command runs");

    let mut closing = tokio::spawn(close(
        LoreGlobalArgs::default(),
        LoreStorageCloseArgs {
            handle: store_handle,
            ..Default::default()
        },
        on_complete("close"),
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !store.invalid.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "close never marked the store invalid"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut closing)
            .await
            .is_err(),
        "close must wait while the graph call is in flight"
    );

    release.send(()).expect("the command waits for its release");
    assert_eq!(call.await.expect("the graph call task"), 0);
    assert_eq!(closing.await.expect("the close task"), 0);
    assert_eq!(
        *completed.lock().unwrap(),
        [("graph call", 1), ("close", 0)]
    );
}
