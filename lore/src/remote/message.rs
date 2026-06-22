// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::io::ErrorKind;
use std::io::Read;

use lore_error_set::prelude::*;
use lore_revision::interface::LoreGlobalArgs;
use serde::Deserialize;
use serde::Serialize;

use crate::interface::LoreEvent;
use crate::interface::LoreEventCallback;
use crate::remote::command::LoreCommand;

#[error_set]
pub enum SerializeError {}

#[error_set]
pub enum DeserializeError {}

#[error_set]
pub enum MessageError {}

pub fn blocking_read_v1_message<Message: for<'a> Deserialize<'a>, Reader: Read + Unpin>(
    reader: &mut Reader,
) -> Result<Option<(V1Header, Message)>, MessageError> {
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

    if version != MessageProtocol::V1 as u8 {
        return Err(MessageError::internal(
            "Message received with wrong version",
        ));
    }

    let mut header_bytes = [0; V1Header::SIZE];
    reader
        .read_exact(&mut header_bytes)
        .internal("reading message header")?;
    let header = V1Header::from_bytes(&header_bytes).unwrap();

    let mut message_bytes = vec![0; header.payload_size as usize];
    reader
        .read_exact(&mut message_bytes)
        .internal("reading message payload")?;

    let message = deserialize_message(message_bytes.as_slice(), header.serialization_type)
        .forward::<MessageError>("deserializing message")?;
    Ok(Some((header, message)))
}

pub fn write_v1_message<Message: Serialize>(
    message: Message,
    serialization_type: SerializationType,
) -> Result<Vec<u8>, MessageError> {
    let message_bytes = serialize_message(message, serialization_type)
        .forward::<MessageError>("serializing message")?;
    let header = V1Header::new(message_bytes.len() as u32, serialization_type);

    let mut result_bytes = Vec::new();
    result_bytes.push(MessageProtocol::V1 as u8);
    result_bytes.extend_from_slice(&header.to_bytes());
    result_bytes.extend_from_slice(&message_bytes);

    Ok(result_bytes)
}

pub fn serialize_message<Message: Serialize>(
    message: Message,
    serialization_type: SerializationType,
) -> Result<Vec<u8>, SerializeError> {
    Ok(match serialization_type {
        SerializationType::Bincode => bitcode::serialize(&message).internal("bitcode serialize")?,
        SerializationType::Json => serde_json::to_vec(&message).internal("json serialize")?,
    })
}

pub fn deserialize_message<Message: for<'a> Deserialize<'a>>(
    message_bytes: &[u8],
    serialization_type: SerializationType,
) -> Result<Message, DeserializeError> {
    Ok(match serialization_type {
        SerializationType::Bincode => {
            bitcode::deserialize(message_bytes).internal("bitcode deserialize")?
        }
        SerializationType::Json => {
            serde_json::from_slice(message_bytes).internal("json deserialize")?
        }
    })
}

// Message wire format
// | MessageProtocol |           V1Header           |      MessageToServer or     |
// |                 |                              |       MessageToClient       |
// |------------------------------------------------------------------------------|
// |     1 byte      |   4 bytes    |    1 byte     |     payload_size bytes      |
// |        0        | payload_size | serialization | serialized bytes of message |

#[repr(u8)]
#[derive(Clone, Copy, PartialEq)]
enum MessageProtocol {
    V1 = 0,
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq)]
pub enum SerializationType {
    Bincode = 0,
    Json = 1,
}

impl TryFrom<u8> for SerializationType {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Bincode),
            1 => Ok(Self::Json),
            _ => Err(()),
        }
    }
}

pub struct V1Header {
    pub payload_size: u32,
    pub serialization_type: SerializationType,
}

impl V1Header {
    const SIZE: usize = 5;

    pub fn new(payload_size: u32, serialization_type: SerializationType) -> Self {
        Self {
            payload_size,
            serialization_type,
        }
    }

    pub fn from_bytes(bytes: &[u8; Self::SIZE]) -> Result<Self, MessageError> {
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

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        [
            (self.payload_size & 0xff) as u8,
            (self.payload_size >> 8 & 0xff) as u8,
            (self.payload_size >> 16 & 0xff) as u8,
            (self.payload_size >> 24 & 0xff) as u8,
            self.serialization_type as u8,
        ]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageToServer {
    pub globals: LoreGlobalArgs,
    pub command: LoreCommand,
}

impl MessageToServer {
    pub async fn invoke(self, callback: LoreEventCallback) -> i32 {
        self.command.invoke_local(self.globals, callback).await
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
pub enum MessageToClient {
    Event(LoreEvent),
    ApiResult(i32),
}
