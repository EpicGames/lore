// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Requests that are for Storage protocol.
// Some requests are common to both QUIC V2 and the GRPC Storage Service, while
// some are specific to just V2.

pub mod authorize;
pub mod connect;
pub mod copy;
pub mod correlate;
pub mod get;
pub mod messages;
pub mod mutable_cas;
pub mod mutable_load;
pub mod mutable_store_handler;
pub mod ping;
pub mod presign_download;
pub mod put;
pub mod query;
pub mod session;
pub mod verify;

pub mod requests {
    pub use crate::protocol::storage::connect::Connect;
    pub use crate::protocol::storage::copy::Copy;
    pub use crate::protocol::storage::correlate::Correlate;
    pub use crate::protocol::storage::get::Get;
    pub use crate::protocol::storage::mutable_cas::MutableCas;
    pub use crate::protocol::storage::mutable_load::MutableLoad;
    pub use crate::protocol::storage::mutable_store_handler::MutableStoreOp;
    pub use crate::protocol::storage::ping::Ping;
    pub use crate::protocol::storage::presign_download::PresignDownload;
    pub use crate::protocol::storage::put::Put;
    pub use crate::protocol::storage::query::Query;
    pub use crate::protocol::storage::verify::Verify;
}

pub mod responses {
    // TODO(jcohen): we should put this behind an integration test feature
    pub use crate::protocol::storage::connect::ConnectResponse;
    pub use crate::protocol::storage::copy::CopyResponse;
    pub use crate::protocol::storage::correlate::CorrelateResponse;
    pub use crate::protocol::storage::get::GetResponse;
    pub use crate::protocol::storage::mutable_cas::MutableCasResponse;
    pub use crate::protocol::storage::mutable_load::MutableLoadResponse;
    pub use crate::protocol::storage::mutable_store_handler::MutableStoreResponse;
    pub use crate::protocol::storage::ping::PingResponse;
    pub use crate::protocol::storage::presign_download::PresignDownloadResponse;
    pub use crate::protocol::storage::put::PutResponse;
    pub use crate::protocol::storage::query::QueryResponse;
    pub use crate::protocol::storage::verify::VerifyResponse;
}
