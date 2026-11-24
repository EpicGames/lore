// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::io::ErrorKind;
use std::io::Read;
use std::io::Write;

use bytes::Bytes;
use futures::future::Either;
use lore_base::env::CallEnvironment;
use lore_base::wire_schema::WireSchema;
use lore_error_set::prelude::*;
use lore_revision::interface::LoreGlobalArgs;

use crate::interface::LoreEvent;
use crate::interface::LoreEventCallback;
use crate::remote::command::LoreCommand;

#[error_set]
pub enum MessageError {}

/// The schemas of the messages a build writes and reads, which a client and the service exchange
/// before a call, as ADR-00025 decides. Each refuses a peer whose schemas differ from its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[lore_macro::test_pub]
pub(crate) struct Schemas {
    /// The schema of the `MessageToServer` the build writes as a client and reads as the service.
    pub to_server: u64,
    /// The schema of the `MessageToClient` the build writes as the service and reads as a client.
    pub to_client: u64,
}

impl Schemas {
    /// The schemas of this build.
    pub const THIS_BUILD: Schemas = Schemas {
        to_server: <MessageToServer as WireSchema>::SCHEMA,
        to_client: <MessageToClient as WireSchema>::SCHEMA,
    };

    /// The bytes of the payload of a hello.
    pub(crate) const SIZE: usize = 2 * size_of::<u64>();

    fn to_bytes(self) -> [u8; Self::SIZE] {
        let mut bytes = [0; Self::SIZE];
        bytes[..8].copy_from_slice(&self.to_server.to_le_bytes());
        bytes[8..].copy_from_slice(&self.to_client.to_le_bytes());
        bytes
    }

    /// The schemas the payload of a hello holds.
    pub(crate) fn from_payload(payload: &[u8]) -> Result<Self, MessageError> {
        match payload.as_chunks::<8>() {
            ([to_server, to_client], []) => Ok(Self {
                to_server: u64::from_le_bytes(*to_server),
                to_client: u64::from_le_bytes(*to_client),
            }),
            _ => Err(wrong_size(FrameKind::Hello, payload.len(), "16")),
        }
    }
}

impl std::fmt::Display for Schemas {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}/{:016x}", self.to_server, self.to_client)
    }
}

/// Why a client and a service whose schemas differ refuse each other.
#[cold]
#[lore_macro::test_pub]
pub(crate) fn schemas_differ(service: Schemas, client: Schemas) -> String {
    format!(
        "the Lore service is of a build whose messages differ from the client's (service schemas \
         {service}, client schemas {client}); stop it with `lore service stop` to use a service \
         of the client's build"
    )
}

/// What a client opens a connection with.
#[derive(Debug, PartialEq, Eq)]
#[lore_macro::test_pub]
pub(crate) enum Opening {
    /// The schemas of the client's build, followed by a call.
    Hello(Schemas),
    /// A request to stop the service, which a service of this protocol honours whatever the
    /// client's schemas. Carries the `force` the stop runs with.
    Stop { force: bool },
    /// A first byte that names another protocol, as a client of an older build writes.
    OtherProtocol(u8),
}

/// What the service answers an [`Opening::Hello`] with.
#[derive(Debug, PartialEq, Eq)]
#[lore_macro::test_pub]
pub(crate) enum Answer {
    /// The schemas of the service's build.
    Hello(Schemas),
    /// Why the service refuses the call.
    Refusal(String),
    /// No answer: the service closed the connection, for the reason [`UNANSWERED`] gives, or
    /// having exited.
    Closed,
    /// A first byte that names another protocol, as a service of another build answers a client
    /// it can't serve with, whose frame this build can't read past it.
    OtherProtocol(u8),
}

/// Reads what a client opens a connection with, or `None` when the client closes it first.
#[lore_macro::test_pub]
pub(crate) fn blocking_read_opening<Reader: Read>(
    reader: &mut Reader,
) -> Result<Option<Opening>, MessageError> {
    let Some(protocol) = read_protocol(reader)? else {
        return Ok(None);
    };
    if protocol != MessageProtocol::V3 as u8 {
        return Ok(Some(Opening::OtherProtocol(protocol)));
    }
    match read_frame(reader, false)? {
        (FrameKind::Hello, payload) => Ok(Some(Opening::Hello(Schemas::from_payload(&payload)?))),
        (FrameKind::Stop, payload) => match payload.as_slice() {
            [force] => Ok(Some(Opening::Stop { force: *force != 0 })),
            _ => Err(wrong_size(FrameKind::Stop, payload.len(), "1")),
        },
        (kind, _) => Err(unexpected(kind, "a hello or a stop")),
    }
}

/// Reads the service's answer to a hello, or [`Answer::Closed`] where the service closes the
/// connection without one, or [`Answer::OtherProtocol`] where it answers in another protocol.
#[lore_macro::test_pub]
pub(crate) fn blocking_read_answer<Reader: Read>(
    reader: &mut Reader,
) -> Result<Answer, MessageError> {
    let Some(protocol) = read_protocol(reader)? else {
        return Ok(Answer::Closed);
    };
    if protocol != MessageProtocol::V3 as u8 {
        return Ok(Answer::OtherProtocol(protocol));
    }
    match read_frame(reader, false)? {
        (FrameKind::Hello, payload) => Ok(Answer::Hello(Schemas::from_payload(&payload)?)),
        (FrameKind::Refusal, payload) => Ok(Answer::Refusal(
            String::from_utf8_lossy(&payload).into_owned(),
        )),
        (kind, _) => Err(unexpected(kind, "a hello or a refusal")),
    }
}

/// Why a call or a stop failed whose service closed the connection without answering it.
#[lore_macro::test_pub]
pub(crate) const UNANSWERED: &str = "the Lore service closed the connection without \
                                     answering, as a service of an older protocol, or one \
                                     that is stopping, does";

/// Reads the status the service answers a stop with, and the message of its failure, or `None`
/// where the service closed the connection without answering, for the reason [`UNANSWERED`]
/// gives.
#[lore_macro::test_pub]
pub(crate) fn blocking_read_stop_result<Reader: Read>(
    reader: &mut Reader,
) -> Result<Option<(i32, String)>, MessageError> {
    let Some(protocol) = read_protocol(reader)? else {
        return Ok(None);
    };
    check_protocol(protocol)?;
    match read_frame(reader, false)? {
        (FrameKind::StopResult, payload) => match payload.split_first_chunk::<4>() {
            Some((status, message)) => Ok(Some((
                i32::from_le_bytes(*status),
                String::from_utf8_lossy(message).into_owned(),
            ))),
            None => Err(wrong_size(
                FrameKind::StopResult,
                payload.len(),
                "at least 4",
            )),
        },
        (kind, _) => Err(unexpected(kind, "a stop result")),
    }
}

/// Writes the schemas of this build, which open a call or answer one.
#[lore_macro::test_pub]
pub(crate) fn write_hello<Writer: Write>(writer: &mut Writer) -> Result<(), MessageError> {
    write_frame(writer, FrameKind::Hello, &Schemas::THIS_BUILD.to_bytes())
}

/// Writes why the service refuses a call, cut as [`control_text`] cuts it.
#[lore_macro::test_pub]
pub(crate) fn write_refusal<Writer: Write>(
    writer: &mut Writer,
    reason: &str,
) -> Result<(), MessageError> {
    write_frame(writer, FrameKind::Refusal, control_text(reason))
}

/// Writes a request that the service stop.
#[lore_macro::test_pub]
pub(crate) fn write_stop<Writer: Write>(
    writer: &mut Writer,
    force: bool,
) -> Result<(), MessageError> {
    write_frame(writer, FrameKind::Stop, &[u8::from(force)])
}

/// Writes the status of a stop and the message of its failure, empty where it succeeded, cut as
/// [`control_text`] cuts it.
#[lore_macro::test_pub]
pub(crate) fn write_stop_result<Writer: Write>(
    writer: &mut Writer,
    status: i32,
    message: &str,
) -> Result<(), MessageError> {
    let mut payload = status.to_le_bytes().to_vec();
    payload.extend_from_slice(control_text(message));
    write_frame(writer, FrameKind::StopResult, &payload)
}

/// Reads one message, or `None` when the peer has closed the connection.
///
/// Returns the payload the message was decoded from beside it: a `LoreBytes` the message carries
/// views the payload, so the payload has to outlive every read of the view.
pub fn blocking_read_message<Message: bitcode::DecodeOwned, Reader: Read + Unpin>(
    reader: &mut Reader,
) -> Result<Option<(Message, Bytes)>, MessageError> {
    let Some(protocol) = read_protocol(reader)? else {
        return Ok(None);
    };
    check_protocol(protocol)?;
    let (kind, payload) = read_frame(reader, true)?;
    if kind != FrameKind::Message {
        return Err(unexpected(kind, "a message"));
    }
    let payload = Bytes::from(payload);

    let message = bitcode::decode(&payload).internal("decoding message")?;
    Ok(Some((message, payload)))
}

/// Writes one message.
pub fn write_message<Message: bitcode::Encode, Writer: Write>(
    writer: &mut Writer,
    message: &Message,
) -> Result<(), MessageError> {
    write_payload(writer, &encode_message(message))
}

/// The payload of one message, for a sender that encodes it before it writes it.
pub fn encode_message<Message: bitcode::Encode>(message: &Message) -> Vec<u8> {
    bitcode::encode(message)
}

/// Writes one message, given the payload [`encode_message`] made of it.
pub fn write_payload<Writer: Write>(
    writer: &mut Writer,
    payload: &[u8],
) -> Result<(), MessageError> {
    write_frame(writer, FrameKind::Message, payload)
}

fn write_frame<Writer: Write>(
    writer: &mut Writer,
    kind: FrameKind,
    payload: &[u8],
) -> Result<(), MessageError> {
    let payload_size = u32::try_from(payload.len()).internal("message too large")?;
    let mut header = [0; FRAME_HEAD_SIZE];
    header[0] = MessageProtocol::V3 as u8;
    header[1..].copy_from_slice(&Header::new(payload_size, kind).to_bytes());
    writer
        .write_all(&header)
        .and_then(|()| writer.write_all(payload))
        .internal("writing message")?;
    Ok(())
}

/// Reads the byte a frame starts with, which names its protocol, or `None` when the peer has
/// closed the connection. A connection reset before the byte counts as closed: the OS may report
/// a reset where the peer closed the connection with data of this side unread, as a peer that
/// can't read this side's protocol does.
fn read_protocol<Reader: Read>(reader: &mut Reader) -> Result<Option<u8>, MessageError> {
    let mut protocol = [0; 1];
    match reader.read_exact(&mut protocol) {
        Ok(()) => Ok(Some(protocol[0])),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::UnexpectedEof
                    | ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(MessageError::internal_with_context(
            error,
            "reading the protocol byte",
        )),
    }
}

fn check_protocol(protocol: u8) -> Result<(), MessageError> {
    if protocol == MessageProtocol::V3 as u8 {
        return Ok(());
    }
    Err(MessageError::internal(format!(
        "Frame received whose first byte is {protocol}, where this build reads protocol version {}",
        MessageProtocol::V3 as u8
    )))
}

/// The bytes of a frame before its payload: its protocol byte and its header.
pub(crate) const FRAME_HEAD_SIZE: usize = 1 + Header::SIZE;

/// The header in `head`, the bytes a frame starts with, for a reader that reads the payload
/// itself. Fails where the frame is of another protocol or of a kind this build doesn't know.
pub(crate) fn frame_header(head: &[u8; FRAME_HEAD_SIZE]) -> Result<Header, MessageError> {
    let [protocol, header @ ..] = head;
    check_protocol(*protocol)?;
    Header::from_bytes(header)
}

/// Reads the rest of a frame after its protocol byte. The size of any frame but the message a
/// reader of messages expects is checked against [`CONTROL_FRAME_LIMIT`] before its payload is
/// allocated, so that a reader of control frames refuses a larger message from its header.
fn read_frame<Reader: Read>(
    reader: &mut Reader,
    reads_messages: bool,
) -> Result<(FrameKind, Vec<u8>), MessageError> {
    let mut header_bytes = [0; Header::SIZE];
    reader
        .read_exact(&mut header_bytes)
        .internal("reading frame header")?;
    let header = Header::from_bytes(&header_bytes)?;
    let unbounded = reads_messages && header.kind == FrameKind::Message;
    if !unbounded && header.payload_size > CONTROL_FRAME_LIMIT {
        return Err(MessageError::internal(format!(
            "Frame of kind {:?} received with {} bytes, more than the {CONTROL_FRAME_LIMIT} a \
             control frame may hold",
            header.kind, header.payload_size
        )));
    }

    let mut payload = vec![0; header.payload_size as usize];
    reader
        .read_exact(&mut payload)
        .internal("reading frame payload")?;
    Ok((header.kind, payload))
}

/// The most bytes the payload of a control frame holds.
#[lore_macro::test_pub]
const CONTROL_FRAME_LIMIT: u32 = 4096;

/// The bytes of `text` that fit a control frame beside a stop status, cut at a character boundary.
fn control_text(text: &str) -> &[u8] {
    let mut end = text
        .len()
        .min(CONTROL_FRAME_LIMIT as usize - size_of::<i32>());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text.as_bytes()[..end]
}

#[cold]
fn wrong_size(kind: FrameKind, size: usize, expected: &str) -> MessageError {
    MessageError::internal(format!(
        "Frame of kind {kind:?} received with {size} bytes, where {expected} were expected"
    ))
}

#[cold]
fn unexpected(kind: FrameKind, expected: &str) -> MessageError {
    MessageError::internal(format!(
        "Frame received of kind {kind:?}, where {expected} was expected"
    ))
}

// Message wire format, protocol version 3
// | MessageProtocol |            Header            |           Payload           |
// |------------------------------------------------------------------------------|
// |     1 byte      |   4 bytes    |    1 byte     |     payload_size bytes      |
// |        3        | payload_size |   FrameKind   | as its FrameKind describes  |
//
// A call: the client writes a Hello and then a Message holding a MessageToServer. The service
// reads the Hello first, and answers it with a Refusal where the schemas differ from its own, or
// else with its Hello, and then a Message holding a MessageToClient for each event and one for
// the status. A stop: the client writes a Stop, and the service answers with a StopResult.

#[repr(u8)]
#[derive(Clone, Copy, PartialEq)]
enum MessageProtocol {
    V3 = 3,
}

/// What a frame's payload holds.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    /// The bitcode of a `MessageToServer` or a `MessageToClient`.
    Message = 0,
    /// The [`Schemas`] of the sender's build, each a little-endian `u64`, `to_server` first.
    Hello = 1,
    /// The UTF-8 reason the service refuses a call.
    Refusal = 2,
    /// One byte, nonzero where the stop is forced.
    Stop = 3,
    /// The little-endian `i32` status of a stop, then the UTF-8 message of its failure.
    StopResult = 4,
}

impl TryFrom<u8> for FrameKind {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(FrameKind::Message),
            1 => Ok(FrameKind::Hello),
            2 => Ok(FrameKind::Refusal),
            3 => Ok(FrameKind::Stop),
            4 => Ok(FrameKind::StopResult),
            _ => Err(()),
        }
    }
}

pub struct Header {
    pub payload_size: u32,
    pub kind: FrameKind,
}

impl Header {
    const SIZE: usize = 5;

    pub fn new(payload_size: u32, kind: FrameKind) -> Self {
        Self { payload_size, kind }
    }

    pub fn from_bytes(bytes: &[u8; Header::SIZE]) -> Result<Self, MessageError> {
        Ok(Self::new(
            bytes[0] as u32
                | (bytes[1] as u32) << 8
                | (bytes[2] as u32) << 16
                | (bytes[3] as u32) << 24,
            FrameKind::try_from(bytes[4]).map_err(|_err| {
                MessageError::internal(format!("Frame received with an unknown kind: {}", bytes[4]))
            })?,
        ))
    }

    pub fn to_bytes(&self) -> [u8; Header::SIZE] {
        [
            (self.payload_size & 0xff) as u8,
            (self.payload_size >> 8 & 0xff) as u8,
            (self.payload_size >> 16 & 0xff) as u8,
            (self.payload_size >> 24 & 0xff) as u8,
            self.kind as u8,
        ]
    }
}

#[derive(Debug, Clone, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
pub struct MessageToServer {
    pub globals: LoreGlobalArgs,
    pub command: LoreCommand,
    pub environment: CallEnvironment,
}

impl MessageToServer {
    /// Starts the command's handler in this process, under the caller's environment. The caller
    /// pins the future before awaiting it, for the reason `LoreCommand::invoke_local` gives.
    ///
    /// Checks the text the call carries first, as the entry point did in the caller's process: it
    /// arrives as the bytes the caller sent, and a handler reads it as `&str`. A call that fails
    /// the check is refused as the entry point refuses it.
    ///
    /// Then resolves a relative repository path against the caller's working directory, since
    /// this process runs in a directory unrelated to the caller's. An empty path names no
    /// repository and is left empty.
    pub fn invoke(mut self, callback: LoreEventCallback) -> impl Future<Output = i32> {
        let run = match crate::call_delegation::validate_call_text(&self.globals, &self.command) {
            Ok(()) => {
                if !self.globals.repository_path.is_empty() {
                    crate::call::resolve_repository_path(&mut self.globals);
                }
                Either::Left(self.command.invoke_local(self.globals, callback))
            }
            Err(error) => Either::Right(crate::call_delegation::reject_call(
                self.globals,
                callback,
                error,
            )),
        };
        crate::call::with_relayed_environment(self.environment, run)
    }
}

/// What the service sends a client: the command's events, then its status.
///
/// Generic over the event so that the service can encode an event it is handed by reference,
/// which encodes as the event itself, without copying it.
#[derive(Clone, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[allow(clippy::large_enum_variant)]
pub enum MessageToClient<Event = LoreEvent> {
    Event(Event),
    ApiResult(i32),
    /// The one answer of a service socket that a move of the external root holds: no service
    /// runs, and the call didn't run.
    ExternalRootMove,
}
