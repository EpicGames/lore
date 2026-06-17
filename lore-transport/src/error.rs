// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::error::*;
use lore_error_set::prelude::*;

#[error_set(clone)]
pub enum ProtocolError {
    Disconnected,
    SlowDown,
    NotAuthorized,
    NotAuthenticated,
    Maintenance,
    NotFound,
    NoRemote,
    NotSupported,
    Oversized,
}

impl From<tonic::Status> for ProtocolError {
    fn from(value: tonic::Status) -> Self {
        match value.code() {
            tonic::Code::Unavailable | tonic::Code::Unknown => Self::from(Disconnected),
            tonic::Code::PermissionDenied => Self::from(NotAuthorized),
            tonic::Code::NotFound => Self::from(NotFound),
            tonic::Code::ResourceExhausted => Self::from(SlowDown),
            tonic::Code::OutOfRange => Self::from(Oversized {
                context: value.message().to_string(),
            }),
            tonic::Code::Unimplemented => Self::from(NotSupported {
                operation: value.message().to_string(),
            }),
            _ => Self::internal(value.to_string()),
        }
    }
}

impl From<ProtocolError> for tonic::Status {
    fn from(value: ProtocolError) -> Self {
        let msg = value.to_string();
        match value {
            ProtocolError::NotAuthenticated(_) => Self::new(tonic::Code::Unauthenticated, msg),
            ProtocolError::NotAuthorized(_) => Self::new(tonic::Code::PermissionDenied, msg),
            ProtocolError::SlowDown(_) => Self::new(tonic::Code::ResourceExhausted, msg),
            ProtocolError::NotFound(_) => Self::new(tonic::Code::NotFound, msg),
            ProtocolError::Oversized(_) => Self::new(tonic::Code::OutOfRange, msg),
            ProtocolError::Disconnected(_) | ProtocolError::Maintenance(_) => {
                Self::new(tonic::Code::Unavailable, msg)
            }
            ProtocolError::NotSupported(_) => Self::new(tonic::Code::Unimplemented, msg),
            ProtocolError::NoRemote(_) | ProtocolError::Internal(_) => {
                Self::new(tonic::Code::Internal, msg)
            }
        }
    }
}
