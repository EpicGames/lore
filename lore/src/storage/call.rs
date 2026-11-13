// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Dispatch helpers for the content-addressed storage API.
//!
//! `storage_call_with` runs an op on a storage handle behind the handle's in-flight guard and
//! completes it with one `Complete` event; `storage_call` runs it with a dispatcher that sends
//! every event.

use std::sync::Arc;
use std::time::Instant;

use lore_base::error::InvalidArguments;
use lore_base::runtime::LORE_CONTEXT;
use lore_error_set::FfiError;
use lore_error_set::HasTrace;
use lore_revision::event::LoreErrorDetail;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::lore::execution_context;

use crate::call::setup_execution;
use crate::interface::LoreEventCallback;
use crate::storage::handle::LoreStore;
use crate::storage::store::OpGuard;
use crate::storage::store::StoreInternal;
use crate::util::log_command_done;
use crate::util::log_command_info;

/// Run a storage-API op as [`storage_call_with`] does, in an execution context whose events all go
/// to `callback`.
#[lore_macro::test_pub]
#[inline]
pub(crate) fn storage_call<Arg, T, F, Fut, ResT, ErrT>(
    globals: LoreGlobalArgs,
    callback: LoreEventCallback,
    store_handle: LoreStore,
    args: Arg,
    caller: T,
    command: F,
) -> impl Future<Output = i32>
where
    ErrT: FfiError + HasTrace + std::fmt::Display + From<InvalidArguments>,
    Arg: std::fmt::Debug,
    F: FnOnce(Arc<StoreInternal>, Arg) -> Fut,
    Fut: Future<Output = Result<ResT, ErrT>> + 'static,
{
    storage_call_with(
        move || setup_execution(globals, callback),
        store_handle,
        args,
        caller,
        command,
    )
}

/// Run an op on a storage handle behind the handle's in-flight guard.
///
/// The helper:
/// 1. Sets up an `ExecutionContext` with `execution` and enters its `LORE_CONTEXT` scope.
///    `execution` runs at the first poll, so the context of a relayed call reads its caller's
///    environment.
/// 2. Acquires an [`OpGuard`] for the handle — if the handle is unknown
///    or already closed, completes with `InvalidArguments` and returns
///    its error code without invoking the op impl.
/// 3. Passes a cloned `Arc<StoreInternal>` to the op impl (ownership
///    transferred; the impl can fan it out to spawned tasks).
/// 4. Translates the impl's `Result` into a `Complete{status}` event.
/// 5. Drops the `OpGuard` only *after* `Complete` fires — so the
///    in-flight counter decrement orders after the last result event.
///
/// # Contract expected of `command`
///
/// All work (including spawned tasks) the op initiates must complete
/// before the returned future resolves — no background work outlives
/// a data op. Use `JoinSet` / `join_all` to await spawned futures
/// before returning.
pub(crate) async fn storage_call_with<Arg, T, S, F, Fut, ResT, ErrT>(
    execution: S,
    store_handle: LoreStore,
    args: Arg,
    caller: T,
    command: F,
) -> i32
where
    ErrT: FfiError + HasTrace + std::fmt::Display + From<InvalidArguments>,
    Arg: std::fmt::Debug,
    S: FnOnce() -> Arc<ExecutionContext>,
    F: FnOnce(Arc<StoreInternal>, Arg) -> Fut,
    Fut: Future<Output = Result<ResT, ErrT>> + 'static,
{
    LORE_CONTEXT
        .scope(execution(), async move {
            let Some(guard) = OpGuard::enter(store_handle) else {
                let err = ErrT::from(InvalidArguments {
                    reason: "storage handle is unknown or has been closed".into(),
                });
                return execution_context()
                    .dispatcher
                    .complete(LoreErrorDetail::from_error(&err))
                    .await;
            };

            log_command_info(&caller, &args);
            let time_start = Instant::now();

            let store = guard.store_clone();
            let detail = LoreErrorDetail::from_result(&command(store, args).await);

            log_command_done(&caller, time_start);
            let status = execution_context().dispatcher.complete(detail).await;
            // Explicit drop after Complete: a closer waiting on the in-flight counter must
            // not be woken before Complete has fired.
            drop(guard);
            status
        })
        .await
}
