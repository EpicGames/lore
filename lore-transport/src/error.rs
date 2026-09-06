// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
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
            tonic::Code::PermissionDenied => ProtocolError::from(NotAuthorized),
            tonic::Code::NotFound => ProtocolError::from(NotFound),
            tonic::Code::FailedPrecondition => address_not_found_details(&value).map_or_else(
                || ProtocolError::internal(value.to_string()),
                ProtocolError::from,
            ),
            tonic::Code::ResourceExhausted => ProtocolError::from(SlowDown),
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
