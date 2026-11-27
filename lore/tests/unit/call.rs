// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::error::RepositoryNotFound;
use lore_revision::event::EventError;
use lore_revision::repository::RepositoryError;

pub(crate) mod test_support;

use std::sync::Mutex;

use lore::call::*;
use lore_base::error::NotFound;
use lore_error_set::Location;
use lore_error_set::prelude::*;
use lore_revision::interface::LoreGlobalArgs;
use test_support::completes;
use test_support::has_error_event;
use test_support::recording_callback;
use test_support::take_recorded;

// A concrete `#[error_set]` error for the wrapper closures. Its `NotFound`
// variant wraps `lore_base::error::NotFound`, which carries error code 79, so
// the failure path has a known non-internal code to assert against.
#[error_set]
enum SampleError {
    NotFound,
}

impl EventError for SampleError {}

#[tokio::test]
async fn failing_op_completes_with_error_code_and_no_error_event() {
    let (sink, callback) = recording_callback();
    let status = no_repository_call(
        LoreGlobalArgs::default(),
        callback,
        (),
        "failing_op",
        |()| async move { Err::<(), SampleError>(NotFound.into()) },
    )
    .await;

    let events = take_recorded(&sink);

    assert!(
        !has_error_event(&events),
        "no Error event must be emitted on terminal failure"
    );

    // Exactly one `Complete` event.
    let completes = completes(&events);
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
    let (sink, callback) = recording_callback();
    let status = no_repository_call(
        LoreGlobalArgs::default(),
        callback,
        (),
        "succeeding_op",
        |()| async move { Ok::<(), SampleError>(()) },
    )
    .await;

    assert_eq!(status, 0);

    let events = take_recorded(&sink);
    let completes = completes(&events);
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

    let (sink, callback) = recording_callback();
    let status = repository_call_read(
        globals,
        callback,
        (),
        "missing_repo",
        |_repo, ()| async move { Ok::<(), SampleError>(()) },
    )
    .await;

    let events = take_recorded(&sink);
    let completes = completes(&events);
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
    let (sink, callback) = recording_callback();
    let _status = no_repository_call(
        LoreGlobalArgs::default(),
        callback,
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

    let events = take_recorded(&sink);
    let data = completes(&events)
        .into_iter()
        .next()
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

/// A repository call at a path inside a FUSE mount whose external directory can't be examined
/// fails before any command runs, delivering one `Complete` that carries the error, for a read
/// call and a write call alike. The instance's external directory, in the binary's sandbox, has
/// mode `0o000`, so its lookup fails with a permission error and touches no other test's.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_call_inside_a_mount_with_an_unknown_external_directory_completes_with_its_error() {
    use std::os::unix::fs::PermissionsExt;

    use lore_revision::fs::fuse::discover::test_util::fuse_lore_entry;
    use lore_revision::fs::fuse::discover::test_util::plant_table;
    use lore_revision::instance::InstanceId;

    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let instance_id = InstanceId::generate();
    let external = lore_revision::global::get_global_data_dir()
        .expect("the sandbox's global data directory")
        .join("external")
        .join(instance_id.text_encoding());
    std::fs::create_dir_all(&external).expect("make the external directory");
    std::fs::set_permissions(&external, std::fs::Permissions::from_mode(0o000))
        .expect("make it unreadable");
    let mount = lore_base::test_util::TempDir::new("lore-call-unknown-external-");
    let _planted = plant_table(mount.path(), fuse_lore_entry(mount.path(), instance_id));
    let globals = || LoreGlobalArgs {
        repository_path: mount.path().join("sub").display().to_string().into(),
        ..LoreGlobalArgs::default()
    };

    let (read_sink, read_callback) = recording_callback();
    let read = repository_call_read(
        globals(),
        read_callback,
        (),
        "unknown_read",
        |_, ()| async { Ok::<(), SampleError>(()) },
    )
    .await;
    let (write_sink, write_callback) = recording_callback();
    let write = repository_call_write(
        globals(),
        write_callback,
        (),
        "unknown_write",
        |_, _, ()| async { Ok::<(), SampleError>(()) },
    )
    .await;

    std::fs::set_permissions(&external, std::fs::Permissions::from_mode(0o700))
        .expect("make it readable again");
    std::fs::remove_dir(&external).expect("remove the external directory");
    for (status, sink) in [(read, read_sink), (write, write_sink)] {
        let events = take_recorded(&sink);
        let completes = completes(&events);
        assert_eq!(completes.len(), 1, "exactly one Complete event");
        assert_eq!(
            status, -1,
            "the lookup's internal error, not a missing repository"
        );
        assert_eq!(completes[0].status, status);
        assert_eq!(completes[0].error.error_code, status);
        assert!(!has_error_event(&events));
    }
}

/// Stores a fragment in `repository`'s immutable store, which its next flush writes.
async fn store_a_fragment(repository: &lore_revision::repository::RepositoryContext) {
    let payload = bytes::Bytes::from_static(b"a payload the call's flush syncs");
    let address = lore_base::types::Address {
        hash: lore_storage::hash::hash_slice(&payload),
        context: lore_base::types::Context::default(),
    };
    let fragment = lore_base::types::Fragment {
        flags: 0,
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    };
    repository
        .immutable_store()
        .put(repository.id, address, fragment, Some(payload), false)
        .await
        .expect("the fragment is stored");
}

/// Stores a fragment in `repository`'s immutable store, and makes the next packfile sync on this
/// thread fail, so the flush that writes the fragment fails.
async fn store_a_fragment_whose_sync_fails(
    repository: &lore_revision::repository::RepositoryContext,
) -> Result<(), SampleError> {
    store_a_fragment(repository).await;
    lore_storage::test_util::fail_packfile_syncs(1);
    Ok(())
}

/// Stores a fragment in `repository`'s immutable store and fails with `NotFound`, after making
/// the next packfile sync on this thread fail where `flush_fails`.
async fn fail_after_storing_a_fragment(
    repository: &lore_revision::repository::RepositoryContext,
    flush_fails: bool,
) -> Result<(), SampleError> {
    store_a_fragment(repository).await;
    if flush_fails {
        lore_storage::test_util::fail_packfile_syncs(1);
    }
    Err(NotFound.into())
}

/// A read, write or planned exclusive call with `no_background_flush` writes what it held back
/// before it completes, and completes with that write's error when it fails. Without the option
/// the same failure, in the flush that follows the call, leaves the call's result alone.
#[tokio::test]
async fn a_call_holding_back_its_index_writes_fails_when_their_write_fails() {
    for held in [true, false] {
        for wrapper in ["read", "write", "exclusive"] {
            let root = lore_base::test_util::TempDir::new("lore-call-held-back-");
            new_local_repository(&root).await;
            let globals = LoreGlobalArgs {
                no_background_flush: u8::from(held),
                sync_data: 1,
                ..globals_at(&root)
            };

            let status =
                match wrapper {
                    "read" => {
                        repository_call_read(
                            globals,
                            None,
                            (),
                            "held_back",
                            |repository, ()| async move {
                                store_a_fragment_whose_sync_fails(&repository).await
                            },
                        )
                        .await
                    }
                    "write" => {
                        repository_call_write(
                            globals,
                            None,
                            (),
                            "held_back",
                            |repository, _token, ()| async move {
                                store_a_fragment_whose_sync_fails(&repository).await
                            },
                        )
                        .await
                    }
                    _ => repository_call_exclusive_planned(
                        globals,
                        None,
                        (),
                        "held_back",
                        |_repository, ()| async move { Ok::<Option<()>, SampleError>(Some(())) },
                        async move |repository, _token, _plan: &()| {
                            store_a_fragment_whose_sync_fails(&repository).await
                        },
                        move |()| async move {},
                    )
                    .await,
                };

            assert_eq!(
                status != 0,
                held,
                "{wrapper} call, held {held}: the call's status {status}"
            );
            lore_storage::test_util::fail_packfile_syncs(0);
        }
    }
}

/// Runs a `wrapper` call, read, write or planned exclusive, with `no_background_flush`, whose
/// command fails after storing a fragment, and checks that the call writes what it held back
/// before it completes, and completes with the command's error. Where `flush_fails`, that write
/// fails, which the check sees in the packfile sync it failed. The flush is held at its packfile
/// sync for long enough for a call that did not wait for it to complete meanwhile: a wait cannot
/// be polled for, since it is the absence of progress. Each case runs in a test of its own, as the
/// storage test points it arms are the thread's, and the flush a call leaves to run after it
/// could take those of a later case.
async fn assert_a_failing_holding_call_writes_before_completing(wrapper: &str, flush_fails: bool) {
    let root = lore_base::test_util::TempDir::new("lore-call-held-back-failed-command-");
    new_local_repository(&root).await;
    let globals = LoreGlobalArgs {
        no_background_flush: 1,
        sync_data: 1,
        ..globals_at(&root)
    };
    let order = Arc::new(Mutex::new(Vec::new()));
    let completed = order.clone();
    let callback: lore::interface::LoreEventCallback =
        Some(Box::new(move |event: &lore_revision::event::LoreEvent| {
            if matches!(event, lore_revision::event::LoreEvent::Complete(_)) {
                completed.lock().unwrap().push("complete");
            }
        }));
    let pause = lore_storage::test_util::pause_at(lore_storage::test_util::PausePoint::Syncing);

    let call = async {
        match wrapper {
            "read" => {
                repository_call_read(
                    globals,
                    callback,
                    (),
                    "held_back_failed_command",
                    |repository, ()| async move {
                        fail_after_storing_a_fragment(&repository, flush_fails).await
                    },
                )
                .await
            }
            "write" => {
                repository_call_write(
                    globals,
                    callback,
                    (),
                    "held_back_failed_command",
                    |repository, _token, ()| async move {
                        fail_after_storing_a_fragment(&repository, flush_fails).await
                    },
                )
                .await
            }
            _ => {
                repository_call_exclusive_planned(
                    globals,
                    callback,
                    (),
                    "held_back_failed_command",
                    |_repository, ()| async move { Ok::<Option<()>, SampleError>(Some(())) },
                    async move |repository, _token, _plan: &()| {
                        fail_after_storing_a_fragment(&repository, flush_fails).await
                    },
                    move |()| async move {},
                )
                .await
            }
        }
    };
    let flush = async {
        let reached =
            tokio::time::timeout(std::time::Duration::from_secs(10), pause.reached.notified())
                .await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        if reached.is_ok() {
            order.lock().unwrap().push("flush");
        }
        pause.resume.notify_one();
    };
    let (status, ()) = tokio::join!(call, flush);

    assert_eq!(
        *order.lock().unwrap(),
        vec!["flush", "complete"],
        "{wrapper} call, flush fails {flush_fails}"
    );
    assert_eq!(
        status,
        SampleError::from(NotFound).ffi_code(),
        "{wrapper} call, flush fails {flush_fails}"
    );
    if flush_fails {
        assert_eq!(
            lore_storage::test_util::packfile_sync_failures_left(),
            0,
            "{wrapper} call: the write held back fails"
        );
    }
}

/// As [`assert_a_failing_holding_call_writes_before_completing`] describes, for a read call.
#[tokio::test]
async fn a_failing_read_call_writes_what_it_held_back_before_completing() {
    assert_a_failing_holding_call_writes_before_completing("read", false).await;
}

/// As [`assert_a_failing_holding_call_writes_before_completing`] describes, for a read call whose
/// held-back write fails.
#[tokio::test]
async fn a_failing_read_call_reports_its_error_when_its_held_back_write_fails() {
    assert_a_failing_holding_call_writes_before_completing("read", true).await;
}

/// As [`assert_a_failing_holding_call_writes_before_completing`] describes, for a write call.
#[tokio::test]
async fn a_failing_write_call_writes_what_it_held_back_before_completing() {
    assert_a_failing_holding_call_writes_before_completing("write", false).await;
}

/// As [`assert_a_failing_holding_call_writes_before_completing`] describes, for a write call whose
/// held-back write fails.
#[tokio::test]
async fn a_failing_write_call_reports_its_error_when_its_held_back_write_fails() {
    assert_a_failing_holding_call_writes_before_completing("write", true).await;
}

/// As [`assert_a_failing_holding_call_writes_before_completing`] describes, for a planned
/// exclusive call.
#[tokio::test]
async fn a_failing_planned_exclusive_call_writes_what_it_held_back_before_completing() {
    assert_a_failing_holding_call_writes_before_completing("exclusive", false).await;
}

/// As [`assert_a_failing_holding_call_writes_before_completing`] describes, for a planned
/// exclusive call whose held-back write fails.
#[tokio::test]
async fn a_failing_planned_exclusive_call_reports_its_error_when_its_held_back_write_fails() {
    assert_a_failing_holding_call_writes_before_completing("exclusive", true).await;
}
