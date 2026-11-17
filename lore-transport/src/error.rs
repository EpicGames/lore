// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::error::*;
use lore_error_set::prelude::*;

use crate::grpc::address_not_found_details;
use crate::grpc::address_not_found_status;

#[error_set(clone)]
pub enum ProtocolError {
    Disconnected,
    SlowDown,
    NotAuthorized,
    NotAuthenticated,
    Maintenance,
    NotFound,
    AddressNotFound,
    NoRemote,
    NotSupported,
    Oversized,
}

impl From<tonic::Status> for ProtocolError {
    fn from(value: tonic::Status) -> Self {
        match value.code() {
            tonic::Code::Unavailable | tonic::Code::Unknown => ProtocolError::from(Disconnected),
            tonic::Code::Unauthenticated => {
                with_remote_reason(ProtocolError::from(NotAuthenticated), &value)
            }
            tonic::Code::PermissionDenied => {
                with_remote_reason(ProtocolError::from(NotAuthorized), &value)
            }
            tonic::Code::NotFound => ProtocolError::from(NotFound),
            tonic::Code::FailedPrecondition => match address_not_found_details(&value) {
                Some(error) => ProtocolError::from(error),
                None => ProtocolError::internal_with_context(value, "remote rejected the request"),
            },
            tonic::Code::ResourceExhausted => {
                with_remote_reason(ProtocolError::from(SlowDown), &value)
            }
            tonic::Code::OutOfRange => ProtocolError::from(Oversized {
                context: value.message().to_string(),
            }),
            tonic::Code::Unimplemented => ProtocolError::from(NotSupported {
                operation: value.message().to_string(),
            }),
            _ => ProtocolError::internal(value.to_string()),
        }
    }
}

/// `error` with the message of `status`, the remote's reason for the refusal or the slow down, on
/// its trace when the remote gave one.
fn with_remote_reason(mut error: ProtocolError, status: &tonic::Status) -> ProtocolError {
    if !status.message().is_empty() {
        error.push_trace(lore_error_set::Location::with_context(
            file!(),
            line!(),
            column!(),
            Arc::from(status.message()),
        ));
    }
    error
}

impl From<ProtocolError> for tonic::Status {
    fn from(value: ProtocolError) -> Self {
        let msg = value.to_string();
        match value {
            ProtocolError::NotAuthenticated(_) => {
                tonic::Status::new(tonic::Code::Unauthenticated, msg)
            }
            ProtocolError::NotAuthorized(_) => {
                tonic::Status::new(tonic::Code::PermissionDenied, msg)
            }
            ProtocolError::SlowDown(_) => tonic::Status::new(tonic::Code::ResourceExhausted, msg),
            ProtocolError::NotFound(_) => tonic::Status::new(tonic::Code::NotFound, msg),
            ProtocolError::AddressNotFound(error) => address_not_found_status(&error, msg),
            ProtocolError::Oversized(_) => tonic::Status::new(tonic::Code::OutOfRange, msg),
            ProtocolError::Disconnected(_) | ProtocolError::Maintenance(_) => {
                tonic::Status::new(tonic::Code::Unavailable, msg)
            }
            ProtocolError::NotSupported(_) => tonic::Status::new(tonic::Code::Unimplemented, msg),
            ProtocolError::NoRemote(_) | ProtocolError::Internal(_) => {
                tonic::Status::new(tonic::Code::Internal, msg)
            }
        }
    }
}
