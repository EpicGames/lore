// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_error_set::prelude::*;
use lore_transport::ProtocolError;

use crate::errors::*;

#[error_set(clone)]
pub enum StorageError {
    AddressNotFound,
    PayloadNotFound,
    NotConnected,
    Disconnected,
    SlowDown,
    Oversized,
    InvalidArguments,
    Maintenance,
    NoRemote,
    NotAuthenticated,
    NotAuthorized,
    NotFound,
    NotSupported,
}

/// Map a `ProtocolError` to a `StorageError`: a miss to `AddressNotFound` at `address`, a refusal
/// to itself with its trace, which carries the remote's reason where the transport gives one,
/// `Disconnected` and `SlowDown` to themselves, and anything else to `NotConnected`.
pub fn protocol_error_to_storage(
    err: ProtocolError,
    address: lore_base::types::Address,
) -> StorageError {
    match err {
        ProtocolError::NotAuthenticated(refusal) => StorageError::NotAuthenticated(refusal),
        ProtocolError::NotAuthorized(refusal) => StorageError::NotAuthorized(refusal),
        ProtocolError::NotFound(_) | ProtocolError::NoRemote(_) => {
            StorageError::from(AddressNotFound::from(address))
        }
        ProtocolError::Disconnected(_) => StorageError::from(Disconnected),
        ProtocolError::SlowDown(_) => StorageError::from(SlowDown),
        err => StorageError::from(NotConnected {
            reason: format!("{err}"),
        }),
    }
}
