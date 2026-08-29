// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fmt::Debug;

use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum AwsError<E> {
    /// The SDK error is boxed because it carries the raw HTTP response and runs to several hundred
    /// bytes, a cost every `Result<_, AwsError<_>>` in this crate would otherwise pay on the success
    /// path as well. Construct it with [`AwsError::sdk_error`].
    #[error("AWS SDK operation failed: {0:?}")]
    AwsSdkError(Box<E>),
    #[error("Dynamo BatchGetItem received empty keys")]
    MissingKeys,
    #[error("Failed to build batch request")]
    BatchRequestError,
    #[error("Failed to join task")]
    JoinError,
}

impl<E> AwsError<E> {
    /// Wrap an SDK error, boxing the payload.
    pub fn sdk_error(error: E) -> Self {
        Self::AwsSdkError(Box::new(error))
    }
}
