// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::io::ErrorKind;
use std::io::Read;
use std::io::Write;

use bytes::Bytes;
use futures::future::Either;
use lore_base::env::CallEnvironment;
use lore_error_set::prelude::*;
use lore_revision::interface::LoreGlobalArgs;

use crate::interface::LoreEvent;
use crate::interface::LoreEventCallback;
use crate::remote::command::LoreCommand;

#[error_set]
pub enum MessageError {}

/// Reads one message, or `None` when the peer has closed the connection.
///
/// Returns the payload the message was decoded from beside it: a `LoreBytes` the message carries
/// views the payload, so the payload has to outlive every read of the view.
pub fn blocking_read_message<Message: bitcode::DecodeOwned, Reader: Read + Unpin>(
    reader: &mut Reader,
) -> Result<Option<(Message, Bytes)>, MessageError> {
    let mut version_byte: [u8; 1] = [0];
    let version = match reader.read_exact(&mut version_byte) {
        Ok(_) => version_byte[0],
        Err(error) => {
            return if error.kind() == ErrorKind::UnexpectedEof {
                Ok(None)
            } else {
                Err(MessageError::internal_with_context(
                    error,
                    "reading message version byte",
                ))
            };
        }
    };

    if version != MessageProtocol::V2 as u8 {
        return Err(MessageError::internal(
            "Message received with wrong version",
        ));
    }

    let mut header_bytes = [0; Header::SIZE];
    reader
        .read_exact(&mut header_bytes)
        .internal("reading message header")?;
    let header = Header::from_bytes(&header_bytes)?;

    let mut payload = vec![0; header.payload_size as usize];
    reader
        .read_exact(&mut payload)
        .internal("reading message payload")?;
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
    let payload_size = u32::try_from(payload.len()).internal("message too large")?;
    let mut header = [0; 1 + Header::SIZE];
    header[0] = MessageProtocol::V2 as u8;
    header[1..].copy_from_slice(&Header::new(payload_size, SerializationType::Bitcode).to_bytes());
    writer
        .write_all(&header)
        .and_then(|()| writer.write_all(payload))
        .internal("writing message")?;
    Ok(())
}

// Message wire format
// | MessageProtocol |            Header            |      MessageToServer or     |
// |                 |                              |       MessageToClient       |
// |------------------------------------------------------------------------------|
// |     1 byte      |   4 bytes    |    1 byte     |     payload_size bytes      |
// |        1        | payload_size | serialization |  bitcode bytes of message   |

#[repr(u8)]
#[derive(Clone, Copy, PartialEq)]
enum MessageProtocol {
    V2 = 1,
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq)]
pub enum SerializationType {
    Bitcode = 0,
}

impl TryFrom<u8> for SerializationType {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(SerializationType::Bitcode),
            _ => Err(()),
        }
    }
}

pub struct Header {
    pub payload_size: u32,
    pub serialization_type: SerializationType,
}

impl Header {
    const SIZE: usize = 5;

    pub fn new(payload_size: u32, serialization_type: SerializationType) -> Self {
        Self {
            payload_size,
            serialization_type,
        }
    }

    pub fn from_bytes(bytes: &[u8; Header::SIZE]) -> Result<Self, MessageError> {
        Ok(Self::new(
            bytes[0] as u32
                | (bytes[1] as u32) << 8
                | (bytes[2] as u32) << 16
                | (bytes[3] as u32) << 24,
            SerializationType::try_from(bytes[4]).map_err(|_err| {
                MessageError::internal(format!(
                    "Message received with invalid serialization type: {}",
                    bytes[4]
                ))
            })?,
        ))
    }

    pub fn to_bytes(&self) -> [u8; Header::SIZE] {
        [
            (self.payload_size & 0xff) as u8,
            (self.payload_size >> 8 & 0xff) as u8,
            (self.payload_size >> 16 & 0xff) as u8,
            (self.payload_size >> 24 & 0xff) as u8,
            self.serialization_type as u8,
        ]
    }
}

#[derive(Debug, Clone, bitcode::Encode, bitcode::Decode)]
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
#[derive(Clone, bitcode::Encode, bitcode::Decode)]
#[allow(clippy::large_enum_variant)]
pub enum MessageToClient<Event = LoreEvent> {
    Event(Event),
    ApiResult(i32),
}
