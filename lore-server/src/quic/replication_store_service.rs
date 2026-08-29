// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fmt::Display;
use std::fmt::Formatter;

use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
use lore_storage::StoreError;
use lore_transport::quic::QuicOpCode;
use lore_transport::quic::RESERVED_ERROR_CODE_START;
use lore_transport::quic::UnknownCommand;
use lore_transport::quic::command_header::CommandHeader;

use crate::protocol::replication_store::put;

pub mod client;
pub mod client_container;
pub mod server;

pub const MAX_CHUNK_SIZE: usize =
    size_of::<CommandHeader>() + put::BASE_REQUEST_SIZE + FRAGMENT_SIZE_THRESHOLD;

/// This service will be receiving all the store traffic for all the connections
/// a downstream Lore Server is receiving, so start off with a high message throughput
pub const DEFAULT_CLIENT_MESSAGE_LIMIT: usize = 50_000;

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ReplicationServiceErrorCode {
    Internal = RESERVED_ERROR_CODE_START,
    AddressNotFound = RESERVED_ERROR_CODE_START + 1,
    SlowDown = RESERVED_ERROR_CODE_START + 2,
    PayloadNotFound = RESERVED_ERROR_CODE_START + 3,
    Oversized = RESERVED_ERROR_CODE_START + 4,
}

impl Display for ReplicationServiceErrorCode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Internal => write!(f, "Internal"),
            Self::AddressNotFound => write!(f, "AddressNotFound"),
            Self::SlowDown => write!(f, "SlowDown"),
            Self::PayloadNotFound => write!(f, "PayloadNotFound"),
            Self::Oversized => write!(f, "Oversized"),
        }
    }
}

impl From<&StoreError> for ReplicationServiceErrorCode {
    fn from(err: &StoreError) -> Self {
        match err {
            StoreError::AddressNotFound(_) => ReplicationServiceErrorCode::AddressNotFound,
            StoreError::PayloadNotFound(_) => ReplicationServiceErrorCode::PayloadNotFound,
            StoreError::SlowDown(_) => ReplicationServiceErrorCode::SlowDown,
            StoreError::Oversized(_) => ReplicationServiceErrorCode::Oversized,
            StoreError::NotFound(_)
            | StoreError::Disconnected(_)
            | StoreError::NotAuthorized(_)
            | StoreError::NotAuthenticated(_)
            | StoreError::Maintenance(_)
            | StoreError::NoRemote(_)
            | StoreError::NotSupported(_)
            | StoreError::Internal(_) => ReplicationServiceErrorCode::Internal,
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq)]
pub enum Command {
    // 0 and 5 were `ExistsBatch` / `LocalExistsBatch`, superseded by `Query` (17) and
    // `LocalQuery` (18). Left unused rather than reassigned, so a peer that still sends one is
    // rejected instead of misread.
    ImmutablePut = 2,
    ImmutableObliterate = 3,
    // 4 and 7 were the single-address `Query`, whose operation no longer exists. Left unused rather
    // than reassigned, so a peer that still sends one is rejected instead of misread.
    ImmutableLocalPut = 8,
    // 11 and 12 were `Query` / `LocalQuery` under the old response shape (without context in each
    // result). Left unused rather than reassigned, so a peer that still sends one is rejected
    // instead of misread.
    // 1, 6, 9, 10 were `Get`, `LocalGet`, `GetMetadata`, `LocalGetMetadata` under the old response
    // shape. Left unused rather than reassigned, so a peer that still sends one is rejected instead
    // of misread.
    ImmutableGet = 13,
    ImmutableLocalGet = 14,
    ImmutableGetMetadata = 15,
    ImmutableLocalGetMetadata = 16,
    ImmutableQuery = 17,
    ImmutableLocalQuery = 18,
    ImmutableCopy = 19,
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
            v if v == Command::ImmutableGet as u8 => Ok(Command::ImmutableGet),
            v if v == Command::ImmutablePut as u8 => Ok(Command::ImmutablePut),
            v if v == Command::ImmutableObliterate as u8 => Ok(Command::ImmutableObliterate),
            v if v == Command::ImmutableGetMetadata as u8 => Ok(Command::ImmutableGetMetadata),
            v if v == Command::ImmutableLocalGet as u8 => Ok(Command::ImmutableLocalGet),
            v if v == Command::ImmutableLocalGetMetadata as u8 => {
                Ok(Command::ImmutableLocalGetMetadata)
            }
            v if v == Command::ImmutableLocalPut as u8 => Ok(Command::ImmutableLocalPut),
            v if v == Command::ImmutableQuery as u8 => Ok(Command::ImmutableQuery),
            v if v == Command::ImmutableLocalQuery as u8 => Ok(Command::ImmutableLocalQuery),
            v if v == Command::ImmutableCopy as u8 => Ok(Command::ImmutableCopy),
            _ => Err(UnknownCommand(value)),
        }
    }
}

#[cfg(test)]
mod tests {
    use lore_base::error::AddressNotFound;
    use lore_base::error::PayloadNotFound;
    use lore_base::error::SlowDown;
    use lore_base::types::Address;
    use lore_base::types::Hash;
    use lore_storage::StoreError;

    use super::*;

    #[test]
    fn store_error_address_not_found_maps_to_address_not_found_code() {
        let err = StoreError::from(AddressNotFound::from(Address::default()));
        let code = ReplicationServiceErrorCode::from(&err);
        assert_eq!(code, ReplicationServiceErrorCode::AddressNotFound);
        assert_eq!(code as u32, 201);
    }

    #[test]
    fn store_error_internal_maps_to_internal_code() {
        let err = StoreError::internal("test");
        let code = ReplicationServiceErrorCode::from(&err);
        assert_eq!(code, ReplicationServiceErrorCode::Internal);
        assert_eq!(code as u32, 200);
    }

    #[test]
    fn store_error_slow_down_maps_to_slow_down_code() {
        let err = StoreError::from(SlowDown);
        let code = ReplicationServiceErrorCode::from(&err);
        assert_eq!(code, ReplicationServiceErrorCode::SlowDown);
        assert_eq!(code as u32, 202);
    }

    #[test]
    fn store_error_payload_not_found_maps_to_payload_not_found_code() {
        let err = StoreError::from(PayloadNotFound::from(Hash::default()));
        let code = ReplicationServiceErrorCode::from(&err);
        assert_eq!(code, ReplicationServiceErrorCode::PayloadNotFound);
        assert_eq!(code as u32, 203);
    }

    #[test]
    fn error_code_address_not_found_converts_to_client_service_error() {
        let client_err =
            client::ReplicationStoreClientError::from(ReplicationServiceErrorCode::AddressNotFound);
        assert!(matches!(
            client_err,
            client::ReplicationStoreClientError::ServiceError(
                ReplicationServiceErrorCode::AddressNotFound
            )
        ));
    }

    #[test]
    fn error_code_slow_down_converts_to_client_service_error() {
        let client_err =
            client::ReplicationStoreClientError::from(ReplicationServiceErrorCode::SlowDown);
        assert!(matches!(
            client_err,
            client::ReplicationStoreClientError::ServiceError(
                ReplicationServiceErrorCode::SlowDown
            )
        ));
    }

    #[test]
    fn error_code_internal_converts_to_client_service_error() {
        let client_err =
            client::ReplicationStoreClientError::from(ReplicationServiceErrorCode::Internal);
        assert!(matches!(
            client_err,
            client::ReplicationStoreClientError::ServiceError(
                ReplicationServiceErrorCode::Internal
            )
        ));
    }

    #[test]
    fn error_code_payload_not_found_converts_to_client_service_error() {
        let client_err =
            client::ReplicationStoreClientError::from(ReplicationServiceErrorCode::PayloadNotFound);
        assert!(matches!(
            client_err,
            client::ReplicationStoreClientError::ServiceError(
                ReplicationServiceErrorCode::PayloadNotFound
            )
        ));
    }
}
