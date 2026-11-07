// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

/// Errors the `lore-fuse` protocol layer can produce. They stay inside FUSE: the provider maps
/// those that leave it to Lore's errors.
#[derive(Debug, thiserror::Error)]
pub enum FuseError {
    #[error("fusermount3 was not found")]
    FusermountMissing,
    #[error("mount failed: {0}")]
    MountFailed(String),
    #[error("unmount failed: {0}")]
    UnmountFailed(String),
    #[error("kernel FUSE protocol {minor} is older than the required {required}")]
    ProtocolTooOld { minor: u32, required: u32 },
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("{0}")]
    Internal(String),
}

impl From<std::io::Error> for FuseError {
    fn from(value: std::io::Error) -> Self {
        FuseError::Internal(value.to_string())
    }
}
