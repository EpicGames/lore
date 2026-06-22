// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
/// Result of a heal operation on a fragment
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HealResult {
    /// No heal was attempted.
    #[default]
    NotAttempted = 0,
    /// The fragment was healed.
    Healed = 1,
    /// Technically healing should always succeed, this mostly exists as a fail-safe in the event
    /// we get an unexpected result from the store operation.
    Failed = 2,
}

impl From<u8> for HealResult {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::NotAttempted,
            1 => Self::Healed,
            _ => Self::Failed,
        }
    }
}

impl From<i32> for HealResult {
    fn from(value: i32) -> Self {
        Self::from(value as u8)
    }
}

/// Result of verifying a fragment
#[derive(Debug, Clone, Copy, Default)]
pub struct VerifyResult {
    /// Whether corruption was detected
    pub corrupted: bool,
    /// Result of heal operation
    pub healed: HealResult,
}
