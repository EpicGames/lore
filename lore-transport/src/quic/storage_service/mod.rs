// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fmt::Display;
use std::fmt::Formatter;

use bytes::Bytes;
use lore_base::error::Disconnected;
use lore_base::error::NotAuthorized;
use lore_base::error::NotFound;
use lore_base::error::Oversized;
use lore_base::error::SlowDown;
use lore_base::types::Address;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::HealResult;
use lore_base::types::VerifyResult;
use zerocopy::FromBytes;

use super::QuicOpCode;
use super::UnknownCommand;
use super::command_header::CommandHeader;
use crate::error::ProtocolError;

mod auth;
pub mod client;
pub(crate) mod sessions;

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
    /// `mutable_load` + `get` performed server-side, saving one round trip. Resolves the key,
    /// treats the value as an immutable hash, and reads it at the caller-supplied `Context`.
    ///
    /// Request:  key `Hash` (32) ++ `Context` (16) ++ `flags` u32 LE (4)
    /// Response: resolved `Hash` (32) ++ `Fragment` (16) ++ payload (`size_payload`)
    ///
    /// The key type is always [`lore_base::types::KeyType::Resolve`] and is therefore not sent;
    /// the 4-byte `flags` tail keeps the request a multiple of 4. The resolved hash is returned
    /// so the caller can cache the key->hash mapping and verify the payload.
    /// Opcodes 4 and 5 are reserved (ping/correlate), hence 12.
    GetResolved = 12,
    /// `put` + `mutable_store` performed server-side, saving one round trip. Stores the fragment
    /// at its content address, then maps the caller's mutable key to that hash under
    /// `KeyType::Resolve` — the write that makes a key readable by [`Command::GetResolved`].
    ///
    /// Request:  key `Hash` (32) ++ `Address` (48) ++ `Fragment` (16) ++ payload (`size_payload`)
    /// Response: empty
    ///
    /// The mapping is written only once the fragment is durably stored, so a key never resolves
    /// to content the server does not hold. Only the root fragment goes through this command; a
    /// fragment list's leaves are written with ordinary [`Command::Put`] calls first.
    ///
    /// A zero `Address` hash removes the mapping instead of publishing one; the `Fragment` and
    /// payload are then ignored, since there is nothing to store.
    PutResolved = 13,
    /// Announces the client's user agent; see
    /// [`send_client_identify`](super::client::send_client_identify).
    ClientIdentify = 14,
}

/// `flags` field of a [`Command::GetResolved`] request. Reserved; no bits are defined.
///
/// Transmitted as a full `u32`. Unknown bits are rejected, not ignored.
pub mod get_resolved_flags {
    /// No optional behaviour.
    pub const NONE: u32 = 0;
    /// Bits this build accepts.
    pub const KNOWN: u32 = 0;
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
            v if v == Command::GetResolved as u8 => Ok(Command::GetResolved),
            v if v == Command::PutResolved as u8 => Ok(Command::PutResolved),
            v if v == Command::ClientIdentify as u8 => Ok(Command::ClientIdentify),
            _ => Err(UnknownCommand(value)),
        }
    }
}

impl Display for Command {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", command_name(self))
    }
}

/// The error a storage command reports when it fails with `error`.
pub(crate) fn command_error(command: &Command, error: super::QuicClientError) -> ProtocolError {
    use super::QuicClientError;
    match error {
        QuicClientError::SlowDown => ProtocolError::from(SlowDown),
        QuicClientError::NotAuthorized => ProtocolError::from(NotAuthorized),
        QuicClientError::NotFound => ProtocolError::from(NotFound),
        QuicClientError::Oversized => ProtocolError::from(Oversized {
            context: format!(
                "{}: server rejected oversized fragment",
                command_name(command)
            ),
        }),
        QuicClientError::Terminated => ProtocolError::from(Disconnected),
        other => ProtocolError::internal(format!(
            "{}: Failed sending command: {other}",
            command_name(command)
        )),
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
        Command::GetResolved => "get_resolved",
        Command::PutResolved => "put_resolved",
        Command::ClientIdentify => "client_identify",
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

/// The fragment at the front of an `operation` response, leaving `payload` holding what follows it,
/// which must be that fragment's payload.
pub(crate) fn read_fragment(
    payload: &mut Bytes,
    operation: &str,
) -> Result<Fragment, ProtocolError> {
    let Ok((fragment, _)) = Fragment::read_from_prefix(payload) else {
        return Err(ProtocolError::internal(format!(
            "{operation}: Invalid server response"
        )));
    };
    bytes::Buf::advance(payload, size_of::<Fragment>());
    if let Err(reason) = lore_base::types::validate_fragment_response(&fragment) {
        return Err(ProtocolError::internal(format!(
            "{operation}: invalid fragment {fragment:?}: {reason}"
        )));
    }
    if payload.len() != fragment.size_payload as usize {
        return Err(ProtocolError::internal(format!(
            "{operation}: Invalid server payload for fragment {fragment:?}, got {} bytes",
            payload.len()
        )));
    }
    Ok(fragment)
}

/// The hash an `operation` response holds, which is all it holds.
pub(crate) fn read_hash(payload: &Bytes, operation: &str) -> Result<Hash, ProtocolError> {
    if payload.len() != size_of::<Hash>() {
        return Err(ProtocolError::internal(format!(
            "{operation}: Invalid server response, expected {} bytes got {}",
            size_of::<Hash>(),
            payload.len()
        )));
    }
    Ok(Hash::from(&payload[..]))
}

/// The resolved hash and the fragment at the front of a `get_resolved` response, leaving `payload`
/// holding what follows them, which must be that fragment's payload.
pub(crate) fn read_resolved(payload: &mut Bytes) -> Result<(Hash, Fragment), ProtocolError> {
    let prefix = size_of::<Hash>() + size_of::<Fragment>();
    if payload.len() < prefix {
        return Err(ProtocolError::internal(format!(
            "get_resolved: Invalid server response, expected at least {prefix} bytes got {}",
            payload.len()
        )));
    }
    let resolved = Hash::from(&payload[..size_of::<Hash>()]);
    bytes::Buf::advance(payload, size_of::<Hash>());
    let fragment = read_fragment(payload, "get_resolved")?;
    Ok((resolved, fragment))
}

/// The fragment a `get_metadata` response holds, which is all it holds.
pub(crate) fn read_metadata(payload: &[u8]) -> Result<Fragment, ProtocolError> {
    Fragment::read_from_bytes(payload).map_err(|_size| {
        ProtocolError::internal(format!(
            "get_metadata: expected {} bytes for Fragment, got {}",
            size_of::<Fragment>(),
            payload.len()
        ))
    })
}

/// The outcome a `verify` response for `address` holds: whether the fragment is corrupted, then
/// what healing it did.
pub(crate) fn read_verify(
    payload: &[u8],
    address: &Address,
) -> Result<VerifyResult, ProtocolError> {
    let &[corrupted, healed] = payload else {
        return Err(ProtocolError::internal(format!(
            "verify: Invalid server payload for address {address}, got {} bytes",
            payload.len()
        )));
    };
    Ok(VerifyResult {
        corrupted: corrupted != 0,
        healed: HealResult::from(healed),
    })
}

/// Most addresses one `query` request carries.
const MAX_QUERY_BATCH: usize = lore_base::types::FRAGMENT_SIZE_EXPECTED / size_of::<Address>();

/// Refuses a `query` of `count` addresses, more than one request carries.
pub(crate) fn check_query_batch(count: usize) -> Result<(), ProtocolError> {
    if count > MAX_QUERY_BATCH {
        return Err(ProtocolError::internal(format!(
            "query: Invalid address batch count: {count}"
        )));
    }
    Ok(())
}

/// Refuses a `query` response that holds other than one status for each of `count` addresses.
pub(crate) fn check_query_statuses(payload: &[u8], count: usize) -> Result<(), ProtocolError> {
    if payload.len() != count {
        return Err(ProtocolError::internal(format!(
            "query: Server returned query payload of wrong size, expected {count} got {}",
            payload.len()
        )));
    }
    Ok(())
}
