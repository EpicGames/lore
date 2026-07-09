// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use lore_base::error::RepositoryNotFound;
use lore_base::runtime::LORE_CONTEXT;
use lore_error_set::FfiError;
use lore_error_set::HasTrace;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorDetail;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::lore::execution_context;
use lore_revision::lore_warn;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository;
use lore_revision::repository::RepositoryAccess;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryError;
use lore_revision::repository::RepositoryFormat;
pub use lore_revision::repository::RepositoryWriteToken;
use lore_revision::util;

use crate::interface::LoreEventCallback;
use crate::util::log_command_done;
use crate::util::log_command_info;

pub fn setup_execution(
    globals: LoreGlobalArgs,
    callback: LoreEventCallback,
) -> Arc<ExecutionContext> {
    Arc::new(ExecutionContext::new_client(
        globals,
        EventDispatcher::new(callback),
    ))
}

/// Read-only repository call. No `RepositoryWriteToken` is minted, so
/// closures cannot name one — write-gated leaf operations fail at compile
/// time.
///
/// ```compile_fail
/// # use std::sync::Arc;
/// # use lore::call::repository_call_read;
/// # use lore::call::RepositoryWriteToken;
/// # use lore_revision::repository::RepositoryContext;
/// # use lore_revision::interface::LoreGlobalArgs;
/// # async fn demo(globals: LoreGlobalArgs, callback: lore::interface::LoreEventCallback) {
/// repository_call_read(globals, callback, (), "demo",
///     |_repo: Arc<RepositoryContext>, _token: RepositoryWriteToken, _args: ()|
///         async move { Ok::<(), std::io::Error>(()) },
/// ).await;
/// # }
/// ```
pub async fn repository_call_read<Arg, T, F, Fut, ResT, ErrT>(
    globals: LoreGlobalArgs,
    callback: LoreEventCallback,
    args: Arg,
    caller: T,
    command: F,
) -> i32
where
    ErrT: EventError + FfiError + HasTrace,
    Arg: std::fmt::Debug,
    F: FnOnce(Arc<RepositoryContext>, Arg) -> Fut,
    Fut: Future<Output = Result<ResT, ErrT>> + 'static,
{
    let (repository_path, execution) = match prepare_repository_call(globals, callback).await {
        Ok(v) => v,
        Err(status) => return status,
    };

    LORE_CONTEXT
        .scope(execution, async move {
            log_command_info(&caller, &args);
            let time_start = Instant::now();

            let detail;
            let mut weak_repository = None;
            match repository::load_and_connect_with_token(
                &repository_path,
                RepositoryAccess::ReadOnly,
                None,
            )
            .await
            {
                Ok(repository) => {
                    detail = LoreErrorDetail::from_result(command(repository.clone(), args).await);
                    weak_repository = Some(post_command_cleanup(repository).await);
                }
                Err(err) => {
                    detail = LoreErrorDetail::from_error(&err);
                }
            }

            check_no_lingering_repository(weak_repository);

            log_command_done(&caller, time_start);
            execution_context().dispatcher.complete(detail).await
        })
        .await
}

/// Write repository call. Mints a [`RepositoryWriteToken`] for the callback
/// and shares a sibling with the [`RepositoryContext`] so opportunistic
/// leaf-fetch writes (mtime cache, status flush) still see it.
///
/// Acquiring the token serializes writes in-process on a per-path
/// `tokio::sync::Mutex`; reads skip this. Cross-process exclusion is the
/// `FSLock` in `load_and_connect`.
pub async fn repository_call_write<Arg, T, F, Fut, ResT, ErrT>(
    globals: LoreGlobalArgs,
    callback: LoreEventCallback,
    args: Arg,
    caller: T,
    command: F,
) -> i32
where
    ErrT: EventError + FfiError + HasTrace,
    Arg: std::fmt::Debug,
    F: FnOnce(Arc<RepositoryContext>, RepositoryWriteToken, Arg) -> Fut,
    Fut: Future<Output = Result<ResT, ErrT>> + 'static,
{
    let (repository_path, execution) = match prepare_repository_call(globals, callback).await {
        Ok(v) => v,
        Err(status) => return status,
    };

    let token = RepositoryWriteToken::acquire(&repository_path).await;
    let context_token = token.share();

    LORE_CONTEXT
        .scope(execution, async move {
            log_command_info(&caller, &args);
            let time_start = Instant::now();

            let detail;
            let mut weak_repository = None;
            match repository::load_and_connect_with_token(
                &repository_path,
                RepositoryAccess::ReadWrite,
                Some(context_token),
            )
            .await
            {
                Ok(repository) => {
                    detail = LoreErrorDetail::from_result(
                        command(repository.clone(), token, args).await,
                    );
                    weak_repository = Some(post_command_cleanup(repository).await);
                }
                Err(err) => {
                    detail = LoreErrorDetail::from_error(&err);
                }
            }

            check_no_lingering_repository(weak_repository);

            log_command_done(&caller, time_start);
            execution_context().dispatcher.complete(detail).await
        })
        .await
}

/// Repository call that doesn't open stores. For notification /
/// config-introspection commands that need a `RepositoryContext` but neither
/// read nor write stores; skips the `FSLock` and never mints a write token.
pub async fn repository_call_no_store<Arg, T, F, Fut, ResT, ErrT>(
    globals: LoreGlobalArgs,
    callback: LoreEventCallback,
    args: Arg,
    caller: T,
    command: F,
) -> i32
where
    ErrT: EventError + FfiError + HasTrace,
    Arg: std::fmt::Debug,
    F: FnOnce(Arc<RepositoryContext>, Arg) -> Fut,
    Fut: Future<Output = Result<ResT, ErrT>> + 'static,
{
    let (repository_path, execution) = match prepare_repository_call(globals, callback).await {
        Ok(v) => v,
        Err(status) => return status,
    };

    LORE_CONTEXT
        .scope(execution, async move {
            log_command_info(&caller, &args);
            let time_start = Instant::now();

            let detail;
            let mut weak_repository = None;
            match repository::load_and_connect_with_token(
                &repository_path,
                RepositoryAccess::NoStore,
                None,
            )
            .await
            {
                Ok(repository) => {
                    detail = LoreErrorDetail::from_result(command(repository.clone(), args).await);
                    weak_repository = Some(post_command_cleanup(repository).await);
                }
                Err(err) => {
                    detail = LoreErrorDetail::from_error(&err);
                }
            }

            check_no_lingering_repository(weak_repository);

            log_command_done(&caller, time_start);
            execution_context().dispatcher.complete(detail).await
        })
        .await
}

/// On `Err`, the error has already been dispatched to the callback.
async fn prepare_repository_call(
    mut globals: LoreGlobalArgs,
    callback: LoreEventCallback,
) -> Result<(PathBuf, Arc<ExecutionContext>), i32> {
    let repository_path =
        if let Ok(path) = util::path::make_absolute(globals.repository_path.as_str()) {
            globals.repository_path = path.display().to_string().into();
            path
        } else {
            PathBuf::from(globals.repository_path.as_str())
        };

    let execution = setup_execution(globals, callback);

    let format = RepositoryFormat::detect(&repository_path);
    let dot_dir = format.dot_dir();
    if !repository_path.join(dot_dir).is_dir() {
        let err = RepositoryError::from(RepositoryNotFound {
            repository: repository_path.display().to_string(),
        });
        // A pre-command failure reports the same status, return value, and
        // detail as a command failure. Complete inside the execution scope so
        // the failure log routes to the dispatcher.
        let status = LORE_CONTEXT
            .scope(execution, async move {
                execution_context()
                    .dispatcher
                    .complete(LoreErrorDetail::from_error(&err))
                    .await
            })
            .await;
        return Err(status);
    }

    Ok((repository_path, execution))
}

async fn post_command_cleanup(
    repository: Arc<RepositoryContext>,
) -> std::sync::Weak<RepositoryContext> {
    // Snapshot the state so we don't force a pending connect to resolve
    // just for teardown. session_stop fires when the last Arc ref drops;
    // local-only commands never connect and have nothing to release.
    if let lore_revision::repository::RemoteStatus::Connected(remote) =
        repository.remote_status().await
    {
        let correlation_id = execution_context().globals().correlation_id.to_string();
        remote.release_session(repository.id, &correlation_id);
    }

    let sync_data = execution_context().globals().sync_data();
    repository.try_spawn_post_command_flush(sync_data);

    if let Some(duration) = execution_context().globals().store_keep_alive_duration() {
        repository.spawn_keep_alive(duration);
    }

    Arc::downgrade(&repository)
}

fn check_no_lingering_repository(weak: Option<std::sync::Weak<RepositoryContext>>) {
    if let Some(repository) = weak
        && repository.strong_count() > 0
    {
        // A stray strong reference means the command spawned a task that
        // outlives completion and is holding the repository context.
        lore_warn!("Repository has strong reference remaining after completion");
        debug_assert!(
            repository.strong_count() == 0,
            "Repository has strong reference remaining after completion"
        );
    }
}

pub async fn no_repository_call<Arg, T, F, Fut, ResT, ErrT>(
    globals: LoreGlobalArgs,
    callback: LoreEventCallback,
    args: Arg,
    caller: T,
    command: F,
) -> i32
where
    ErrT: EventError + FfiError + HasTrace,
    Arg: std::fmt::Debug,
    F: FnOnce(Arg) -> Fut,
    Fut: Future<Output = Result<ResT, ErrT>> + 'static,
{
    let execution = setup_execution(globals, callback);

    LORE_CONTEXT
        .scope(execution, async move {
            log_command_info(&caller, &args);

            let time_start = Instant::now();

            let detail = LoreErrorDetail::from_result(command(args).await);

            log_command_done(&caller, time_start);
            execution_context().dispatcher.complete(detail).await
        })
        .await
}

/// Shared test harness for the dispatch wrappers. The storage and
/// revision-tree dispatch helpers capture the same callback event shape, so
/// the capture enum and sink helpers live here once.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Arc;
    use std::sync::Mutex;

    use lore_revision::event::LoreCompleteEventData;
    use lore_revision::event::LoreEvent;

    use crate::interface::LoreEventCallback;

    /// The full event a callback receives, kept verbatim so a test can read the
    /// `Complete` detail (code, message, trace) a real consumer would see.
    #[derive(Clone)]
    pub(crate) enum CapturedEvent {
        Error,
        Complete(LoreCompleteEventData),
        Other,
    }

    impl CapturedEvent {
        pub(crate) fn from_event(event: &LoreEvent) -> Self {
            match event {
                LoreEvent::Error(_) => Self::Error,
                LoreEvent::Complete(data) => Self::Complete(data.clone()),
                _ => Self::Other,
            }
        }
    }

    /// Build a callback that pushes each event into the shared sink.
    pub(crate) fn make_callback(sink: Arc<Mutex<Vec<CapturedEvent>>>) -> LoreEventCallback {
        Some(Box::new(move |event: &LoreEvent| {
            sink.lock().unwrap().push(CapturedEvent::from_event(event));
        }))
    }

    /// Collect just the `Complete` event payloads from a captured sink.
    pub(crate) fn completes(events: &[CapturedEvent]) -> Vec<LoreCompleteEventData> {
        events
            .iter()
            .filter_map(|e| match e {
                CapturedEvent::Complete(data) => Some(data.clone()),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn has_error_event(events: &[CapturedEvent]) -> bool {
        events.iter().any(|e| matches!(e, CapturedEvent::Error))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use lore_base::error::NotFound;
    use lore_error_set::Location;
    use lore_error_set::prelude::*;
    use lore_revision::event::LoreEvent;
    use lore_revision::interface::LoreGlobalArgs;

    use super::*;

    // A concrete `#[error_set]` error for the wrapper closures. Its `NotFound`
    // variant wraps `lore_base::error::NotFound`, which carries error code 13, so
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

        // The status holds the error code (13 for `NotFound`) and the detail is
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
}
