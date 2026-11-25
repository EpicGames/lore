// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum AwsError<E> {
    /// The call failed: the service answered with an error, the transport failed, or the
    /// operation's budget was spent.
    ///
    /// The stores report the shed cases among these as a back-off and every other as an internal
    /// failure.
    #[error("AWS operation failed: {0}")]
    Aws(E),
    /// A paged read was answered with the cursor it resumed from. Asking again returns the same
    /// page, so this is a terminal failure rather than a back-off.
    #[error("DynamoDB reported another page without advancing its cursor")]
    StalledCursor,
    #[error("Failed to join task")]
    JoinError,
    /// A `BatchGetItem` chunk still had unprocessed keys after its last resend. The table kept
    /// shedding the read, so the stores report a back-off.
    #[error("a DynamoDB BatchGetItem chunk left {remaining} key(s) unprocessed after all resends")]
    UnprocessedKeys { remaining: usize },
}
