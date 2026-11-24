// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::io::Read;
use std::io::Write;
use std::ops::ControlFlow;

use bytes::Bytes;
use lore_base::env::CallEnvironment;
use lore_base::error::ServiceUnavailable;
use lore_base::log::LoreLogLevel;
use lore_error_set::prelude::*;
use lore_revision::event::LoreErrorDetail;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::relay::EventDispatcher;
use tokio::task::JoinHandle;

use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::remote::command::LoreCommand;
use crate::remote::message::Answer;
use crate::remote::message::MessageError;
use crate::remote::message::MessageToClient;
use crate::remote::message::MessageToServer;
use crate::remote::message::Schemas;
use crate::remote::message::UNANSWERED;
use crate::remote::message::blocking_read_answer;
use crate::remote::message::blocking_read_message;
use crate::remote::message::blocking_read_stop_result;
use crate::remote::message::schemas_differ;
use crate::remote::message::write_hello;
use crate::remote::message::write_message;
use crate::remote::message::write_stop;
use crate::remote::network::UdsStream;
use crate::remote::network::uds_supported;
use crate::remote::service_process::ServiceStart;
use crate::remote::service_process::connect_or_spawn_service;

#[error_set]
pub enum ServiceCallError {
    ServiceUnavailable,
}

/// Records the directory the service resolves this call's relative paths
/// against, when the caller left it unset. The service runs in a directory
/// unrelated to the caller's, so without this a relative path would resolve
/// there rather than where the caller ran. A caller that set the field, such as
/// an installed tool that runs from a fixed directory but wants relative paths
/// resolved elsewhere, keeps its value.
#[allow(clippy::disallowed_methods)]
fn fill_working_directory(globals: &mut LoreGlobalArgs) {
    if globals.working_directory().is_some() {
        return;
    }
    if let Ok(directory) = std::env::current_dir() {
        globals.working_directory = directory.display().to_string().into();
    }
}

/// What came of a call run on the service.
#[lore_macro::test_pub]
pub(crate) enum Relayed {
    /// The service ran the call, which ended with this status.
    Ran(i32),
    /// A move of the external root holds the service socket, so the call didn't run, and is handed
    /// back to run in this process.
    HandedBack(Box<HandedBack>),
}

/// A call the service socket answered with the move reply, with its dispatcher's callback. Boxed
/// in [`Relayed`]: only this cold branch holds the call's values.
#[lore_macro::test_pub]
pub(crate) struct HandedBack {
    pub(crate) globals: LoreGlobalArgs,
    pub(crate) command: LoreCommand,
    pub(crate) callback: LoreEventCallback,
}

/// Why a call can't run while a move of the external root holds the service socket.
#[lore_macro::test_pub]
pub(crate) const EXTERNAL_ROOT_MOVING: &str = "the Lore service is stopped while the external \
    directories of its virtual instances move. Run the command again once the move has finished";

/// Runs the call on the service: over `connection` when the caller holds one, or else over a
/// connection to the running service, starting one from the named executable when none is running.
/// Where a move of the external root holds the socket, the call is handed back.
///
/// Not an `async fn`, which would hold a second copy of its arguments. The dispatcher is built
/// before the future, which then holds it and not the callback it is built from. Building the
/// dispatcher starts its forwarder, so a caller awaits the future where it builds it. The future
/// moves `globals` and `command` from its captures into the write of the request: passed to a
/// helper's future, they would be held a second time.
#[inline]
#[lore_macro::test_pub]
pub(crate) fn run_service_call(
    connection: Option<UdsStream>,
    mut globals: LoreGlobalArgs,
    command: LoreCommand,
    callback: LoreEventCallback,
) -> impl Future<Output = Relayed> {
    fill_working_directory(&mut globals);
    let event_dispatcher = EventDispatcher::new(callback);

    async move {
        let result = match reach_service(connection).await {
            Ok(connection) => {
                let sent = lore_base::lore_spawn_blocking!(move || {
                    request(
                        connection,
                        MessageToServer {
                            globals,
                            command,
                            environment: CallEnvironment::capture(),
                        },
                    )
                });
                relay_events(&event_dispatcher, sent).await
            }
            Err(err) => Err(err),
        };
        let relayed = match result {
            Ok(relayed) => relayed,
            Err(err) => return Relayed::Ran(fail_service_call(&event_dispatcher, err).await),
        };
        match relayed {
            Relayed::Ran(status) => {
                // The read loop returns on the result, leaving events queued for this
                // process's forwarder. Drained so a caller reading what its callback
                // collected sees all of it; a local call's `complete` does the same.
                event_dispatcher.drain().await;
                Relayed::Ran(status)
            }
            Relayed::HandedBack(mut handed_back) => {
                handed_back.callback = event_dispatcher.hand_back().await;
                Relayed::HandedBack(handed_back)
            }
        }
    }
}

/// Completes a call that no service completed with the error that kept it from running.
async fn fail_service_call(event_dispatcher: &EventDispatcher, err: ServiceCallError) -> i32 {
    // No service completed the command, so it completes here as a failing
    // command does, with the failure's own code: a caller has to be able
    // to tell a call that never reached a service from one the service
    // ran and reported on.
    event_dispatcher.send(LoreEvent::Log(EventDispatcher::make_log(
        LoreLogLevel::Error,
        format!("Failed to send command to Lore service because: {err}"),
    )));
    event_dispatcher
        .complete(LoreErrorDetail::from_error(&err))
        .await
}

/// What the service socket answered first to a request. Unboxed, as every call's first message
/// passes through it.
#[allow(clippy::large_enum_variant)]
enum Response {
    /// The connection the service runs the call over, with its first message, or `None` where it
    /// closed the connection without one.
    Serving(UdsStream, Option<(MessageToClient, Bytes)>),
    /// A move of the external root holds the socket: the request's call, handed back.
    Moving(Box<HandedBack>),
}

/// Writes the hello and `message` over `connection`, checks the answer to the hello, and reads the
/// first message. Blocking.
fn request(
    mut connection: UdsStream,
    message: MessageToServer,
) -> Result<Response, ServiceCallError> {
    let sent = write_hello(connection.writer())
        .and_then(|()| write_message(connection.writer(), &message));
    // The answer is read before a failed write is reported: a service that refuses the hello
    // closes the connection without reading the call, which fails the write of a call larger than
    // the socket buffers.
    blocking_check_answer(connection.reader(), sent)?;
    let first = blocking_read_message(connection.reader())
        .forward::<ServiceCallError>("receiving message")?;
    if let Some((MessageToClient::ExternalRootMove, _)) = first {
        return Ok(Response::Moving(Box::new(HandedBack {
            globals: message.globals,
            command: message.command,
            callback: None,
        })));
    }
    Ok(Response::Serving(connection, first))
}

/// Reads the service's answer to the hello a call opened with, then reports `sent`, the write of
/// the hello and the call. Fails with `ServiceUnavailable` where the service refused the call, its
/// schemas differ from this build's, or it closed the connection without answering, giving a
/// failed write in the reason for the last.
#[lore_macro::test_pub]
fn blocking_check_answer<Reader: Read>(
    reader: &mut Reader,
    sent: Result<(), MessageError>,
) -> Result<(), ServiceCallError> {
    let reason = match blocking_read_answer(reader)
        .forward::<ServiceCallError>("receiving the service's hello")?
    {
        Answer::Hello(schemas) if schemas == Schemas::THIS_BUILD => {
            return sent.forward::<ServiceCallError>("sending message");
        }
        Answer::Hello(schemas) => schemas_differ(schemas, Schemas::THIS_BUILD),
        // The refusal says why a write it cut short failed.
        Answer::Refusal(reason) => reason,
        Answer::OtherProtocol(protocol) => format!(
            "the Lore service answered in protocol version {protocol}, where this client reads \
             version 3; stop it with the `lore` it came from to use a service of the client's \
             build"
        ),
        Answer::Closed => match sent {
            Ok(()) => UNANSWERED.to_string(),
            // Kept, as it tells a service that exited from one that closed on the hello.
            Err(err) => {
                let reason = format!("{UNANSWERED}; sending the call failed: {err}");
                return Err(ServiceCallError::ServiceUnavailable(
                    ServiceUnavailable { reason }.chain_err_from(err, "sending message"),
                ));
            }
        },
    };
    Err(ServiceUnavailable { reason }.into())
}

/// Writes a request that the service stop and reads the status and message it answers with,
/// failing with `ServiceUnavailable` where the service closes the connection without answering.
#[lore_macro::test_pub]
fn blocking_request_stop<Writer: Write, Reader: Read>(
    writer: &mut Writer,
    reader: &mut Reader,
    force: bool,
) -> Result<(i32, String), ServiceCallError> {
    write_stop(writer, force)
        .and_then(|()| blocking_read_stop_result(reader))
        .forward::<ServiceCallError>("asking the service to stop")?
        .ok_or_else(|| {
            ServiceUnavailable {
                reason: UNANSWERED.to_string(),
            }
            .into()
        })
}

/// Asks the service on `connection` to stop, with a request that a service of this protocol
/// honours whatever its schemas, so that a service of another build can be stopped. Completes with
/// the status and message the service answers with, or with the failure of
/// [`blocking_request_stop`].
///
/// The stop's events aren't relayed, since they would arrive as messages of the service's build,
/// which this build may not read. The service keeps the stop's log in its own.
pub(crate) async fn run_service_stop(
    mut connection: UdsStream,
    force: bool,
    callback: LoreEventCallback,
) -> i32 {
    let event_dispatcher = EventDispatcher::new(callback);
    let answered = lore_base::lore_spawn_blocking!(move || {
        let mut reader = connection.try_clone().internal("cloning connection")?;
        blocking_request_stop(connection.writer(), reader.reader(), force)
    })
    .await
    .unwrap_or_else(|err| {
        Err(ServiceCallError::internal_with_context(
            err,
            "joining the stop task",
        ))
    });
    match answered {
        Ok((status, message)) => {
            // Logged here, as the service's own log of the failure stays in the service: a caller
            // such as the CLI says nothing of a `Complete`, so this is all it has to report.
            if status != 0 {
                event_dispatcher.send(LoreEvent::Log(EventDispatcher::make_log(
                    LoreLogLevel::Error,
                    message.clone(),
                )));
            }
            event_dispatcher
                .complete(LoreErrorDetail {
                    error_code: status,
                    message: message.into(),
                    trace_locations: LoreArray::default(),
                })
                .await
        }
        Err(err) => fail_service_call(&event_dispatcher, err).await,
    }
}

/// The connection the call runs over: `connection` where the caller holds one, or else one to the
/// running service, started from the named executable when none is running. Fails with
/// `ServiceUnavailable` where the OS supports no IPC, whether or not the caller holds a connection.
async fn reach_service(connection: Option<UdsStream>) -> Result<UdsStream, ServiceCallError> {
    if !uds_supported() {
        // No service can be reached here at all, which is the same answer for a
        // caller as one that could not be started.
        return Err(ServiceUnavailable {
            reason: "OS doesn't support IPC".to_string(),
        }
        .into());
    }

    match connection {
        Some(connection) => Ok(connection),
        None => connect_or_spawn_service(ServiceStart::Named)
            .await
            .forward::<ServiceCallError>("reaching a Lore service"),
    }
}

/// Hands each event the service sends to `event_dispatcher`, once `sent` has written the request
/// and read the first answer, and returns the call's result, or the call handed back.
async fn relay_events(
    event_dispatcher: &EventDispatcher,
    sent: JoinHandle<Result<Response, ServiceCallError>>,
) -> Result<Relayed, ServiceCallError> {
    let connection = match handle_answer(
        event_dispatcher,
        sent.await.internal("joining connection task")??,
    )? {
        ControlFlow::Continue(connection) => connection,
        ControlFlow::Break(relayed) => return Ok(relayed),
    };

    loop {
        let mut connection = connection.try_clone().internal("cloning connection")?;
        let message: Option<(MessageToClient, Bytes)> =
            lore_base::lore_spawn_blocking!(move || blocking_read_message(connection.reader()))
                .await
                .internal("joining receive task")?
                .forward::<ServiceCallError>("receiving message")?;
        let Some((message, payload)) = message else {
            return Err(closed_without_result());
        };
        if let Some(api_result) = handle_message(event_dispatcher, message, payload)? {
            return Ok(Relayed::Ran(api_result));
        }
    }
}

/// Hands the first message of `answer` to `event_dispatcher`, and answers the connection to read
/// the rest from, or what came of the call where nothing more is to be read.
fn handle_answer(
    event_dispatcher: &EventDispatcher,
    answer: Response,
) -> Result<ControlFlow<Relayed, UdsStream>, ServiceCallError> {
    let (connection, first) = match answer {
        Response::Serving(connection, first) => (connection, first),
        Response::Moving(handed_back) => {
            return Ok(ControlFlow::Break(Relayed::HandedBack(handed_back)));
        }
    };
    let (message, payload) = first.ok_or_else(closed_without_result)?;
    Ok(match handle_message(event_dispatcher, message, payload)? {
        Some(api_result) => ControlFlow::Break(Relayed::Ran(api_result)),
        None => ControlFlow::Continue(connection),
    })
}

/// The failure of a call whose service closed the connection before it sent the call's result.
fn closed_without_result() -> ServiceCallError {
    ServiceCallError::internal("Lore service closed connection without sending a result")
}

/// Hands an event to the callback, keeping `payload`, which holds any `LoreBytes` it carries,
/// alive until the callback has run. A move reply after the first message fails the call.
pub fn handle_message(
    event_dispatcher: &EventDispatcher,
    message: MessageToClient,
    payload: Bytes,
) -> Result<Option<i32>, ServiceCallError> {
    match message {
        MessageToClient::Event(event) => {
            event_dispatcher.send_with_bytes(event, payload);
            Ok(None)
        }
        MessageToClient::ApiResult(api_result) => Ok(Some(api_result)),
        MessageToClient::ExternalRootMove => Err(ServiceCallError::internal(
            "Lore service answered a move of the external root after the call began",
        )),
    }
}
