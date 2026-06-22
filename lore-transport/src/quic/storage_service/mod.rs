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
        value as Self
    }
}

impl TryFrom<QuicOpCode> for Command {
    type Error = UnknownCommand;

    fn try_from(value: QuicOpCode) -> Result<Self, Self::Error> {
        match value {
            v if v == Self::Authorize as u8 => Ok(Self::Authorize),
            v if v == Self::Get as u8 => Ok(Self::Get),
            v if v == Self::Put as u8 => Ok(Self::Put),
            v if v == Self::Query as u8 => Ok(Self::Query),
            v if v == Self::Verify as u8 => Ok(Self::Verify),
            v if v == Self::Copy as u8 => Ok(Self::Copy),
            v if v == Self::MutableLoad as u8 => Ok(Self::MutableLoad),
            v if v == Self::MutableStore as u8 => Ok(Self::MutableStore),
            v if v == Self::MutableCas as u8 => Ok(Self::MutableCas),
            v if v == Self::GetMetadata as u8 => Ok(Self::GetMetadata),
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

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum QueryStatus {
    /// Address exist with full match including context
    ExistFullMatch = 0,
    /// Hash exist in repository, but not in given context
    ExistHashMatch = 1,
    /// Hash does not exist in repository
    NotFound = 3,
}

impl From<u8> for QueryStatus {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::ExistFullMatch,
            1 => Self::ExistHashMatch,
            _ => Self::NotFound,
        }
    }
}

impl From<usize> for QueryStatus {
    fn from(value: usize) -> Self {
        match value {
            0 => Self::ExistFullMatch,
            1 => Self::ExistHashMatch,
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
                Self::ExistFullMatch => "Full match",
                Self::ExistHashMatch => "Hash match",
                Self::NotFound => "Not found",
            }
        )
    }
}
