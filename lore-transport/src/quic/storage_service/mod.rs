// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fmt::Display;
use std::fmt::Formatter;

use super::QuicOpCode;
use super::UnknownCommand;
use super::command_header::CommandHeader;

mod auth;
pub mod client;

pub const MAX_CHUNK_SIZE: usize = lore_base::types::FRAGMENT_SIZE_THRESHOLD
    + size_of::<CommandHeader>()
    + size_of::<lore_base::types::Address>()
    + size_of::<lore_base::types::Fragment>();

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Command {
    Authorize = 0,
    Get = 1,
    Put = 2,
    Query = 3,
    Verify = 6,
    Copy = 7,
    MutableLoad = 8,
    MutableStore = 9,
    MutableCas = 10,
    /// Same wire request as `Get` (just an `Address`), but the server's response carries the
    /// `Fragment` only — no payload bytes. Used by callers that need fragment metadata for
    /// existence/size lookups without paying for the payload transfer.
    GetMetadata = 11,
}

impl From<Command> for QuicOpCode {
    fn from(value: Command) -> Self {
        value as QuicOpCode
    }
}

impl TryFrom<QuicOpCode> for Command {
    type Error = UnknownCommand;

    fn try_from(value: QuicOpCode) -> Result<Self, Self::Error> {
        match value {
            v if v == Command::Authorize as u8 => Ok(Command::Authorize),
            v if v == Command::Get as u8 => Ok(Command::Get),
            v if v == Command::Put as u8 => Ok(Command::Put),
            v if v == Command::Query as u8 => Ok(Command::Query),
            v if v == Command::Verify as u8 => Ok(Command::Verify),
            v if v == Command::Copy as u8 => Ok(Command::Copy),
            v if v == Command::MutableLoad as u8 => Ok(Command::MutableLoad),
            v if v == Command::MutableStore as u8 => Ok(Command::MutableStore),
            v if v == Command::MutableCas as u8 => Ok(Command::MutableCas),
            v if v == Command::GetMetadata as u8 => Ok(Command::GetMetadata),
            _ => Err(UnknownCommand(value)),
        }
    }
}

impl Display for Command {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", command_name(self))
    }
}

pub fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Authorize => "authorize",
        Command::Get => "get",
        Command::Put => "put",
        Command::Query => "query",
        Command::Verify => "verify",
        Command::Copy => "copy",
        Command::MutableLoad => "mutable_load",
        Command::MutableStore => "mutable_store",
        Command::MutableCas => "mutable_cas",
        Command::GetMetadata => "get_metadata",
    }
}

/// What a peer will admit about an address.
///
/// The ladder a store resolves on has four levels; this has three, and the missing one is
/// deliberate. A hash held only by a partition the caller has no claim to is another tenant's
/// content, and its existence is not the caller's to learn, so it collapses into `NotFound`
/// alongside genuine absence. Value 2 is unused for that reason: it is where a hash-only match
/// would sit if it were sayable.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum QueryStatus {
    /// Address exists with a full match, including context
    ExistFullMatch = 0,
    /// Hash exists in this partition, under some other context
    ExistPartitionMatch = 1,
    /// Nothing this caller may be told about
    NotFound = 3,
}

impl From<u8> for QueryStatus {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::ExistFullMatch,
            1 => Self::ExistPartitionMatch,
            _ => Self::NotFound,
        }
    }
}

impl From<usize> for QueryStatus {
    fn from(value: usize) -> Self {
        match value {
            0 => Self::ExistFullMatch,
            1 => Self::ExistPartitionMatch,
            _ => Self::NotFound,
        }
    }
}

impl Display for QueryStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                QueryStatus::ExistFullMatch => "Full match",
                QueryStatus::ExistPartitionMatch => "Partition match",
                QueryStatus::NotFound => "Not found",
            }
        )
    }
}
