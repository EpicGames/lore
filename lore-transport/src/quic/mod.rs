// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod client;
pub mod command_header;
pub mod net_runtime;
mod response_reader;
pub mod storage_service;

use std::sync::Arc;
use std::sync::Weak;

use command_header::CommandHeader;
use lore_base::types::RepositoryId;
use lore_credential::domain_from_url_str_or_url;
use lore_error_set::prelude::*;
use thiserror::Error;

use crate::connection::Connection;
use crate::error::ProtocolError;
use crate::quic::storage_service::client::StorageClient;
use crate::traits::Storage;

pub const PACKET_THRESHOLD: u32 = 3;
pub const TIME_THRESHOLD: f32 = 9.0 / 8.0;
pub const MAX_RTT_MS: u64 = 2000;

pub type QuicOpCode = u8;
pub type QuicErrorStatus = u32;

pub const RESERVED_ERROR_CODE_START: u32 = 200;

/// These are the error status that a service can return.
/// Service-agnostic errors can occur (such as failing to read bytes) surfaced
/// by the server scaffolding, and some errors might be raised by the service
/// implementation, using the common error status or the reserved range.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum QuicServiceError {
    // core service-agnostic errors that can occur from handling requests
    InvalidCommand = 1,
    NotAuthorized = 2,
    Failed = 3,
    NotFound = 4,
    Oversized = 5,
    SlowDown = 100,
    // service specific implementations can use 200-299
    ImplementationReserved = RESERVED_ERROR_CODE_START,
    ImplementationReservedEnd = 299,
}

#[derive(Clone, Debug, Error)]
pub enum QuicClientError {
    #[error("Failed to open bidirectional stream")]
    StreamOpen,
    #[error("The client did not send the message because it is larger than the agreed chunk size")]
    ClientMessageTooBig,
    #[error("Failed to write chunks to stream")]
    WriteChunks,
    #[error("Failed to read chunks from stream")]
    ReadChunks,
    #[error("Server returned error code {0}")]
    ServerError(QuicErrorStatus),
    #[error("Failed writing command to stream")]
    Write,
    #[error("Failed reading response from stream")]
    Read,
    #[error("A cryptography error has occurred that cannot be retried")]
    CrytpoError,
    #[error("Server sent an invalid response: {0:?}")]
    InvalidResponse(CommandHeader),
    #[error("Server sent an unexpected response, command not pending: {0:?}")]
    UnexpectedCommand(CommandHeader),
    #[error("Connection terminated")]
    Terminated,
    #[error("Failed to acquire command permit")]
    Permit,
    #[error("Slow down")]
    SlowDown,
    #[error("Not authorized")]
    NotAuthorized,
    #[error("Not found")]
    NotFound,
    #[error("Oversized fragment rejected by server")]
    Oversized,
}

#[derive(Debug)]
pub struct UnknownCommand(pub QuicOpCode);

pub async fn storage(
    connection: Weak<Connection>,
    remote_url: &str,
    auth_url: &str,
    identity: &str,
    repository: RepositoryId,
) -> Result<Arc<dyn Storage>, ProtocolError> {
    let remote_domain = domain_from_url_str_or_url(remote_url)
        .internal(&format!("remote {remote_url} is invalid"))?;

    let storage = StorageClient::connect(
        connection,
        remote_url,
        remote_domain,
        auth_url,
        identity,
        repository,
    )
    .await
    .internal(&format!("connecting to {remote_url}"))?;

    Ok(Arc::new(storage))
}
