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
use crate::remote::call::ServiceCallError;
use crate::remote::call::run_service_call;
use crate::remote::command::LoreCommand;
use crate::remote::service_process::ServiceProcessError;
use crate::remote::service_process::ServiceStart;
use crate::remote::service_process::connect_or_spawn_service;
use crate::remote::service_process::connect_to_running_service;
use crate::remote::service_process::running_as_service;
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

/// Whether `globals` names a FUSE-backed instance that must reach the
/// service rather than run in this process.
///
/// A FUSE instance's working tree is served through a mount the service
/// maintains; running its command here instead would read and write the
/// repository outside that mount, alongside whatever the service is doing
/// through it. The check is unconditional -- unlike [`service_in_use`], which
/// is a process-wide setting -- because it follows from the instance's own
/// config, not from a choice this process makes for every call.
///
/// Always `false` inside the service process itself: it is the one process
/// every FUSE instance's calls are forced to, so forcing them here as well
/// would relay a call the service is already running back to itself.
#[lore_macro::test_pub]
fn instance_requires_service_given(globals: &LoreGlobalArgs, running_as_service: bool) -> bool {
    if running_as_service {
        return false;
    }
    instance_path(globals).is_some_and(|path| {
        lore_revision::repository::load_repository_config(&path)
            .is_ok_and(|config| config.vfs_type() == lore_revision::repository::VfsType::Fuse)
    })
}

/// The absolute path of the instance `globals` names, if they name one.
fn instance_path(globals: &LoreGlobalArgs) -> Option<std::path::PathBuf> {
    let repository_path = globals.repository_path();
    if repository_path.is_empty() {
        return None;
    }
    Some(
        lore_revision::util::path::make_absolute_from(
            repository_path,
            globals.working_directory().map(std::path::Path::new),
        )
        .unwrap_or_else(|_| std::path::PathBuf::from(repository_path)),
    )
}

/// Whether a forced call that reaches no service starts one: only on Linux, the one platform that
/// serves FUSE, and not for a call in an instance whose mount answers, which a service that is
/// still starting or listens on another socket serves. The mount is probed on a blocking thread,
/// as its server answers the probe.
#[cfg(target_os = "linux")]
async fn starts_a_service(globals: &LoreGlobalArgs, makes_instance: bool) -> bool {
    let Some(path) = instance_path(globals).filter(|_| !makes_instance) else {
        return true;
    };
    lore_base::lore_spawn_blocking!(move || {
        lore_revision::fs::fuse::discover::answering_lore_mount(&path).is_none()
    })
    .await
    .unwrap_or(true)
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::unused_async)]
async fn starts_a_service(_globals: &LoreGlobalArgs, _makes_instance: bool) -> bool {
    false
}

/// Whether `args` must run in the Lore service rather than here, decided without building a
/// command: either `args` require it themselves (see [`LoreArgs::requires_service`], overridden
/// by the clone and create arguments) or [`instance_requires_service_given`] holds.
///
/// [`command_requires_service_given`] answers the same question once a command already
/// exists, by reading this same predicate off the args a `LoreCommand` variant carries,
/// rather than re-deriving it -- the FUSE-vfs check lives once, on [`LoreArgs`].
fn args_require_service<ArgsType: LoreArgs>(
    globals: &LoreGlobalArgs,
    args: &ArgsType,
    running_as_service: bool,
) -> bool {
    if running_as_service {
        return false;
    }
    args.requires_service() || instance_requires_service_given(globals, false)
}

/// Whether `command` must run in the Lore service rather than here: either `command` is a
/// clone or a create whose `vfs` resolves to FUSE, which would otherwise be refused rather
/// than set up as a mount outside the service that must serve it, or
/// [`instance_requires_service_given`] holds.
#[lore_macro::test_pub]
fn command_requires_service(globals: &LoreGlobalArgs, command: &LoreCommand) -> bool {
    command_requires_service_given(globals, command, running_as_service())
}

/// The decision behind [`command_requires_service`], with "is this the
/// service process" taken as a parameter for the reason
/// [`instance_requires_service_given`] gives.
#[lore_macro::test_pub]
fn command_requires_service_given(
    globals: &LoreGlobalArgs,
    command: &LoreCommand,
    running_as_service: bool,
) -> bool {
    if running_as_service {
        return false;
    }
    // The commands whose arguments opt into `LoreArgs::requires_service`.
    let args_require = match command {
        LoreCommand::RepositoryClone(args) => args.requires_service(),
        LoreCommand::RepositoryCreate(args) => args.requires_service(),
        _ => false,
    };
    args_require || instance_requires_service_given(globals, false)
}

/// Runs a call on the service for an instance, clone, or create only the service may act
/// through, starting one when none is running and [`starts_a_service`] holds: from the executable
/// named for it, or else from this program's own where it runs the service
/// ([`ServiceStart::NamedOrThisProgram`]).
///
/// Never runs the call here. Where no service can be reached or started, it reports
/// [`lore_base::error::ServiceUnavailable`], saying why the call needs one and why none could be
/// had.
async fn forced_service_call(
    globals: LoreGlobalArgs,
    command: LoreCommand,
    callback: LoreEventCallback,
) -> i32 {
    let unreached = match connect_to_running_service().await {
        Ok(Some(connection)) => {
            return run_service_call(Some(connection), globals, command, callback).await;
        }
        Ok(None) => None,
        Err(error) => Some(error),
    };
    Box::pin(unanswered_forced_service_call(
        globals, command, callback, unreached,
    ))
    .await
}

/// [`forced_service_call`] where no service answered its connect, failing with `unreached` or,
/// where that is `None`, finding none running. Starts one where [`starts_a_service`] holds and
/// runs the call on it, or else reports why none could be had.
///
/// Boxed by its caller: it holds a second [`run_service_call`], and runs only where a service is
/// missing, so the future of every forced call stays the size of the call over a running service.
async fn unanswered_forced_service_call(
    globals: LoreGlobalArgs,
    command: LoreCommand,
    callback: LoreEventCallback,
    mut unreached: Option<ServiceProcessError>,
) -> i32 {
    let makes_instance = matches!(
        command,
        LoreCommand::RepositoryClone(_) | LoreCommand::RepositoryCreate(_)
    );
    if unreached.is_none() && starts_a_service(&globals, makes_instance).await {
        match connect_or_spawn_service(ServiceStart::NamedOrThisProgram).await {
            Ok(connection) => {
                return run_service_call(Some(connection), globals, command, callback).await;
            }
            Err(error) => unreached = Some(error),
        }
    }
    let needs = match &command {
        LoreCommand::RepositoryClone(_) => "A FUSE clone is mounted by the Lore service",
        LoreCommand::RepositoryCreate(_) => {
            "A FUSE instance is created and mounted by the Lore service"
        }
        _ => "This instance is served through a FUSE mount only the Lore service may use",
    };
    let reason = match unreached {
        None if makes_instance || !cfg!(target_os = "linux") => {
            format!("{needs}, and no service is running")
        }
        None => format!(
            "{needs}, and the service serving its mount doesn't listen on this command's socket, \
             {}: it is still starting, or it listens on another socket. Run the command again \
             once it has started, or with the `LORE_SERVICE_SOCKET` it listens on",
            crate::remote::service_socket_name()
        ),
        Some(ServiceProcessError::ServiceUnavailable(unavailable)) => format!(
            "{needs}, and no service could be reached or started: {}",
            unavailable.into_inner().reason
        ),
        Some(error @ ServiceProcessError::Internal(_)) => {
            format!("{needs}, and no service could be reached or started: {error}")
        }
    };
    crate::call::no_repository_call(
        globals,
        callback,
        (),
        "fuse_forced_relay",
        |()| async move {
            Err::<(), ServiceCallError>(ServiceCallError::from(
                lore_base::error::ServiceUnavailable { reason },
            ))
        },
    )
    .await
}

/// Runs `command` to completion, in the Lore service when the command requires one or one is in
/// use, and in this process otherwise. Blocks on the runtime, so it is called from outside it.
///
/// Checks for the service before choosing what to run, so the calling thread's stack holds the
/// relay's future or this command's, never a future sized for every command.
pub fn run_command(
    globals: LoreGlobalArgs,
    command: LoreCommand,
    callback: LoreEventCallback,
) -> i32 {
    if command_requires_service(&globals, &command) {
        run_forced(globals, command, callback)
    } else if service_in_use_blocking() {
        run_relayed(globals, command, callback)
    } else {
        command.run_local(globals, callback)
    }
}

/// Relays `command` to the Lore service and blocks until it finishes, for an instance, clone, or
/// create only the service may act through, as [`forced_service_call`] does. Not inlined, for the
/// reason [`run_relayed`] gives.
#[inline(never)]
fn run_forced(globals: LoreGlobalArgs, command: LoreCommand, callback: LoreEventCallback) -> i32 {
    block_on_command(pin!(forced_service_call(globals, command, callback)))
}

/// Relays `command` to the Lore service and blocks until it finishes. Not inlined, so the relay's
/// future is in a frame of its own rather than under every command run in this process.
#[inline(never)]
fn run_relayed(globals: LoreGlobalArgs, command: LoreCommand, callback: LoreEventCallback) -> i32 {
    block_on_command(pin!(run_service_call(None, globals, command, callback)))
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

/// Runs `command` in the Lore service when the command requires one or one is in use, and in this
/// process otherwise.
///
/// Pins each future it awaits, for the reason `LoreCommand::invoke_local` gives.
#[lore_macro::test_pub]
pub(crate) async fn dispatch_command(
    globals: LoreGlobalArgs,
    command: LoreCommand,
    callback: LoreEventCallback,
) -> i32 {
    if command_requires_service(&globals, &command) {
        let relay = pin!(forced_service_call(globals, command, callback));
        relay.await
    } else if service_in_use().await {
        let relay = pin!(run_service_call(None, globals, command, callback));
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

#[lore_macro::test_pub]
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
    if args_require_service(&globals, &args, running_as_service()) {
        return forced_service_call(globals, args.to_command(), callback).await;
    }
    if service_in_use().await {
        run_service_call(None, globals, args.to_command(), callback).await
    } else {
        handler(globals, args, callback).await
    }
}
