// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use serde::Deserialize;
use serde::Serialize;

/// Kind of value a stored key refers to.
/// cbindgen:prefix-with-name
/// cbindgen:rename-all=ScreamingSnakeCase
#[repr(C)]
#[derive(Debug, Copy, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum KeyType {
    /// Key has no specific type.
    #[default]
    Untyped = 0,
    /// Key refers to branch metadata.
    BranchMetadata = 1,
    /// Key refers to a branch identifier.
    BranchId = 2,
    /// Key refers to a pointer to a branch's latest revision.
    BranchLatestPointer = 3,
    /// Key refers to repository metadata.
    RepositoryMetadata = 4,
    /// Key refers to a repository identifier.
    RepositoryId = 5,
    /// Key refers to a repository instance.
    Instance = 6,
}

impl TryFrom<u8> for KeyType {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(KeyType::Untyped),
            1 => Ok(KeyType::BranchMetadata),
            2 => Ok(KeyType::BranchId),
            3 => Ok(KeyType::BranchLatestPointer),
            4 => Ok(KeyType::RepositoryMetadata),
            5 => Ok(KeyType::RepositoryId),
            6 => Ok(KeyType::Instance),
            other => Err(other),
        }
    }
}

impl TryFrom<u32> for KeyType {
    type Error = u32;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        if value > u8::MAX as u32 {
            return Err(value);
        }
        KeyType::try_from(value as u8).map_err(|_unknown| value)
    }
}
