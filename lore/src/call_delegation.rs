// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::interface::LoreGlobalArgs;

use crate::args::InvokableLoreArgs;
use crate::interface::LoreEventCallback;
use crate::interface::LoreEventCallbackConfig;
use crate::remote::call::service_call;

pub(crate) fn run_synchronously<
    ArgsType: InvokableLoreArgs + Clone + Send + 'static,
    Handler: Fn(LoreGlobalArgs, ArgsType, LoreEventCallback) -> Fut,
    Fut: Future<Output = i32> + Send + 'static,
>(
    globals: &LoreGlobalArgs,
    args: &ArgsType,
    callback: LoreEventCallbackConfig,
    handler: Handler,
) -> i32 {
    let callback = lore_revision::event::convert_event_callback(callback);
    let globals = globals.clone();
    let args = args.clone();
    crate::runtime().block_on(handler(globals, args, callback))
}

pub(crate) fn run_asynchronously<
    ArgsType: InvokableLoreArgs + Clone + Send + 'static,
    Handler: Fn(LoreGlobalArgs, ArgsType, LoreEventCallback) -> Fut,
    Fut: Future<Output = i32> + Send + 'static,
>(
    globals: &LoreGlobalArgs,
    args: &ArgsType,
    callback: LoreEventCallbackConfig,
    handler: Handler,
) {
    let callback = lore_revision::event::convert_event_callback(callback);
    let globals = globals.clone();
    let args = args.clone();
    drop(lore_base::lore_spawn!(handler(globals, args, callback)));
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
    use std::sync::Mutex;
    use std::sync::OnceLock;
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
}
