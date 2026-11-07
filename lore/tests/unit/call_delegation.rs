// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;

use lore::args::LoreArgs;
use lore::call_delegation::*;
use lore::interface::LoreEventCallback;
use lore::interface::LoreString;
use lore::remote::command::LoreCommand;
use lore::remote::service_process::SERVICE_EXECUTABLE_SETTING;
use lore::remote::service_process::running_as_service;
use lore_base::error::InvalidArguments;
use lore_base::error::NotFound;
use lore_base::text::ValidateText;
use lore_error_set::FfiError;
use lore_error_set::prelude::*;
use lore_revision::event::EventError;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreEventCallbackConfig;
use lore_revision::interface::LoreGlobalArgs;

/// Held by each test that listens on or connects to this process's service socket. The name is
/// per process, so a connect made while another test listens there reaches that test's listener,
/// and each then waits on the other.
static SERVICE_SOCKET_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// A concrete error whose `NotFound` variant carries error code 79, so the
// async failure path has a known non-`1` code to assert against.
#[error_set]
enum SampleError {
    NotFound,
}

impl EventError for SampleError {}

// The async entry point returns `void`, so the only channel for the code is
// the callback. The callback is a real `extern "C"` function pointer (the
// FFI boundary), keyed by `user_context` to a per-test sink.
struct AsyncSink {
    status: Mutex<Option<i32>>,
    message: Mutex<Option<String>>,
    done: Mutex<Option<mpsc::Sender<()>>>,
}

fn registry() -> &'static Mutex<HashMap<u64, &'static AsyncSink>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u64, &'static AsyncSink>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

unsafe extern "C" fn record_event(event: &LoreEvent, user_context: u64) {
    let sink = *registry().lock().unwrap().get(&user_context).unwrap();
    match event {
        LoreEvent::Complete(data) => {
            *sink.status.lock().unwrap() = Some(data.status);
            *sink.message.lock().unwrap() = Some(data.error.message.as_str().to_string());
        }
        // `End` fires after `Complete`; use it to release the test.
        LoreEvent::End(_) => {
            if let Some(sender) = sink.done.lock().unwrap().take() {
                let _ = sender.send(());
            }
        }
        _ => {}
    }
}

#[test]
fn async_failure_delivers_code_only_through_complete_status() {
    let (done_tx, done_rx) = mpsc::channel();
    // Leaked so the `'static` callback can hold a stable reference for the
    // duration of the spawned task; the test process tears it down.
    let sink: &'static AsyncSink = Box::leak(Box::new(AsyncSink {
        status: Mutex::new(None),
        message: Mutex::new(None),
        done: Mutex::new(Some(done_tx)),
    }));
    let context = sink as *const AsyncSink as u64;
    registry().lock().unwrap().insert(context, sink);

    let config = LoreEventCallbackConfig {
        user_context: context,
        func: Some(record_event),
    };

    let args = lore::auth::LoreAuthLocalUserInfoArgs {
        auth_endpoint: LoreString::default(),
        user_ids: lore_revision::interface::LoreArray::default(),
        with_identity_token: 0,
        with_access_token: 0,
    };

    // The async entry point returns `()`; the failing handler's code can
    // only reach the caller through the `Complete` event.
    let returned: () = run_asynchronously(
        &LoreGlobalArgs::default(),
        &args,
        config,
        |_globals, _args, callback| async move {
            // The wrappers turn a concrete error into the derived status.
            lore::call::no_repository_call(
                LoreGlobalArgs::default(),
                callback,
                (),
                "async_failure",
                |()| async move { Err::<(), SampleError>(NotFound.into()) },
            )
            .await
        },
    );
    assert_eq!(returned, ());

    // Block until the spawned task has flushed `Complete` and `End`.
    done_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("async task must complete");

    let expected_code = SampleError::from(NotFound).ffi_code();
    assert_ne!(expected_code, 1, "the sample error must not collide with 1");
    assert_eq!(
        *sink.status.lock().unwrap(),
        Some(expected_code),
        "the failure code arrives through Complete.status"
    );
}

fn rejected_status() -> i32 {
    InvalidArguments {
        reason: String::new(),
    }
    .ffi_code()
}

fn invalid_utf8() -> LoreString {
    LoreString::from_bytes(&[b'a', 0xff, 0xfe])
}

fn no_callback() -> LoreEventCallbackConfig {
    LoreEventCallbackConfig {
        user_context: 0,
        func: None,
    }
}

/// Run `args` through the synchronous entry point and report the status
/// together with whether the handler was reached.
fn dispatch<ArgsType: ValidateText + Clone + Into<LoreCommand>>(
    globals: &LoreGlobalArgs,
    args: &ArgsType,
) -> (i32, bool) {
    let reached = Arc::new(AtomicBool::new(false));
    let handler_reached = reached.clone();
    let status = run_synchronously(
        globals,
        args,
        no_callback(),
        move |_globals, _args, _callback| {
            handler_reached.store(true, Ordering::Release);
            0
        },
    );
    (status, reached.load(Ordering::Acquire))
}

/// A plain text field.
#[test]
fn a_string_argument_that_is_not_utf8_is_rejected_before_the_handler_runs() {
    let args = lore::revision_tree::resolve_path::LoreRevisionTreeResolvePathArgs {
        id: 1,
        handle: lore::revision_tree::handle::LoreRevisionTree::INVALID,
        path: invalid_utf8(),
    };

    let (status, handler_ran) = dispatch(&LoreGlobalArgs::default(), &args);

    assert_eq!(
        status,
        rejected_status(),
        "a non-UTF-8 path must be rejected"
    );
    assert!(!handler_ran, "the verb must not run on a rejected call");
}

/// The same field holding valid text reaches the verb, so the check rejects
/// the encoding rather than the field.
#[test]
fn a_string_argument_that_is_utf8_reaches_the_handler() {
    let args = lore::revision_tree::resolve_path::LoreRevisionTreeResolvePathArgs {
        id: 1,
        handle: lore::revision_tree::handle::LoreRevisionTree::INVALID,
        path: LoreString::from_str("docs/readme.md"),
    };

    let (status, handler_ran) = dispatch(&LoreGlobalArgs::default(), &args);

    assert_eq!(status, 0);
    assert!(handler_ran, "valid text must reach the verb");
}

/// An element of an array of text.
#[test]
fn a_text_array_element_that_is_not_utf8_is_rejected() {
    let args = lore::file::LoreFileHashArgs {
        paths: lore_revision::interface::LoreArray::from_vec(vec![
            LoreString::from_str("first"),
            invalid_utf8(),
        ]),
    };

    let (status, handler_ran) = dispatch(&LoreGlobalArgs::default(), &args);

    assert_eq!(status, rejected_status());
    assert!(!handler_ran, "the verb must not run on a rejected call");
}

/// A text field of a struct held in an array, which the check only reaches
/// by descending into the element type.
#[test]
fn a_text_field_of_an_array_element_that_is_not_utf8_is_rejected() {
    let args = lore::storage::put_file::LoreStoragePutFileArgs {
        handle: lore::storage::handle::LoreStore::INVALID,
        items: lore_revision::interface::LoreArray::from_vec(vec![
            lore::storage::put_file::LoreStoragePutFileItem {
                path: invalid_utf8(),
                ..Default::default()
            },
        ]),
    };

    let (status, handler_ran) = dispatch(&LoreGlobalArgs::default(), &args);

    assert_eq!(status, rejected_status());
    assert!(!handler_ran, "the verb must not run on a rejected call");
}

/// A batch write verb's entry name. The entry type is only reached by
/// descending into it, so declaring it text-free instead of deriving the
/// check would let a name through unread.
#[test]
fn a_batch_entry_name_that_is_not_utf8_is_rejected() {
    let args = lore::revision_tree::add::LoreRevisionTreeAddArgs {
        batch_id: 1,
        handle: lore::revision_tree::handle::LoreRevisionTree::INVALID,
        entries: lore_revision::interface::LoreArray::from_vec(vec![
            lore::revision_tree::add::LoreRevisionTreeAddEntry {
                name: invalid_utf8(),
                ..Default::default()
            },
        ]),
    };

    let (status, handler_ran) = dispatch(&LoreGlobalArgs::default(), &args);

    assert_eq!(status, rejected_status());
    assert!(!handler_ran, "the verb must not run on a rejected call");
}

/// A text field of a nested struct, reached by descending one field deep.
#[test]
fn a_text_field_of_a_nested_argument_struct_that_is_not_utf8_is_rejected() {
    let args = lore::storage::open::LoreStorageOpenArgs {
        remote_config: lore::storage::open::LoreStorageRemoteConfig {
            remote_url: invalid_utf8(),
        },
        ..Default::default()
    };

    let (status, handler_ran) = dispatch(&LoreGlobalArgs::default(), &args);

    assert_eq!(status, rejected_status());
    assert!(!handler_ran, "the verb must not run on a rejected call");
}

/// The global arguments every operation carries are checked too, so a bad
/// repository path fails before the path is read.
#[test]
fn a_global_argument_that_is_not_utf8_is_rejected() {
    let globals = LoreGlobalArgs {
        repository_path: invalid_utf8(),
        ..LoreGlobalArgs::default()
    };
    let args = lore::revision_tree::close::LoreRevisionTreeCloseArgs::default();

    let (status, handler_ran) = dispatch(&globals, &args);

    assert_eq!(status, rejected_status());
    assert!(!handler_ran, "the verb must not run on a rejected call");
}

/// The rejection names the field so a caller can tell which string to fix.
#[test]
fn the_rejection_names_the_failing_field() {
    let args = lore::storage::put_file::LoreStoragePutFileArgs {
        handle: lore::storage::handle::LoreStore::INVALID,
        items: lore_revision::interface::LoreArray::from_vec(vec![
            lore::storage::put_file::LoreStoragePutFileItem::default(),
            lore::storage::put_file::LoreStoragePutFileItem {
                path: invalid_utf8(),
                ..Default::default()
            },
        ]),
    };

    let error =
        validate_call_text(&LoreGlobalArgs::default(), &args).expect_err("the item path must fail");

    assert_eq!(
        error.to_string(),
        "invalid arguments: items[1].path is not valid UTF-8"
    );
}

/// The asynchronous entry point rejects the same calls, reporting the status
/// through `Complete` because it has no return value.
#[test]
fn the_asynchronous_entry_point_rejects_text_that_is_not_utf8() {
    let (done_tx, done_rx) = mpsc::channel();
    let sink: &'static AsyncSink = Box::leak(Box::new(AsyncSink {
        status: Mutex::new(None),
        message: Mutex::new(None),
        done: Mutex::new(Some(done_tx)),
    }));
    let context = sink as *const AsyncSink as u64;
    registry().lock().unwrap().insert(context, sink);

    let config = LoreEventCallbackConfig {
        user_context: context,
        func: Some(record_event),
    };

    let args = lore::revision_tree::resolve_path::LoreRevisionTreeResolvePathArgs {
        id: 1,
        handle: lore::revision_tree::handle::LoreRevisionTree::INVALID,
        path: invalid_utf8(),
    };

    let reached = Arc::new(AtomicBool::new(false));
    let handler_reached = reached.clone();
    run_asynchronously(
        &LoreGlobalArgs::default(),
        &args,
        config,
        move |_globals, _args, _callback| {
            let reached = handler_reached.clone();
            async move {
                reached.store(true, Ordering::Release);
                0
            }
        },
    );

    done_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the rejection must complete");

    assert_eq!(*sink.status.lock().unwrap(), Some(rejected_status()));
    assert!(
        !reached.load(Ordering::Acquire),
        "the verb must not run on a rejected call"
    );
}

/// Credential arguments with no single meaning -- here an identity alongside
/// a token that already names one -- are rejected the same way malformed text
/// is, and for the same reason: the call cannot be run as asked.
#[test]
fn conflicting_identity_arguments_are_rejected_before_the_handler_runs() {
    let globals = LoreGlobalArgs {
        identity: LoreString::from_str("bob"),
        identity_token: LoreString::from_str("some-token"),
        ..LoreGlobalArgs::default()
    };
    let args = lore::revision_tree::close::LoreRevisionTreeCloseArgs::default();

    let (status, handler_ran) = dispatch(&globals, &args);

    assert_eq!(status, rejected_status());
    assert!(!handler_ran, "the verb must not run on a rejected call");
}

/// A repository directory with the given `config.toml` body. Returns the
/// repository root, whose lifetime keeps the directory alive.
fn repository_with_config(label: &str, config: &str) -> lore_base::test_util::TempDir {
    let root = lore_base::test_util::TempDir::new(&format!("lore-cd-{label}-"));
    let dot_dir = root.path().join(lore_revision::repository::DOT_LORE);
    std::fs::create_dir_all(&dot_dir).expect("creating the repository directory");
    std::fs::write(dot_dir.join("config.toml"), config).expect("writing the config");
    root
}

fn globals_for(root: &lore_base::test_util::TempDir) -> LoreGlobalArgs {
    LoreGlobalArgs {
        repository_path: root.path().display().to_string().into(),
        ..LoreGlobalArgs::default()
    }
}

/// A FUSE instance forces a relay to the service regardless of the
/// process-wide "use a service" setting -- the decision is made from the
/// instance's own config, not from the setting that governs everything
/// else, which is what makes it a forced relay rather than the ordinary one.
#[test]
fn fuse_instance_forces_service_call_when_service_disabled() {
    let root = repository_with_config("fuse-forced", "[vfs]\nvfs_type = \"Fuse\"\n");
    let globals = globals_for(&root);

    assert!(
        instance_requires_service_given(&globals, running_as_service()),
        "a FUSE-backed instance must require the service"
    );
}

/// A non-FUSE instance (here SWFS) is left to the ordinary process-wide
/// decision: nothing about it forces a relay.
#[test]
fn non_fuse_instance_keeps_process_wide_decision() {
    let root = repository_with_config("non-fuse", "[vfs]\nvfs_type = \"Swfs\"\n");
    let globals = globals_for(&root);

    assert!(
        !instance_requires_service_given(&globals, running_as_service()),
        "a non-FUSE instance must not force a relay"
    );
}

/// Inside the service process, a FUSE instance never forces a relay: the
/// service would otherwise relay the call to itself.
#[test]
fn in_service_process_fuse_instance_runs_locally() {
    let root = repository_with_config("fuse-in-service", "[vfs]\nvfs_type = \"Fuse\"\n");
    let globals = globals_for(&root);

    // `running_as_service` is passed in directly rather than set through
    // `mark_running_as_service()`: that flag is one-way and process-wide,
    // so setting the real one here would leak into every other test
    // sharing this process afterwards.
    assert!(
        !instance_requires_service_given(&globals, true),
        "a FUSE instance must run locally inside the service process"
    );
}

fn clone_command(vfs: lore::repository::LoreVfsType) -> LoreCommand {
    LoreCommand::RepositoryClone(lore::repository::LoreRepositoryCloneArgs {
        vfs,
        ..Default::default()
    })
}

/// A clone whose `vfs` resolves to FUSE forces the same relay an existing
/// FUSE instance does: outside the service, a local FUSE clone is
/// refused, not mounted.
#[test]
fn a_fuse_clone_requires_the_service() {
    let globals = LoreGlobalArgs::default();
    let command = clone_command(lore::repository::LoreVfsType::Fuse);

    assert!(
        command_requires_service(&globals, &command),
        "a FUSE clone must require the service"
    );
}

/// A clone whose `vfs` does not resolve to FUSE is left to the ordinary
/// process-wide relaying decision.
#[test]
fn a_plain_clone_does_not() {
    let globals = LoreGlobalArgs::default();
    let command = clone_command(lore::repository::LoreVfsType::None);

    assert!(
        !command_requires_service(&globals, &command),
        "a plain clone must not force a relay"
    );
}

/// Inside the service process, a FUSE clone never forces a relay either:
/// the service would otherwise relay the call to itself.
#[test]
fn inside_the_service_nothing_requires_it() {
    let globals = LoreGlobalArgs::default();
    let command = clone_command(lore::repository::LoreVfsType::Fuse);

    assert!(
        !command_requires_service_given(&globals, &command, true),
        "a FUSE clone must run locally inside the service process"
    );
}

fn create_args(vfs: lore::repository::LoreVfsType) -> lore::repository::LoreRepositoryCreateArgs {
    lore::repository::LoreRepositoryCreateArgs {
        repository_url: Default::default(),
        description: Default::default(),
        id: Default::default(),
        vfs,
        use_shared_store: lore_revision::repository::LoreSharedStoreMode::Disabled,
        shared_store_path: Default::default(),
    }
}

fn create_command(vfs: lore::repository::LoreVfsType) -> LoreCommand {
    LoreCommand::RepositoryCreate(create_args(vfs))
}

/// A create whose `vfs` resolves to FUSE forces the same relay a FUSE
/// clone does: outside the service, it would be refused rather than set
/// up as a mount.
#[test]
fn a_fuse_create_requires_the_service() {
    let globals = LoreGlobalArgs::default();
    let command = create_command(lore::repository::LoreVfsType::Fuse);

    assert!(
        command_requires_service(&globals, &command),
        "a FUSE create must require the service"
    );
}

/// `LoreVfsType::Default` resolves to FUSE on Linux, so a clone that
/// leaves `vfs` at its default must require the service exactly as one
/// naming FUSE explicitly does.
#[cfg(target_os = "linux")]
#[test]
fn a_default_vfs_clone_resolves_to_fuse_and_requires_the_service() {
    let globals = LoreGlobalArgs::default();
    let command = clone_command(lore::repository::LoreVfsType::Default);

    assert!(
        command_requires_service(&globals, &command),
        "a Default-vfs clone must resolve to FUSE on Linux and require the service"
    );
}

/// A FUSE instance with no running service, in a program that names no executable to start
/// one from and does not run the service itself, fails with `ServiceUnavailable`, on Linux
/// naming where to set one, rather than silently running the call locally against a mount the
/// service isn't maintaining.
#[tokio::test]
async fn fuse_instance_without_a_service_to_reach_or_start_reports_where_to_name_one() {
    let _socket = SERVICE_SOCKET_TESTS.lock().await;
    let root = repository_with_config("fuse-unavailable", "[vfs]\nvfs_type = \"Fuse\"\n");
    let globals = globals_for(&root);

    let (done_tx, done_rx) = mpsc::channel();
    let sink: &'static AsyncSink = Box::leak(Box::new(AsyncSink {
        status: Mutex::new(None),
        message: Mutex::new(None),
        done: Mutex::new(Some(done_tx)),
    }));
    let context = sink as *const AsyncSink as u64;
    registry().lock().unwrap().insert(context, sink);
    let config = LoreEventCallbackConfig {
        user_context: context,
        func: Some(record_event),
    };
    let callback = lore_revision::event::convert_event_callback(config);

    let reached = Arc::new(AtomicBool::new(false));
    let handler_reached = reached.clone();
    let args = lore::revision_tree::close::LoreRevisionTreeCloseArgs::default();
    let status = dispatch_call(
        globals,
        args,
        callback,
        move |_globals, _args, _callback| {
            let reached = handler_reached.clone();
            async move {
                reached.store(true, Ordering::Release);
                0
            }
        },
    )
    .await;

    assert!(
        !reached.load(Ordering::Acquire),
        "a FUSE instance without a running service must not run locally"
    );
    done_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the call must complete");
    let expected_code = lore_base::error::ServiceUnavailable {
        reason: String::new(),
    }
    .ffi_code();
    assert_eq!(status, expected_code);
    assert_eq!(*sink.status.lock().unwrap(), Some(expected_code));
    let message = sink.message.lock().unwrap().clone().unwrap_or_default();
    assert!(
        message.contains("This instance is served through a FUSE mount"),
        "the message must say why the call needs the service, got: {message}"
    );
    assert!(
        !cfg!(target_os = "linux") || message.contains(SERVICE_EXECUTABLE_SETTING),
        "on Linux, where a service would be started, it must say where to name one, got: {message}"
    );
}

/// A sink registered for [`record_event`], the callback reporting to it, and the receiver
/// its `End` releases.
fn recording_callback() -> (&'static AsyncSink, LoreEventCallback, mpsc::Receiver<()>) {
    let (done_tx, done_rx) = mpsc::channel();
    let sink: &'static AsyncSink = Box::leak(Box::new(AsyncSink {
        status: Mutex::new(None),
        message: Mutex::new(None),
        done: Mutex::new(Some(done_tx)),
    }));
    let context = sink as *const AsyncSink as u64;
    registry().lock().unwrap().insert(context, sink);
    let callback = lore_revision::event::convert_event_callback(LoreEventCallbackConfig {
        user_context: context,
        func: Some(record_event),
    });
    (sink, callback, done_rx)
}

/// The status and message a call reported to `sink`, once its `End` came.
fn reported(sink: &AsyncSink, done: &mpsc::Receiver<()>) -> (Option<i32>, String) {
    done.recv_timeout(std::time::Duration::from_secs(10))
        .expect("the call must complete");
    let message = sink.message.lock().unwrap().clone().unwrap_or_default();
    (*sink.status.lock().unwrap(), message)
}

fn service_unavailable() -> i32 {
    lore_base::error::ServiceUnavailable {
        reason: String::new(),
    }
    .ffi_code()
}

/// Whether [`dispatch_call`] of `args`, from no repository and with no service running, ran
/// its handler, which reports nothing, else its status and what it reported.
async fn dispatched_without_a_service<ArgsType: LoreArgs>(
    args: ArgsType,
) -> (bool, i32, Option<i32>, String) {
    let (sink, callback, done) = recording_callback();
    let reached = Arc::new(AtomicBool::new(false));
    let handler_reached = reached.clone();
    let status = dispatch_call(
        LoreGlobalArgs::default(),
        args,
        callback,
        move |_globals, _args, _callback| {
            let reached = handler_reached.clone();
            async move {
                reached.store(true, Ordering::Release);
                0
            }
        },
    )
    .await;
    if reached.load(Ordering::Acquire) {
        return (true, status, None, String::new());
    }
    let (reported, message) = reported(sink, &done);
    (false, status, reported, message)
}

/// A FUSE clone and a FUSE create through the arguments entry point, with no service
/// running and none this program can start, never run here: each is refused with
/// `ServiceUnavailable` and its own reason.
#[tokio::test]
async fn fuse_clone_and_create_arguments_without_a_running_service_report_their_reasons() {
    let _socket = SERVICE_SOCKET_TESTS.lock().await;
    let clone = lore::repository::LoreRepositoryCloneArgs {
        vfs: lore::repository::LoreVfsType::Fuse,
        ..Default::default()
    };
    let cases = [
        (
            dispatched_without_a_service(clone).await,
            "A FUSE clone is mounted by the Lore service",
        ),
        (
            dispatched_without_a_service(create_args(lore::repository::LoreVfsType::Fuse)).await,
            "A FUSE instance is created and mounted by the Lore service",
        ),
    ];

    for ((ran, status, reported, message), reason) in cases {
        assert!(!ran, "{reason}: the call must not run here");
        assert_eq!(status, service_unavailable(), "{reason}: {message}");
        assert_eq!(reported, Some(status));
        assert!(
            message.contains(reason),
            "expected '{reason}', got: {message}"
        );
    }
}

/// A FUSE clone through the command entry point, with no service running and none this
/// program can start, is refused with `ServiceUnavailable` and the clone's reason rather than
/// run here.
#[tokio::test]
async fn a_fuse_clone_command_without_a_running_service_reports_the_clone_reason() {
    let _socket = SERVICE_SOCKET_TESTS.lock().await;
    let (sink, callback, done) = recording_callback();

    let status = dispatch_command(
        LoreGlobalArgs::default(),
        clone_command(lore::repository::LoreVfsType::Fuse),
        callback,
    )
    .await;

    let (reported, message) = reported(sink, &done);
    assert_eq!(status, service_unavailable(), "{message}");
    assert_eq!(reported, Some(status));
    assert!(
        message.contains("A FUSE clone is mounted by the Lore service"),
        "got: {message}"
    );
}
