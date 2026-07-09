// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//
// End-to-end tests across the FFI boundary. They drive the real C entry
// points (`lore_repository_*` and the `_async` variants) and observe events
// through a real `extern "C"` callback, the only point mocked here. No library
// module is mocked. Each test reads the failure from the enriched `Complete`
// event a consumer would receive.
#![allow(clippy::disallowed_methods)]

mod test_util;

mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::OnceLock;
    use std::sync::mpsc;

    use lore::interface::LoreString;
    use lore::repository::LoreRepositoryCreateArgs;
    use lore::repository::LoreRepositoryStatusArgs;
    use lore_base::error::RepositoryNotFound;
    use lore_base::log::LoreLogLevel;
    use lore_error_set::FfiError;
    use lore_revision::event::LoreEvent;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreEventCallbackConfig;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::repository::RepositoryError;
    use rand::Rng;
    use rand::distr::Alphanumeric;
    use serial_test::serial;

    use super::test_util::TempDir;

    // One trace location copied out of the live event during the callback.
    // After the callback returns, the test reads these owned copies instead of
    // the original FFI pointers.
    #[derive(Clone, PartialEq, Eq, Debug)]
    struct OwnedTraceLocation {
        file: String,
        line: u32,
        column: u32,
        context: String,
    }

    // Everything a consumer keeps from a single `Complete` event, copied into
    // owned storage during the callback. The `extern "C"` callback never lets
    // the original pointers escape its own invocation.
    #[derive(Default)]
    struct CapturedComplete {
        // Number of `Complete` events seen, to assert exactly one arrives.
        complete_count: u32,
        // Set if any mid-stream `Error` event arrives.
        error_event_seen: bool,
        status: i32,
        error_code: i32,
        message: String,
        trace: Vec<OwnedTraceLocation>,
        // Error-level log messages seen, the text a consumer like the CLI prints.
        error_logs: Vec<String>,
    }

    // Per-test sink, keyed by `user_context`, so a real C callback can route
    // its captured copies back to the test that registered it.
    struct Sink {
        captured: Mutex<CapturedComplete>,
        done: Mutex<Option<mpsc::Sender<()>>>,
    }

    fn registry() -> &'static Mutex<HashMap<u64, &'static Sink>> {
        static REGISTRY: OnceLock<Mutex<HashMap<u64, &'static Sink>>> = OnceLock::new();
        REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
    }

    // The FFI boundary. This copies every field a consumer would keep out of
    // the live event into owned storage before returning. It never stores a
    // raw pointer from the event, so the test can read the copies safely after
    // the callback returns (NFR2).
    unsafe extern "C" fn record_event(event: &LoreEvent, user_context: u64) {
        let sink = *registry().lock().unwrap().get(&user_context).unwrap();
        match event {
            LoreEvent::Error(_) => {
                sink.captured.lock().unwrap().error_event_seen = true;
            }
            LoreEvent::Complete(data) => {
                let mut captured = sink.captured.lock().unwrap();
                captured.complete_count += 1;
                captured.status = data.status;
                captured.error_code = data.error.error_code;
                // Copy the message bytes into an owned `String`.
                captured.message = data.error.message.as_str().to_string();
                // Copy each trace location into owned storage.
                captured.trace = data
                    .error
                    .trace_locations
                    .as_slice()
                    .iter()
                    .map(|location| OwnedTraceLocation {
                        file: location.file.as_str().to_string(),
                        line: location.line,
                        column: location.column,
                        context: location.context.as_str().to_string(),
                    })
                    .collect();
            }
            LoreEvent::Log(data) if data.level == LoreLogLevel::Error => {
                sink.captured
                    .lock()
                    .unwrap()
                    .error_logs
                    .push(data.message.as_str().to_string());
            }
            // `End` fires after `Complete`; the async test waits on it.
            LoreEvent::End(_) => {
                if let Some(sender) = sink.done.lock().unwrap().take() {
                    let _ = sender.send(());
                }
            }
            _ => {}
        }
    }

    // Registers a fresh sink under a unique `user_context` and returns the
    // leaked reference plus the callback config wired to it. The sink is leaked
    // so the `'static` C callback can hold a stable reference; the test process
    // tears it down.
    fn make_sink() -> (&'static Sink, LoreEventCallbackConfig, mpsc::Receiver<()>) {
        let (done_tx, done_rx) = mpsc::channel();
        let sink: &'static Sink = Box::leak(Box::new(Sink {
            captured: Mutex::new(CapturedComplete::default()),
            done: Mutex::new(Some(done_tx)),
        }));
        let context = sink as *const Sink as u64;
        registry().lock().unwrap().insert(context, sink);
        let config = LoreEventCallbackConfig {
            user_context: context,
            func: Some(record_event),
        };
        (sink, config, done_rx)
    }

    fn random_name() -> String {
        rand::rng()
            .sample_iter(&Alphanumeric)
            .take(16)
            .map(char::from)
            .collect()
    }

    fn missing_repository_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("lore-missing-repo-{}", random_name()))
    }

    // The unique trailing component of a missing-repo path. It carries no path
    // separators, so a "message names the path" check holds the same on every
    // OS regardless of how the full path is rendered (e.g. Windows backslashes).
    fn missing_repo_name(path: &std::path::Path) -> String {
        path.file_name().unwrap().to_string_lossy().into_owned()
    }

    fn status_args() -> LoreRepositoryStatusArgs {
        LoreRepositoryStatusArgs {
            staged: 1,
            scan: 0,
            check_dirty: 0,
            reset: 0,
            sync_point: 0,
            revision_only: 0,
            count: 0,
            paths: LoreArray::default(),
        }
    }

    fn create_args(name: &str) -> LoreRepositoryCreateArgs {
        LoreRepositoryCreateArgs {
            repository_url: name.into(),
            id: LoreString::default(),
            description: LoreString::default(),
            use_shared_store: 0,
            shared_store_path: LoreString::default(),
        }
    }

    fn globals_for(path: &std::path::Path) -> LoreGlobalArgs {
        LoreGlobalArgs {
            repository_path: path.display().to_string().into(),
            offline: 1,
            ..Default::default()
        }
    }

    // The real repository-not-found FFI code, derived rather than hard-coded so
    // the test fails if the mapping ever moves. It must be the spec's 45 and
    // never the old flat 1.
    fn repository_not_found_code(path: &std::path::Path) -> i32 {
        RepositoryError::from(RepositoryNotFound {
            repository: path.display().to_string(),
        })
        .ffi_code()
    }

    // A failing op delivers one error-bearing event, the enriched `Complete`. A
    // consumer reconstructs the file, line, column, and context from the trace
    // carried on that event. The sync entry point also returns the FFI code.
    #[serial]
    #[test]
    fn failing_op_delivers_one_complete_with_reconstructable_trace() {
        let (sink, config, _done_rx) = make_sink();
        let path = missing_repository_path();

        let status =
            lore::interface::lore_repository_status(&globals_for(&path), &status_args(), config);

        let captured = sink.captured.lock().unwrap();

        assert!(
            !captured.error_event_seen,
            "a failing op must not emit an Error event"
        );
        // Exactly one error-bearing event reaches the consumer.
        assert_eq!(captured.complete_count, 1, "exactly one Complete event");

        let expected_code = repository_not_found_code(&path);
        // The sync entry point returns the FFI code on failure, matching status.
        assert_eq!(status, expected_code);
        assert_eq!(captured.status, expected_code);
        assert_eq!(captured.error_code, expected_code);
        assert!(
            !captured.message.is_empty(),
            "the detail must carry the error message"
        );

        // The consumer reconstructs the trace from the event alone. With
        // `track-locations` on, the failure carries at least one location with
        // a real file, line, and column.
        assert!(
            !captured.trace.is_empty(),
            "the failure must carry at least one trace location"
        );
        let first = &captured.trace[0];
        assert!(!first.file.is_empty(), "trace location must name a file");
        assert!(first.line > 0, "trace location must carry a line");
    }

    // A failing op routes its message and trace through an error-level `Log`
    // event, the text a consumer like the CLI prints. Installing the log bridge
    // makes the library forward logs to the event dispatcher. The message names
    // the failure and the trace follows on indented `  at file:line:column`
    // lines.
    #[serial]
    #[test]
    fn failing_op_logs_error_message_with_trace() {
        lore::log::initialize();
        let (sink, config, _done_rx) = make_sink();
        let path = missing_repository_path();

        lore::interface::lore_repository_status(&globals_for(&path), &status_args(), config);

        let captured = sink.captured.lock().unwrap();
        let logged = captured
            .error_logs
            .iter()
            .find(|message| message.contains(&missing_repo_name(&path)))
            .unwrap_or_else(|| {
                panic!(
                    "a failing op must log an error message naming the missing path, got: {:?}",
                    captured.error_logs
                )
            });

        // The trace follows the message on indented lines, each naming a source
        // location.
        assert!(
            logged.contains("\n  at "),
            "the log must include the trace on indented lines, got: {logged}"
        );
    }

    // The synchronous entry point returns `0` when its op succeeds.
    #[serial]
    #[test]
    fn sync_entry_point_returns_zero_on_success() {
        let (sink, config, _done_rx) = make_sink();
        let tempdir = TempDir::new("lore-ffi-error-test-");
        let name = random_name();

        let status = lore::interface::lore_repository_create(
            &globals_for(tempdir.path()),
            &create_args(&name),
            config,
        );

        assert_eq!(status, 0, "a succeeding op returns 0");

        let captured = sink.captured.lock().unwrap();
        assert_eq!(captured.complete_count, 1, "exactly one Complete event");
        assert_eq!(captured.status, 0);
        assert_eq!(captured.error_code, 0);
        assert!(captured.message.is_empty());
        assert!(captured.trace.is_empty());
    }

    // The asynchronous entry point returns `void`; the failure code arrives
    // only through `Complete.status`. A missing-repository call reports the real
    // FFI code (45), not the old flat `1`.
    #[serial]
    #[test]
    fn async_entry_point_delivers_code_through_complete_status() {
        let (sink, config, done_rx) = make_sink();
        let path = missing_repository_path();

        // The async entry point returns nothing.
        let returned: () = lore::interface::lore_repository_status_async(
            &globals_for(&path),
            &status_args(),
            config,
        );
        assert_eq!(returned, ());

        // Block until the spawned task has flushed `Complete` and `End`.
        done_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the async task must complete");

        let captured = sink.captured.lock().unwrap();
        assert_eq!(captured.complete_count, 1, "exactly one Complete event");
        assert!(
            !captured.error_event_seen,
            "a failing async op must not emit an Error event"
        );

        let expected_code = repository_not_found_code(&path);
        assert_eq!(expected_code, 45, "the missing-repository code is 45");
        assert_ne!(expected_code, 1, "the code must be real, not the flat 1");
        // The code arrives only through the event status on the async path.
        assert_eq!(captured.status, expected_code);
        assert_eq!(captured.error_code, expected_code);
    }

    // NFR2: the library owns the error-detail memory; its pointers are valid
    // only for the single callback invocation. A consumer that copies the
    // detail out during the callback can read its copy after the callback has
    // returned. This test copies inside `record_event` and asserts only against
    // the owned copies here, never the original FFI pointers.
    #[serial]
    #[test]
    fn copied_detail_is_readable_after_callback_returns() {
        let (sink, config, _done_rx) = make_sink();
        let path = missing_repository_path();

        let status =
            lore::interface::lore_repository_status(&globals_for(&path), &status_args(), config);

        // The callback has returned. From here the test reads only the owned
        // copies the callback made; the original event pointers are gone.
        let captured = sink.captured.lock().unwrap();
        let expected_code = repository_not_found_code(&path);

        assert_eq!(status, expected_code);
        assert_eq!(captured.status, expected_code);
        assert_eq!(captured.error_code, expected_code);

        // The copied message is still readable and names the missing path.
        assert!(
            captured.message.contains(&missing_repo_name(&path)),
            "the copied message must name the missing repository path: {}",
            captured.message
        );

        // The copied trace is still readable and well-formed.
        assert!(
            !captured.trace.is_empty(),
            "the copied trace must carry at least one location"
        );
        for location in &captured.trace {
            assert!(!location.file.is_empty(), "copied file must be readable");
            assert!(location.line > 0, "copied line must be readable");
        }
    }
}
