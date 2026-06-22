// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use super::QuicErrorStatus;
use super::QuicOpCode;

pub const COMMAND_HEADER_SIZE: usize = 8;
pub const COMMAND_HEADER_SIZE_V4: usize = 12;

#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct CommandHeader {
    pub cmd: u8,
    pub error: bool,
    /// Either the size of a successful response, or the `QuicErrorStatus`.
    /// Sent over the wire as u23
    pub size_or_status: u32,
    pub command_id: u32,
    /// Session identifier for lore-storage/0.4. Always 0 for urc/0.2 headers.
    pub session_id: u32,
    /// Runtime flag indicating this header uses the v4 12-byte format.
    /// Not serialized — set by the stream handler based on the protocol version.
    pub v4: bool,
}

impl CommandHeader {
    pub fn new(command: QuicOpCode, id: u32, size: usize) -> Self {
        Self {
            cmd: command,
            error: false,
            size_or_status: size as u32,
            command_id: id,
            session_id: 0,
            v4: false,
        }
    }

    pub fn new_with_session(command: QuicOpCode, id: u32, size: usize, session_id: u32) -> Self {
        Self {
            cmd: command,
            error: false,
            size_or_status: size as u32,
            command_id: id,
            session_id,
            v4: true,
        }
    }

    pub fn response_error(&self, status: QuicErrorStatus) -> Self {
        Self {
            cmd: self.cmd,
            error: true,
            size_or_status: status,
            command_id: self.command_id,
            session_id: self.session_id,
            v4: self.v4,
        }
    }

    pub fn response_success(&self, size: u32) -> Self {
        Self {
            cmd: self.cmd,
            error: false,
            size_or_status: size,
            command_id: self.command_id,
            session_id: self.session_id,
            v4: self.v4,
        }
    }

    /// Serialize the response header into a stack-allocated buffer.
    /// Returns `(buffer, length)` where length is 8 for v2 or 12 for v4.
    pub fn response_bytes(&self) -> ([u8; COMMAND_HEADER_SIZE_V4], usize) {
        let mut buffer = [0u8; COMMAND_HEADER_SIZE_V4];
        buffer[0] = self.cmd;
        buffer[1] = (self.size_or_status & 0xFF) as u8;
        buffer[2] = ((self.size_or_status >> 8) & 0xFF) as u8;
        buffer[3] =
            (((self.size_or_status >> 16) & 0x7F) as u8) | (if self.error { 0x80 } else { 0 });
        buffer[4..8].copy_from_slice(&self.command_id.to_le_bytes());
        if self.v4 {
            buffer[8..12].copy_from_slice(&self.session_id.to_le_bytes());
            (buffer, COMMAND_HEADER_SIZE_V4)
        } else {
            (buffer, COMMAND_HEADER_SIZE)
        }
    }

    pub fn from_bytes(value: &[u8]) -> Self {
        let cmd = value[0];

        // Next 3 bytes represent the error bit and the size/status field.
        let size_status =
            (value[1] as u32) | ((value[2] as u32) << 8) | (((value[3] & 0b1111111) as u32) << 16);
        // Last bit for the error
        let error = (value[3] & 0b10000000) != 0;
        // Extract the last 4 bytes for the command_id.
        let command_id = u32::from_le_bytes({
            let mut bytes = [0u8; 4];
            bytes.copy_from_slice(&value[4..8]);
            bytes
        });

        Self {
            cmd,
            error,
            size_or_status: size_status,
            command_id,
            session_id: 0,
            v4: false,
        }
    }

    pub fn from_bytes_v4(value: &[u8]) -> Self {
        let mut header = Self::from_bytes(value);
        header.session_id = u32::from_le_bytes({
            let mut bytes = [0u8; 4];
            bytes.copy_from_slice(&value[8..12]);
            bytes
        });
        header.v4 = true;
        header
    }

    pub fn to_bytes(&self) -> [u8; 8] {
        let mut buffer = [0u8; 8];

        buffer[0] = self.cmd;

        // combine the size_status and error fields back into 3 bytes.
        buffer[1] = (self.size_or_status & 0b11111111) as u8;
        buffer[2] = ((self.size_or_status >> 8) & 0b11111111) as u8;
        buffer[3] = (((self.size_or_status >> 16) & 0b01111111) as u8)
            | (if self.error { 0b10000000 } else { 0 });

        buffer[4..8].copy_from_slice(&self.command_id.to_le_bytes());

        buffer
    }

    pub fn to_bytes_v4(&self) -> [u8; 12] {
        let mut buffer = [0u8; 12];
        buffer[..8].copy_from_slice(&self.to_bytes());
        buffer[8..12].copy_from_slice(&self.session_id.to_le_bytes());
        buffer
    }
}
