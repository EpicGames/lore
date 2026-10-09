// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::pin::Pin;
use std::pin::pin;

use lore_base::error::InvalidArguments;
use lore_base::error::ShutDown;
use lore_base::text::TextNotUtf8;
use lore_base::text::ValidateText;
use lore_error_set::prelude::*;
use lore_revision::event::EventError;
use lore_revision::event::LoreCompleteEventData;
use lore_revision::event::LoreEndEventData;
use lore_revision::event::LoreErrorDetail;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreGlobalArgs;

use crate::args::LoreArgs;
use crate::interface::LoreEventCallback;
use crate::interface::LoreEventCallbackConfig;
use crate::remote::call::service_call;
use crate::remote::command::LoreCommand;
use crate::remote::service_process::service_in_use;
use crate::remote::service_process::service_in_use_blocking;

/// Rejection of a call whose arguments are malformed, before the verb runs.
#[lore_macro::test_pub]
#[error_set]
pub(crate) enum ArgumentError {
    InvalidArguments,
}

impl EventError for ArgumentError {}

/// Rejection of a call that arrived after `lore_shutdown()`.
#[error_set]
pub(crate) enum ShutdownError {
    ShutDown,
}

impl EventError for ShutdownError {}

/// Emits Complete and End events if called after shutdown. Shutdown is terminal
/// and terminates the runtime. Therefore we cannot use the normal
/// `crate::runtime().block_on(reject_call(...))` mechanism to emit the error events.
fn reject_after_shutdown(callback: LoreEventCallbackConfig) -> i32 {
    let error = ShutdownError::from(ShutDown);
    let status = error.ffi_code();
    lore_base::lore_warn!("{error}");

    if let Some(callback) = lore_revision::event::convert_event_callback(callback) {
        callback(&LoreEvent::Complete(LoreCompleteEventData {
            status,
            error: LoreErrorDetail::from_error(&error),
        }));
        callback(&LoreEvent::End(LoreEndEventData::default()));
    }

    status
}

/// Check every text field a call carries, so a handler can read its arguments
/// as `&str`.
///
/// The C boundary accepts any bytes for a string. Checking the whole call here,
/// once, keeps a bad encoding a uniform argument rejection instead of leaving
/// each verb to catch it — or to miss it and read invalid text.
#[lore_macro::test_pub]
pub(crate) fn validate_call_text<ArgsType: ValidateText>(
    globals: &LoreGlobalArgs,
    args: &ArgsType,
) -> Result<(), ArgumentError> {
    globals
        .validate_text()
        .map_err(|error: TextNotUtf8| error.inside("globals"))
        .and_then(|()| args.validate_text())
        .map_err(|error| ArgumentError::from(InvalidArguments::from(error)))
}

/// Runs a C API call to completion: checks its arguments, then hands its command to `run`, which
/// blocks until the command finishes.
///
/// Generic over the arguments only for the check and the conversion to a command, so running the
/// command is compiled once for each `run` rather than once for each C API function.
#[lore_macro::test_pub]
pub(crate) fn run_synchronously<ArgsType, Run>(
    globals: &LoreGlobalArgs,
    args: &ArgsType,
    callback: LoreEventCallbackConfig,
    run: Run,
) -> i32
where
    ArgsType: ValidateText + Clone + Into<LoreCommand>,
    Run: FnOnce(LoreGlobalArgs, LoreCommand, LoreEventCallback) -> i32,
{
    let command = validate_call_text(globals, args).map(|()| args.clone().into());
    run_command_synchronously(globals, command, callback, run)
}

fn run_command_synchronously<Run>(
    globals: &LoreGlobalArgs,
    command: Result<LoreCommand, ArgumentError>,
    callback: LoreEventCallbackConfig,
    run: Run,
) -> i32
where
    Run: FnOnce(LoreGlobalArgs, LoreCommand, LoreEventCallback) -> i32,
{
    // Ahead of the sizing below, which would build the runtime that shutdown is
    // taking away.
    if lore_base::runtime::runtime_shutdown_started() {
        return reject_after_shutdown(callback);
    }
    // Every entry point here reaches the runtime, and by the time a call is
    // dispatched it has been built, so a relaying process is sized before then.
    // A no-op once one exists, which is the case for a caller that sized it
    // itself — the client does, since it builds the runtime before calling in.
    crate::size_threads_for_relaying();
    let callback = lore_revision::event::convert_event_callback(callback);
    let command = match command {
        Ok(command) => command,
        Err(error) => {
            return crate::runtime().block_on(reject_call(globals.clone(), callback, error));
        }
    };
    let mut globals = globals.clone();
    // Resolving the credentials reads their text, so it follows the check above.
    if let Err(error) = globals.validate() {
        return crate::runtime().block_on(reject_call(
            globals,
            callback,
            ArgumentError::from(error),
        ));
    }
    run(globals, command, callback)
}

/// Checks a C API call's arguments as [`run_synchronously`] does, then starts its command on the
/// runtime without waiting for it.
#[lore_macro::test_pub]
pub(crate) fn run_asynchronously<ArgsType, Run, Fut>(
    globals: &LoreGlobalArgs,
    args: &ArgsType,
    callback: LoreEventCallbackConfig,
    run: Run,
) where
    ArgsType: ValidateText + Clone + Into<LoreCommand>,
    Run: FnOnce(LoreGlobalArgs, LoreCommand, LoreEventCallback) -> Fut,
    Fut: Future<Output = i32> + Send + 'static,
{
    let command = validate_call_text(globals, args).map(|()| args.clone().into());
    run_command_asynchronously(globals, command, callback, run);
}

fn run_command_asynchronously<Run, Fut>(
    globals: &LoreGlobalArgs,
    command: Result<LoreCommand, ArgumentError>,
    callback: LoreEventCallbackConfig,
    run: Run,
) where
    Run: FnOnce(LoreGlobalArgs, LoreCommand, LoreEventCallback) -> Fut,
    Fut: Future<Output = i32> + Send + 'static,
{
    if lore_base::runtime::runtime_shutdown_started() {
        reject_after_shutdown(callback);
        return;
    }
    crate::size_threads_for_relaying();
    let callback = lore_revision::event::convert_event_callback(callback);
    let command = match command {
        Ok(command) => command,
        Err(error) => {
            drop(lore_base::lore_spawn!(reject_call(
                globals.clone(),
                callback,
                error
            )));
            return;
        }
    };
    let mut globals = globals.clone();
    // Resolving the credentials reads their text, so it follows the check above.
    if let Err(error) = globals.validate() {
        drop(lore_base::lore_spawn!(reject_call(
            globals,
            callback,
            ArgumentError::from(error)
        )));
        return;
    }
    drop(lore_base::lore_spawn!(run(globals, command, callback)));
}

/// Report a malformed call the way a failing command reports: the status on the
/// return value and on a `Complete` event carrying the detail. No verb ran, so
/// no verb-specific terminal event fires.
pub(crate) async fn reject_call(
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

/// Runs `command` to completion, in the Lore service when one is in use and in this process
/// otherwise. Blocks on the runtime, so it is called from outside it.
///
/// Checks for the service before choosing what to run, so the calling thread's stack holds the
/// relay's future or this command's, never a future sized for every command.
pub fn run_command(
    globals: LoreGlobalArgs,
    command: LoreCommand,
    callback: LoreEventCallback,
) -> i32 {
    if service_in_use_blocking() {
        run_relayed(globals, command, callback)
    } else {
        command.run_local(globals, callback)
    }
}

/// Relays `command` to the Lore service and blocks until it finishes. Not inlined, so the relay's
/// future is in a frame of its own rather than under every command run in this process.
#[inline(never)]
fn run_relayed(globals: LoreGlobalArgs, command: LoreCommand, callback: LoreEventCallback) -> i32 {
    block_on_command(pin!(service_call(globals, command, callback)))
}

/// Runs `command` to completion in this process, for the commands that act on the Lore service
/// rather than through it. Blocks on the runtime, so it is called from outside it.
pub fn run_command_locally(
    globals: LoreGlobalArgs,
    command: LoreCommand,
    callback: LoreEventCallback,
) -> i32 {
    command.run_local(globals, callback)
}

/// Blocks on `running`, a command's future pinned in the caller's frame.
///
/// Taken as `dyn Future`, every command blocks through one instantiation of `block_on`, and the
/// runtime, which boxes a large future it is handed by value, has only a reference to hold.
pub(crate) fn block_on_command(running: Pin<&mut dyn Future<Output = i32>>) -> i32 {
    crate::runtime().block_on(running)
}

/// Runs `command` in the Lore service when one is in use and in this process otherwise.
///
/// Pins each future it awaits, for the reason `LoreCommand::invoke_local` gives.
pub(crate) async fn dispatch_command(
    globals: LoreGlobalArgs,
    command: LoreCommand,
    callback: LoreEventCallback,
) -> i32 {
    if service_in_use().await {
        let relay = pin!(service_call(globals, command, callback));
        relay.await
    } else {
        let handler = pin!(command.invoke_local(globals, callback));
        handler.await
    }
}

/// Runs `command` in this process, for the commands that act on the Lore service rather than
/// through it.
pub(crate) fn invoke_locally(
    globals: LoreGlobalArgs,
    command: LoreCommand,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> {
    command.invoke_local(globals, callback)
}

pub(crate) async fn dispatch_call<
    ArgsType: LoreArgs,
    Handler: Fn(LoreGlobalArgs, ArgsType, LoreEventCallback) -> Fut,
    Fut: Future<Output = i32> + Send + 'static,
>(
    globals: LoreGlobalArgs,
    args: ArgsType,
    callback: LoreEventCallback,
    handler: Handler,
) -> i32 {
    if service_in_use().await {
        service_call(globals, args.to_command(), callback).await
    } else {
        handler(globals, args, callback).await
    }
}
