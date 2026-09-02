// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_error_set::prelude::*;

use crate::errors::*;

#[error_set]
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

/// Map a `ProtocolError` to a `StorageError`, preserving the address when available.
pub fn protocol_error_to_storage(
    err: lore_transport::ProtocolError,
    address: lore_base::types::Address,
) -> StorageError {
    if err.is_not_found() || err.is_no_remote() {
        StorageError::from(AddressNotFound::from(address))
    } else if err.is_disconnected() {
        StorageError::from(Disconnected)
    } else if err.is_slow_down() {
        StorageError::from(SlowDown)
    } else {
        StorageError::from(NotConnected {
            reason: format!("{err}"),
        })
    }
}
