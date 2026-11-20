// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::convert::Infallible;

use lore_base::error::*;
use lore_error_set::prelude::*;

use crate::grpc::address_not_found_details;
use crate::grpc::address_not_found_status;
use crate::grpc::carries_branch_advanced;

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

/// The failures of a branch push at the remote: those of any call, and the remote's refusal of a
/// push onto a branch latest that has moved on.
#[error_set(clone)]
pub enum BranchPushError {
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
    BranchAdvanced,
}

impl From<ProtocolError> for BranchPushError {
    #[track_caller]
    fn from(err: ProtocolError) -> Self {
        let Err(err) = Err::<Infallible, _>(err).forward::<Self>("pushing a branch");
        err
    }
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

/// The error of a branch push the remote answered with `status`: `FAILED_PRECONDITION` with the
/// details of Lore Server's refusal of a push onto a branch latest that has moved, a push that
/// isn't a fast-forward or a fast-forward merge that conflicts
/// ([`branch_advanced_status`](crate::grpc::branch_advanced_status)), is `BranchAdvanced`, with
/// the remote's reason on its trace. Every other status maps as any call's does, a hook's
/// `FAILED_PRECONDITION` and an older Lore Server's refusal, which carry no details, included.
#[lore_macro::test_pub]
pub(crate) fn branch_push_error(status: tonic::Status) -> BranchPushError {
    if status.code() == tonic::Code::FailedPrecondition && carries_branch_advanced(&status) {
        return with_remote_reason(BranchPushError::from(BranchAdvanced), &status);
    }
    BranchPushError::from(ProtocolError::from(status))
}

/// `error` with the message of `status`, the remote's reason for the refusal or the slow down, on
/// its trace when the remote gave one.
fn with_remote_reason<E: ErrorSet>(mut error: E, status: &tonic::Status) -> E {
    if !status.message().is_empty() {
        error.push_context(status.message());
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
