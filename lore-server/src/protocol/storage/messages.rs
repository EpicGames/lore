// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fmt::Debug;
use std::string::FromUtf8Error;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use enum_dispatch::enum_dispatch;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_storage::StoreError;
use thiserror::Error;
use tracing::warn;

use crate::auth::jwt::JwtVerifier;
use crate::protocol::attribute_map::AttributeMap;
use crate::protocol::storage::responses;

#[derive(Debug, Error, PartialEq)]
pub enum MessageParseError {
    #[error("Failed to parse branch name: {0}")]
    BranchNameParseFailure(#[from] FromUtf8Error),
    #[error("Unable to parse empty slice")]
    EmptySlice,
    #[error("Invalid field length")]
    InvalidFieldLength,
    #[error("Invalid ping value")]
    InvalidPingValue,
    #[error("Invalid query length, should be a multiple of {}", size_of::<Address>())]
    InvalidQueryLength,
    #[error("Failed to parse message: {0}")]
    ParseFailure(&'static str),
    #[error("Expected {0} bytes, but got {1}")]
    SizeMismatch(usize, usize),
    #[error("Too many fragments specified (maximum: {0}, got: {1})")]
    TooManyFragments(usize, usize),
    #[error("Unknown/unsupported opcode: {0}")]
    UnknownOpcode(u8),
}

#[derive(Debug, Error)]
pub enum MessageHandleError {
    #[error("Authorization failed ({0})")]
    AuthorizationFailure(String),
    #[error("Already connected to a repository")]
    AlreadyConnected,
    #[error("Branch already exists")]
    BranchExists,
    #[error("Branch name mismatch")]
    BranchMismatch,
    #[error("Branch is protected")]
    BranchProtected,
    #[error("Fragment not found")]
    FragmentNotFound,
    #[error("Hash for content did not match the provided hash")]
    HashMismatch,
    #[error("Branch parent does not match commit parent")]
    InvalidParentBranch,
    #[error("Internal error")]
    InternalError,
    #[error("Authorization failed: missing token")]
    MissingToken,
    #[error("Mutable data not found for hash: {0}")]
    MutableDataNotFound(Hash),
    #[error("Branch does not exist")]
    NoSuchBranch,
    #[error("Not connected to a repository")]
    NotConnected,
    #[error("Operation not implemented")]
    NotImplemented,
    #[error("Failed to query fragments, size of results did not match size of fragments")]
    QueryResultSizeMismatch,
    #[error("Store operation failed")]
    StoreFailure,
    #[error("Server overloaded, slow down")]
    SlowDown,
    #[error("Fragment or blob exceeded size limit")]
    Oversized,
    #[error("Metadata operation failed")]
    Metadata,
    #[error("Failed to compute hash for content")]
    HashFailed,
    #[error("Failed to validate fragment")]
    InvalidFragment,
    #[error("Failed to handle the request in time")]
    HandlerTimeout,
    #[error("Session Limit Reached")]
    SessionLimitReached,
}

impl From<StoreError> for MessageHandleError {
    fn from(value: StoreError) -> Self {
        warn!("Received store error: {value:?}");
        match value {
            StoreError::SlowDown(_) => Self::SlowDown,
            StoreError::Oversized(_) => Self::Oversized,
            _ => Self::StoreFailure,
        }
    }
}

#[async_trait]
#[enum_dispatch]
pub trait Message: Debug + Send + Sync {
    async fn handle(
        &self,
        _context: Arc<AttributeMap>,
        _immutable_store: Arc<dyn ImmutableStore>,
    ) -> Result<LoreResponse, MessageHandleError> {
        Err(MessageHandleError::NotImplemented)
    }

    async fn handle_auth(
        &self,
        _context: Arc<AttributeMap>,
        _jwt_verifier: Arc<Option<JwtVerifier>>,
    ) -> Result<LoreResponse, MessageHandleError> {
        Err(MessageHandleError::NotImplemented)
    }

    async fn handle_mutable(
        &self,
        _context: Arc<AttributeMap>,
        _mutable_store: Arc<dyn MutableStore>,
    ) -> Result<LoreResponse, MessageHandleError> {
        Err(MessageHandleError::NotImplemented)
    }
}

#[enum_dispatch]
pub trait Response {
    fn data(&self) -> Vec<Bytes>;
}

#[derive(Debug, PartialEq)]
#[enum_dispatch(Response)]
pub enum LoreResponse {
    Connect(responses::ConnectResponse),
    Copy(responses::CopyResponse),
    Get(responses::GetResponse),
    Put(responses::PutResponse),
    Query(responses::QueryResponse),
    Ping(responses::PingResponse),
    Correlate(responses::CorrelateResponse),
    Verify(responses::VerifyResponse),
    MutableLoad(responses::MutableLoadResponse),
    MutableStore(responses::MutableStoreResponse),
    MutableCas(responses::MutableCasResponse),
}
