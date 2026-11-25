// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

pub mod aws;
pub mod aws_error;
pub mod clients;
pub mod dynamodb;
pub mod s3;
pub mod store;
pub mod telemetry;

/// Whether the [`s3::S3`] and [`dynamodb::DynamoDb`] facades are the `test-util` mocks rather than
/// the service clients.
///
/// Cargo unifies features across one invocation, so building this crate's own tests alongside a
/// dependent hands the dependent the mocks as well.
pub const FACADES_ARE_MOCKS: bool = cfg!(feature = "test-util");

/// The whole-operation budget, in milliseconds, for a deployment that configures none.
///
/// The default of [`clients::TimeoutConfig`] and of the `timeout_millis` setting of the immutable
/// and mutable stores.
pub(crate) fn default_aws_timeout_millis() -> u64 {
    5_000
}
