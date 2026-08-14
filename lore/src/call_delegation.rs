// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::error::InvalidArguments;
use lore_base::text::TextNotUtf8;
use lore_base::text::ValidateText;
use lore_error_set::prelude::*;
use lore_revision::event::EventError;
use lore_revision::interface::LoreGlobalArgs;

use crate::args::InvokableLoreArgs;
use crate::interface::LoreEventCallback;
use crate::interface::LoreEventCallbackConfig;
use crate::remote::call::service_call;

/// Rejection of a call whose arguments are malformed, before the verb runs.
#[error_set]
pub(crate) enum ArgumentError {
    InvalidArguments,
}

impl EventError for ArgumentError {}

/// Check every text field a call carries, so a handler can read its arguments
/// as `&str`.
///
/// The C boundary accepts any bytes for a string. Checking the whole call here,
/// once, keeps a bad encoding a uniform argument rejection instead of leaving
/// each verb to catch it — or to miss it and read invalid text.
fn validate_call_text<ArgsType: ValidateText>(
    globals: &LoreGlobalArgs,
    args: &ArgsType,
) -> Result<(), ArgumentError> {
    globals
        .validate_text()
        .map_err(|error: TextNotUtf8| error.inside("globals"))
        .and_then(|()| args.validate_text())
        .map_err(|error| ArgumentError::from(InvalidArguments::from(error)))
}

pub(crate) fn run_synchronously<
    ArgsType: InvokableLoreArgs + ValidateText + Clone + Send + 'static,
    Handler: Fn(LoreGlobalArgs, ArgsType, LoreEventCallback) -> Fut,
    Fut: Future<Output = i32> + Send + 'static,
>(
    globals: &LoreGlobalArgs,
    args: &ArgsType,
    callback: LoreEventCallbackConfig,
    handler: Handler,
) -> i32 {
    let callback = lore_revision::event::convert_event_callback(callback);
    if let Err(error) = validate_call_text(globals, args) {
        return crate::runtime().block_on(reject_call(globals.clone(), callback, error));
    }
    let globals = globals.clone();
    let args = args.clone();
    crate::runtime().block_on(handler(globals, args, callback))
}

pub(crate) fn run_asynchronously<
    ArgsType: InvokableLoreArgs + ValidateText + Clone + Send + 'static,
    Handler: Fn(LoreGlobalArgs, ArgsType, LoreEventCallback) -> Fut,
    Fut: Future<Output = i32> + Send + 'static,
>(
    globals: &LoreGlobalArgs,
    args: &ArgsType,
    callback: LoreEventCallbackConfig,
    handler: Handler,
) {
    let callback = lore_revision::event::convert_event_callback(callback);
    if let Err(error) = validate_call_text(globals, args) {
        drop(lore_base::lore_spawn!(reject_call(
            globals.clone(),
            callback,
            error
        )));
        return;
    }
    let globals = globals.clone();
    let args = args.clone();
    drop(lore_base::lore_spawn!(handler(globals, args, callback)));
}

/// Report a malformed call the way a failing command reports: the status on the
/// return value and on a `Complete` event carrying the detail. No verb ran, so
/// no verb-specific terminal event fires.
async fn reject_call(
    globals: LoreGlobalArgs,
    callback: LoreEventCallback,
    error: ArgumentError,
) -> i32 {
    crate::call::no_repository_call(
        globals,
        callback,
        (),
        "validate_arguments",
        |()| async move { Err::<(), ArgumentError>(error) },
    )
    .await
}

pub(crate) async fn dispatch_call<
    ArgsType: InvokableLoreArgs + Clone + Send + 'static,
    Handler: Fn(LoreGlobalArgs, ArgsType, LoreEventCallback) -> Fut,
    Fut: Future<Output = i32> + Send + 'static,
>(
    globals: LoreGlobalArgs,
    args: ArgsType,
    callback: LoreEventCallback,
    handler: Handler,
) -> i32 {
    if let Ok(environment_value) = std::env::var("LORE_USE_SERVICE")
        && !environment_value.is_empty()
    {
        service_call(globals, args, callback).await
    } else {
        handler(globals, args, callback).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::OnceLock;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    use lore_base::error::NotFound;
    use lore_error_set::FfiError;
    use lore_error_set::prelude::*;
    use lore_revision::event::EventError;
    use lore_revision::event::LoreEvent;
    use lore_revision::interface::LoreEventCallbackConfig;
    use lore_revision::interface::LoreGlobalArgs;

    use super::*;
    use crate::interface::LoreString;

    // A concrete error whose `NotFound` variant carries error code 13, so the
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
            done: Mutex::new(Some(done_tx)),
        }));
        let context = sink as *const AsyncSink as u64;
        registry().lock().unwrap().insert(context, sink);

        let config = LoreEventCallbackConfig {
            user_context: context,
            func: Some(record_event),
        };

        let args = crate::auth::LoreAuthLocalUserInfoArgs {
            auth_endpoint: LoreString::default(),
            user_ids: lore_revision::interface::LoreArray::default(),
            with_token: 0,
        };

        // The async entry point returns `()`; the failing handler's code can
        // only reach the caller through the `Complete` event.
        let returned: () = run_asynchronously(
            &LoreGlobalArgs::default(),
            &args,
            config,
            |_globals, _args, callback| async move {
                // The wrappers turn a concrete error into the derived status.
                crate::call::no_repository_call(
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
    fn dispatch<ArgsType: InvokableLoreArgs + ValidateText + Clone + Send + 'static>(
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
                let reached = handler_reached.clone();
                async move {
                    reached.store(true, Ordering::Release);
                    0
                }
            },
        );
        (status, reached.load(Ordering::Acquire))
    }

    /// A plain text field.
    #[test]
    fn a_string_argument_that_is_not_utf8_is_rejected_before_the_handler_runs() {
        let args = crate::revision_tree::resolve_path::LoreRevisionTreeResolvePathArgs {
            id: 1,
            handle: crate::revision_tree::handle::LoreRevisionTree::INVALID,
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
        let args = crate::revision_tree::resolve_path::LoreRevisionTreeResolvePathArgs {
            id: 1,
            handle: crate::revision_tree::handle::LoreRevisionTree::INVALID,
            path: LoreString::from_str("docs/readme.md"),
        };

        let (status, handler_ran) = dispatch(&LoreGlobalArgs::default(), &args);

        assert_eq!(status, 0);
        assert!(handler_ran, "valid text must reach the verb");
    }

    /// An element of an array of text.
    #[test]
    fn a_text_array_element_that_is_not_utf8_is_rejected() {
        let args = crate::file::LoreFileHashArgs {
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
        let args = crate::storage::put_file::LoreStoragePutFileArgs {
            handle: crate::storage::handle::LoreStore::INVALID,
            items: lore_revision::interface::LoreArray::from_vec(vec![
                crate::storage::put_file::LoreStoragePutFileItem {
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
        let args = crate::revision_tree::add::LoreRevisionTreeAddArgs {
            batch_id: 1,
            handle: crate::revision_tree::handle::LoreRevisionTree::INVALID,
            entries: lore_revision::interface::LoreArray::from_vec(vec![
                crate::revision_tree::add::LoreRevisionTreeAddEntry {
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
        let args = crate::storage::open::LoreStorageOpenArgs {
            remote_config: crate::storage::open::LoreStorageRemoteConfig {
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
        let args = crate::revision_tree::close::LoreRevisionTreeCloseArgs::default();

        let (status, handler_ran) = dispatch(&globals, &args);

        assert_eq!(status, rejected_status());
        assert!(!handler_ran, "the verb must not run on a rejected call");
    }

    /// The rejection names the field so a caller can tell which string to fix.
    #[test]
    fn the_rejection_names_the_failing_field() {
        let args = crate::storage::put_file::LoreStoragePutFileArgs {
            handle: crate::storage::handle::LoreStore::INVALID,
            items: lore_revision::interface::LoreArray::from_vec(vec![
                crate::storage::put_file::LoreStoragePutFileItem::default(),
                crate::storage::put_file::LoreStoragePutFileItem {
                    path: invalid_utf8(),
                    ..Default::default()
                },
            ]),
        };

        let error = validate_call_text(&LoreGlobalArgs::default(), &args)
            .expect_err("the item path must fail");

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
            done: Mutex::new(Some(done_tx)),
        }));
        let context = sink as *const AsyncSink as u64;
        registry().lock().unwrap().insert(context, sink);

        let config = LoreEventCallbackConfig {
            user_context: context,
            func: Some(record_event),
        };

        let args = crate::revision_tree::resolve_path::LoreRevisionTreeResolvePathArgs {
            id: 1,
            handle: crate::revision_tree::handle::LoreRevisionTree::INVALID,
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
}
