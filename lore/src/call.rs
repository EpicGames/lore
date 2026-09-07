// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
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
pub use lore_revision::repository::RepositoryWriteToken;
use lore_revision::repository::get_dot_lore_path;
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

            let (detail, weak_repository) = 'call: {
                let repository = match repository::load_and_connect_with_token(
                    &repository_path,
                    RepositoryAccess::ReadOnly,
                    None,
                )
                .await
                {
                    Ok(repository) => repository,
                    Err(err) => break 'call (LoreErrorDetail::from_error(&err), None),
                };
                let detail = LoreErrorDetail::from_result(command(repository.clone(), args).await);
                (detail, Some(post_command_cleanup(repository).await))
            };

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
/// `tokio::sync::Mutex`; reads skip this. Cross-process exclusion is the store
/// claims `load_and_connect` takes, and the repository `FSLock` it takes inside them.
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

    LORE_CONTEXT
        .scope(execution, async move {
            let token = RepositoryWriteToken::acquire(&repository_path).await;
            let context_token = token.share();

            log_command_info(&caller, &args);
            let time_start = Instant::now();

            let (detail, weak_repository) = 'call: {
                let repository = match repository::load_and_connect_with_token(
                    &repository_path,
                    RepositoryAccess::ReadWrite,
                    Some(context_token),
                )
                .await
                {
                    Ok(repository) => repository,
                    Err(err) => break 'call (LoreErrorDetail::from_error(&err), None),
                };
                let detail =
                    LoreErrorDetail::from_result(command(repository.clone(), token, args).await);
                (detail, Some(post_command_cleanup(repository).await))
            };

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

            let (detail, weak_repository) = 'call: {
                let repository = match repository::load_and_connect_with_token(
                    &repository_path,
                    RepositoryAccess::NoStore,
                    None,
                )
                .await
                {
                    Ok(repository) => repository,
                    Err(err) => break 'call (LoreErrorDetail::from_error(&err), None),
                };
                let detail = LoreErrorDetail::from_result(command(repository.clone(), args).await);
                (detail, Some(post_command_cleanup(repository).await))
            };

            check_no_lingering_repository(weak_repository);

            log_command_done(&caller, time_start);
            execution_context().dispatcher.complete(detail).await
        })
        .await
}

/// Resolves `globals.repository_path` against the call's working directory and records the result
/// in `globals`. Left as given when it cannot be resolved.
pub(crate) fn resolve_repository_path(globals: &mut LoreGlobalArgs) -> PathBuf {
    match util::path::make_absolute_from(
        globals.repository_path.as_str(),
        globals.working_directory().map(Path::new),
    ) {
        Ok(path) => {
            globals.repository_path = path.display().to_string().into();
            path
        }
        Err(_) => PathBuf::from(globals.repository_path.as_str()),
    }
}

/// On `Err`, the error has already been dispatched to the callback.
async fn prepare_repository_call(
    mut globals: LoreGlobalArgs,
    callback: LoreEventCallback,
) -> Result<(PathBuf, Arc<ExecutionContext>), i32> {
    let repository_path = resolve_repository_path(&mut globals);

    let execution = setup_execution(globals, callback);

    let dotpath = get_dot_lore_path(&repository_path).map_err(|err| err.ffi_code())?;
    if !dotpath.is_dir() {
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
    ErrT: FfiError + HasTrace + std::fmt::Display,
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
