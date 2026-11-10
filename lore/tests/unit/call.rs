// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore::interface::LoreEventCallback;
use lore_base::error::RepositoryNotFound;
use lore_revision::event::EventError;
use lore_revision::repository::RepositoryError;

pub(crate) mod test_support;

use std::sync::Mutex;

use lore::call::*;
use lore_base::error::NotFound;
use lore_error_set::Location;
use lore_error_set::prelude::*;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreGlobalArgs;

// A concrete `#[error_set]` error for the wrapper closures. Its `NotFound`
// variant wraps `lore_base::error::NotFound`, which carries error code 79, so
// the failure path has a known non-internal code to assert against.
#[error_set]
enum SampleError {
    NotFound,
}

impl EventError for SampleError {}

// The full event a callback receives, kept verbatim so a test can read the
// `Complete` detail (code, message, trace) a real consumer would see.
#[derive(Clone)]
enum CapturedEvent {
    Error,
    Complete(lore_revision::event::LoreCompleteEventData),
    Other,
}

impl CapturedEvent {
    fn from_event(event: &LoreEvent) -> Self {
        match event {
            LoreEvent::Error(_) => Self::Error,
            LoreEvent::Complete(data) => Self::Complete(data.clone()),
            _ => Self::Other,
        }
    }
}

fn make_callback(sink: Arc<Mutex<Vec<CapturedEvent>>>) -> LoreEventCallback {
    Some(Box::new(move |event: &LoreEvent| {
        sink.lock().unwrap().push(CapturedEvent::from_event(event));
    }))
}

#[tokio::test]
async fn failing_op_completes_with_error_code_and_no_error_event() {
    let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let status = no_repository_call(
        LoreGlobalArgs::default(),
        make_callback(sink.clone()),
        (),
        "failing_op",
        |()| async move { Err::<(), SampleError>(NotFound.into()) },
    )
    .await;

    let events = sink.lock().unwrap().clone();

    assert!(
        !events.iter().any(|e| matches!(e, CapturedEvent::Error)),
        "no Error event must be emitted on terminal failure"
    );

    // Exactly one `Complete` event.
    let completes: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            CapturedEvent::Complete(data) => Some(data.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(completes.len(), 1, "exactly one Complete event");

    // The status holds the error code (79 for `NotFound`) and the detail is
    // populated with that code and the error's message.
    let data = &completes[0];
    let expected_code = SampleError::from(NotFound).ffi_code();
    // The synchronous return equals the error code, matching `Complete.status`.
    assert_eq!(status, expected_code);
    assert_eq!(data.status, expected_code);
    assert_eq!(data.error.error_code, expected_code);
    assert_eq!(
        data.error.message.as_str(),
        SampleError::from(NotFound).to_string()
    );
}

#[tokio::test]
async fn succeeding_op_completes_with_status_zero_and_empty_detail() {
    let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let status = no_repository_call(
        LoreGlobalArgs::default(),
        make_callback(sink.clone()),
        (),
        "succeeding_op",
        |()| async move { Ok::<(), SampleError>(()) },
    )
    .await;

    assert_eq!(status, 0);

    let events = sink.lock().unwrap().clone();
    let completes: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            CapturedEvent::Complete(data) => Some(data.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(completes.len(), 1, "exactly one Complete event");

    let data = &completes[0];
    assert_eq!(data.status, 0);
    assert_eq!(data.error.error_code, 0);
    assert!(data.error.message.is_empty());
    assert!(data.error.trace_locations.is_empty());
}

#[tokio::test]
async fn missing_repository_completes_with_real_ffi_code() {
    // A path with no repository fails in `prepare_repository_call` before
    // any command runs. The failure must report the real
    // repository-not-found error code (45), not the old flat `1`.
    let temp = std::env::temp_dir().join(format!(
        "lore-missing-repo-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let globals = LoreGlobalArgs {
        repository_path: temp.display().to_string().into(),
        ..LoreGlobalArgs::default()
    };

    let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let status = repository_call_read(
        globals,
        make_callback(sink.clone()),
        (),
        "missing_repo",
        |_repo, ()| async move { Ok::<(), SampleError>(()) },
    )
    .await;

    let events = sink.lock().unwrap().clone();
    let completes: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            CapturedEvent::Complete(data) => Some(data.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(completes.len(), 1, "exactly one Complete event");

    let expected_code = RepositoryError::from(RepositoryNotFound {
        repository: temp.display().to_string(),
    })
    .ffi_code();
    assert_ne!(expected_code, 1, "must report a real code, not the flat 1");
    assert_ne!(expected_code, 0);

    let data = &completes[0];
    // status, the synchronous return, and the detail all carry the real code.
    assert_eq!(status, expected_code);
    assert_eq!(data.status, expected_code);
    assert_eq!(data.error.error_code, expected_code);
    assert!(
        !data.error.message.is_empty(),
        "the detail must carry the error message"
    );
}

#[tokio::test]
async fn failing_op_lets_consumer_read_trace_locations() {
    let sink: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let _status = no_repository_call(
        LoreGlobalArgs::default(),
        make_callback(sink.clone()),
        (),
        "trace_op",
        |()| async move {
            // Add a known trace entry so the consumer can read it back from
            // the `Complete` detail.
            let mut err: SampleError = NotFound.into();
            err.push_trace(Location::with_context(
                "src/trace_op.rs",
                7,
                3,
                std::sync::Arc::from("running trace op"),
            ));
            Err::<(), SampleError>(err)
        },
    )
    .await;

    let events = sink.lock().unwrap().clone();
    let data = events
        .iter()
        .find_map(|e| match e {
            CapturedEvent::Complete(data) => Some(data.clone()),
            _ => None,
        })
        .expect("a Complete event must be emitted");

    // The pushed location is reconstructable from the trace; with
    // `track-locations` on, the `From` conversion prepends its own caller
    // location, so the entry we pushed is the last one.
    let locations = data.error.trace_locations.as_slice();
    assert!(
        !locations.is_empty(),
        "trace must carry at least one location"
    );
    let last = locations.last().unwrap();
    assert_eq!(last.file.as_str(), "src/trace_op.rs");
    assert_eq!(last.line, 7);
    assert_eq!(last.column, 3);
    assert_eq!(last.context.as_str(), "running trace op");
}

/// A new local repository at `root`, as `lore repository create` leaves it.
async fn new_local_repository(root: &std::path::Path) {
    lore_base::runtime::LORE_CONTEXT
        .scope(setup_execution(LoreGlobalArgs::default(), None), async {
            let token = lore_revision::repository::RepositoryWriteToken::acquire(root).await;
            lore_revision::repository::create_local(
                root,
                &token,
                lore_revision::lore::RepositoryId::from(uuid::Uuid::now_v7()),
                lore_base::types::Context::from(uuid::Uuid::now_v7()),
                lore_revision::branch::DEFAULT_DEFAULT_NAME.to_string(),
                lore_revision::repository::RepositoryConfig::default(),
                false,
            )
            .await
            .expect("create a repository");
        })
        .await;
}

fn globals_at(root: &std::path::Path) -> LoreGlobalArgs {
    LoreGlobalArgs {
        repository_path: root.display().to_string().into(),
        ..LoreGlobalArgs::default()
    }
}

/// A plan that answers `None` ends the call with status 0, running neither the command nor the
/// finish.
#[tokio::test]
async fn an_exclusive_call_whose_plan_answers_none_succeeds_without_running_command_or_finish() {
    let root = lore_base::test_util::TempDir::new("lore-call-exclusive-none-");
    new_local_repository(&root).await;
    let ran = Arc::new(Mutex::new(Vec::new()));
    let (command_ran, finish_ran) = (ran.clone(), ran.clone());

    let status = repository_call_exclusive_planned(
        globals_at(&root),
        None,
        (),
        "exclusive_none",
        |_repository, ()| async move { Ok::<Option<()>, SampleError>(None) },
        async move |_repository, _token, _plan: &()| {
            command_ran.lock().unwrap().push("command");
            Ok::<(), SampleError>(())
        },
        move |()| async move { finish_ran.lock().unwrap().push("finish") },
    )
    .await;

    assert_eq!(status, 0);
    assert!(ran.lock().unwrap().is_empty());
}

/// A plan that fails ends the call with its error code, running neither the command nor the
/// finish, as there is no plan to finish.
#[tokio::test]
async fn an_exclusive_call_whose_plan_fails_reports_its_code_without_command_or_finish() {
    let root = lore_base::test_util::TempDir::new("lore-call-exclusive-plan-fails-");
    new_local_repository(&root).await;
    let ran = Arc::new(Mutex::new(Vec::new()));
    let (command_ran, finish_ran) = (ran.clone(), ran.clone());

    let status = repository_call_exclusive_planned(
        globals_at(&root),
        None,
        (),
        "exclusive_plan_fails",
        |_repository, ()| async move { Err::<Option<()>, SampleError>(NotFound.into()) },
        async move |_repository, _token, _plan: &()| {
            command_ran.lock().unwrap().push("command");
            Ok::<(), SampleError>(())
        },
        move |()| async move { finish_ran.lock().unwrap().push("finish") },
    )
    .await;

    assert_eq!(status, SampleError::from(NotFound).ffi_code());
    assert!(ran.lock().unwrap().is_empty());
}

/// The finish runs after the command whether the command succeeds or fails, without the write
/// token, and the call reports the command's error code.
#[tokio::test]
async fn an_exclusive_call_finishes_after_its_command_whatever_it_returns() {
    for fails in [false, true] {
        let root = lore_base::test_util::TempDir::new("lore-call-exclusive-finish-");
        new_local_repository(&root).await;
        let ran = Arc::new(Mutex::new(Vec::new()));
        let (command_ran, finish_ran) = (ran.clone(), ran.clone());
        let token_path = root.to_path_buf();

        let status = repository_call_exclusive_planned(
            globals_at(&root),
            None,
            (),
            "exclusive_finish",
            |_repository, ()| async move { Ok::<Option<u32>, SampleError>(Some(7)) },
            async move |_repository, _token, plan: &u32| {
                assert_eq!(*plan, 7);
                command_ran.lock().unwrap().push("command");
                if fails {
                    Err(SampleError::from(NotFound))
                } else {
                    Ok(())
                }
            },
            move |plan| async move {
                assert_eq!(plan, 7);
                // The write token is free by the time the finish runs, but for the flush that ends
                // the command, which the timeout outlasts.
                let token = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    lore_revision::repository::RepositoryWriteToken::acquire(&token_path),
                )
                .await;
                finish_ran.lock().unwrap().push(if token.is_ok() {
                    "finish"
                } else {
                    "finish while the write token was held"
                });
            },
        )
        .await;

        let expected = if fails {
            SampleError::from(NotFound).ffi_code()
        } else {
            0
        };
        assert_eq!(status, expected, "fails: {fails}");
        assert_eq!(
            *ran.lock().unwrap(),
            vec!["command", "finish"],
            "fails: {fails}"
        );
    }
}

/// A read command on the same instance in the same process, which takes no write token, waits
/// until an exclusive command is done, rather than sharing its hold of the lock.
#[tokio::test]
async fn a_read_call_waits_for_an_exclusive_call_on_the_same_instance() {
    let root = lore_base::test_util::TempDir::new("lore-call-exclusive-wait-");
    new_local_repository(&root).await;
    let order = Arc::new(Mutex::new(Vec::new()));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let release = Arc::new(tokio::sync::Notify::new());

    let exclusive = {
        let (order, release, globals) = (order.clone(), release.clone(), globals_at(&root));
        lore_base::lore_spawn!(async move {
            repository_call_exclusive_planned(
                globals,
                None,
                (),
                "exclusive_holder",
                |_repository, ()| async move { Ok::<Option<()>, SampleError>(Some(())) },
                async move |_repository, _token, _plan: &()| {
                    let _ = started_tx.send(());
                    release.notified().await;
                    order.lock().unwrap().push("exclusive");
                    Ok::<(), SampleError>(())
                },
                |()| async {},
            )
            .await
        })
    };
    started_rx.await.expect("the exclusive command starts");
    let read = {
        let (order, globals) = (order.clone(), globals_at(&root));
        lore_base::lore_spawn!(async move {
            repository_call_read(globals, None, (), "waiting_reader", |_r, ()| async move {
                order.lock().unwrap().push("read");
                Ok::<(), SampleError>(())
            })
            .await
        })
    };
    // Long enough for a read that did not wait to have run. A wait cannot be polled for, since
    // it is the absence of progress.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(order.lock().unwrap().is_empty(), "the read waits");

    release.notify_one();
    assert_eq!(exclusive.await.expect("join"), 0);
    assert_eq!(read.await.expect("join"), 0);
    assert_eq!(*order.lock().unwrap(), vec!["exclusive", "read"]);
}

/// The finish runs where the instance cannot be opened for the command, which then never runs,
/// without the write token, and the call reports the open's failure.
#[tokio::test]
async fn an_exclusive_call_finishes_where_the_instance_cannot_be_opened() {
    let root = lore_base::test_util::TempDir::new("lore-call-exclusive-unopenable-");
    new_local_repository(&root).await;
    let ran = Arc::new(Mutex::new(Vec::new()));
    let (command_ran, finish_ran) = (ran.clone(), ran.clone());
    let dot = root.join(".lore");
    let token_path = root.to_path_buf();

    let status = repository_call_exclusive_planned(
        globals_at(&root),
        None,
        (),
        "exclusive_unopenable",
        move |_repository, ()| async move {
            // Unopenable from here on: the instance has no repository ID.
            std::fs::remove_file(dot.join("id")).expect("remove the repository ID");
            Ok::<Option<()>, SampleError>(Some(()))
        },
        async move |_repository, _token, _plan: &()| {
            command_ran.lock().unwrap().push("command");
            Ok::<(), SampleError>(())
        },
        move |()| async move {
            // The write token is free by the time the finish runs.
            let token = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                lore_revision::repository::RepositoryWriteToken::acquire(&token_path),
            )
            .await;
            finish_ran.lock().unwrap().push(if token.is_ok() {
                "finish"
            } else {
                "finish while the write token was held"
            });
        },
    )
    .await;

    assert_eq!(
        status, -1,
        "the open fails as internal, with no repository ID to read"
    );
    assert_eq!(*ran.lock().unwrap(), vec!["finish"]);
}

/// An instance whose instance file is missing is planned for: the call first recovers its ID as a
/// write command does, so the plan, which opens the instance without its stores, gets it.
#[tokio::test]
async fn an_exclusive_call_plans_for_an_instance_missing_its_instance_file() {
    let root = lore_base::test_util::TempDir::new("lore-call-exclusive-no-instance-file-");
    new_local_repository(&root).await;
    std::fs::remove_file(root.join(".lore").join("instance")).expect("remove the instance file");
    let planned = Arc::new(Mutex::new(None));
    let seen = planned.clone();

    let status = repository_call_exclusive_planned(
        globals_at(&root),
        None,
        (),
        "exclusive_no_instance_file",
        move |repository, ()| async move {
            *seen.lock().unwrap() = Some(repository.instance_id);
            Ok::<Option<()>, SampleError>(None)
        },
        async move |_repository, _token, _plan: &()| Ok::<(), SampleError>(()),
        move |()| async move {},
    )
    .await;

    assert_eq!(status, 0);
    let planned = planned.lock().unwrap().expect("the plan runs");
    assert!(!planned.is_zero(), "the plan sees the recovered ID");
    assert!(root.join(".lore").join("instance").is_file());
}
