// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::global::CONFIG as GLOBAL_CONFIG;
use lore_revision::global::GlobalConfig;
use lore_revision::global::get_global_config_dir;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::lore_debug;
use parking_lot::RwLock;

use crate::args::InvokableLoreArgs;
use crate::interface::LoreEventCallback;
use crate::interface::LoreEventCallbackConfig;
use crate::remote::call::service_call;
use crate::remote::process;

const USE_SERVICE_VAR: &str = "LORE_USE_SERVICE";

/// Caches `use_service_automatically` for the life of the process. Every public
/// API entry point consults it, and reading it means parsing the global TOML
/// config, so it must not happen per call.
///
/// The cache lives for the whole process and is only refreshed by a write in
/// this process (see [`invalidate_use_service_cache`]). A change made elsewhere
/// — an external `lore service set-use-automatically`, or a hand-edited config —
/// is not observed until the process restarts. That is acceptable because the
/// setting is a deploy-time choice that rarely changes under a running process,
/// and the CLI runs one command per process regardless.
static USE_SERVICE: RwLock<Option<bool>> = RwLock::new(None);

/// Drops the cached setting so the next call in this process rereads it. Called
/// after the setting is written here.
///
/// This refreshes only writes made by this process, and it races a concurrent
/// reader: a [`use_service`] that loads the old config and fills the cache after
/// this runs can leave the stale value in place until the next write. The window
/// is narrow and self-heals on any later write; only a long-lived multi-threaded
/// embedder that flips the setting mid-run is exposed.
pub(crate) fn invalidate_use_service_cache() {
    *USE_SERVICE.write() = None;
}

/// Whether a `LORE_USE_SERVICE` value turns the service on. Off values, matched
/// case-insensitively with surrounding whitespace ignored, are an empty value
/// and `0`, `false`, `f`, `no`, `n`, and `off`; any other value is on.
fn use_service_from_value(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "f" | "no" | "n" | "off"
    )
}

/// `LORE_USE_SERVICE` overrides the stored setting when set, so that tests and
/// one-off invocations can route through the service without writing config.
/// See [`use_service_from_value`] for how the value is interpreted.
fn use_service_override() -> Option<bool> {
    std::env::var(USE_SERVICE_VAR)
        .ok()
        .map(|value| use_service_from_value(&value))
}

/// Whether calls should be routed through the service process.
///
/// Test builds never read the stored setting, so that the suite runs against a
/// clean global config rather than whatever the developer has configured on
/// their own machine. `LORE_USE_SERVICE` is still honoured, so a test that wants
/// the service path can ask for it explicitly.
pub(crate) async fn use_service() -> bool {
    if let Some(value) = use_service_override() {
        return value;
    }
    if cfg!(test) {
        return false;
    }
    if let Some(cached) = *USE_SERVICE.read() {
        return cached;
    }
    let enabled = GlobalConfig::load()
        .await
        .is_ok_and(|config| config.use_service_automatically());
    *USE_SERVICE.write() = Some(enabled);
    enabled
}

/// Answers the same question as [`use_service`] without a runtime, for callers
/// that must decide before one exists. Reads the global config directly rather
/// than through its async loader, and fills the same cache, so a later
/// [`use_service`] cannot reach a different answer.
pub fn will_use_service() -> bool {
    if let Some(value) = use_service_override() {
        return value;
    }
    if cfg!(test) {
        return false;
    }
    if let Some(cached) = *USE_SERVICE.read() {
        return cached;
    }
    let enabled = get_global_config_dir()
        .ok()
        .map(|dir| dir.join(GLOBAL_CONFIG))
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| toml::from_str::<GlobalConfig>(&text).ok())
        .is_some_and(|config| config.use_service_automatically());
    *USE_SERVICE.write() = Some(enabled);
    enabled
}

/// Sizes the shared runtime before an FFI entry point first builds it: lean when
/// the call will be relayed to the service, so an embedder whose first call is a
/// routed API does not build the full runtime it never uses. A no-op once the
/// runtime exists, and when the service is not in use. The routed operations all
/// relay and the local ones (`service start`/`stop`/`set-use-automatically`) are
/// lightweight, so the lean runtime suits both.
fn size_runtime_for_dispatch() {
    if will_use_service() {
        drop(lore_base::runtime::runtime_with_settings(Some(
            lore_base::runtime::TokioSettings::relay_only(),
        )));
    }
}

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
    size_runtime_for_dispatch();
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
    size_runtime_for_dispatch();
    let callback = lore_revision::event::convert_event_callback(callback);
    let globals = globals.clone();
    let args = args.clone();
    drop(lore_base::lore_spawn!(handler(globals, args, callback)));
}

/// Runs a call either in the service process or locally, according to
/// [`use_service`]. A call already executing inside the service always runs
/// locally, so that it cannot dispatch back into the service it is running in.
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
    if use_service().await && !process::running_as_service() {
        lore_debug!(
            "Using Lore service process for {}",
            std::any::type_name::<ArgsType>()
        );
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

    #[test]
    fn use_service_value_parsing() {
        for on in ["1", "true", "TRUE", "yes", "on", "enabled", "  1 "] {
            assert!(
                use_service_from_value(on),
                "{on:?} should enable the service"
            );
        }
        for off in [
            "", "  ", "0", "false", "FALSE", "f", "no", "n", "off", " off ",
        ] {
            assert!(
                !use_service_from_value(off),
                "{off:?} should disable the service"
            );
        }
    }

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
