// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// The included files do not pass this lint
#![allow(clippy::doc_markdown)]
#![allow(clippy::large_enum_variant)]

#[rustfmt::skip]
#[path = "grpc/epic_urc.rs"]
pub mod auth;

// Nested to match proto package hierarchy. The generated lore.notification.rs uses
// super::super::urc::lock::Resource, which requires lock/model to be at crate::urc::*
// and notification to be at crate::lore::notification (two levels deep).
pub mod epic;
pub mod lore;
pub mod urc;

pub use urc::lock;
pub use urc::model;

#[rustfmt::skip]
#[path = "grpc/urc.rpc.rs"]
pub mod rpc;

#[rustfmt::skip]
#[path = "grpc/ucs.auth.rs"]
pub mod rebac;

mod convert;

pub use lock::lock_service_client::LockServiceClient;
pub use lock::lock_service_server::LockService;
pub use lock::lock_service_server::LockServiceServer;
pub use rebac::rebac_api_client::RebacApiClient;
pub use rpc::admin_service_client::AdminServiceClient;
pub use rpc::admin_service_server::AdminService;
pub use rpc::admin_service_server::AdminServiceServer;
pub use urc::model::*;
