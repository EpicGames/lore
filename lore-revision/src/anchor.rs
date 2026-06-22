// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::io;
use std::io::Read;
use std::path::Path;

use lore_error_set::prelude::*;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

use crate::errors::InvalidArguments;
use crate::errors::WriteRequired;
use crate::event::EventError;
use crate::interface::LoreError;
use crate::lore::BranchId;
use crate::lore::Hash;
use crate::lore_spawn_blocking;

#[error_set]
pub enum AnchorError {
    InvalidArguments,
    WriteRequired,
}

impl EventError for AnchorError {
    fn translated(&self) -> LoreError {
        match self {
            Self::InvalidArguments(_) => LoreError::InvalidArguments,
            Self::WriteRequired(_) | Self::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

pub const CURRENT: &str = "current";
pub const STAGED: &str = "staged";

/// Read an old file-based anchor (48 bytes: 32-byte hash + 16-byte branch ID)
/// for migration purposes only.
pub async fn deserialize_migrate_old(
    path: impl AsRef<Path> + Send,
) -> std::io::Result<(Hash, BranchId)> {
    let path = path.as_ref().to_path_buf();

    #[repr(C)]
    #[derive(Default, IntoBytes, FromBytes, Immutable)]
    struct OldAnchor {
        signature: Hash,
        branch: BranchId,
    }

    lore_spawn_blocking!(move || {
        let mut anchor = OldAnchor::default();
        std::fs::File::open(path)?.read_exact(anchor.as_mut_bytes())?;
        Ok((anchor.signature, anchor.branch))
    })
    .await
    .map_err(io::Error::other)
    .flatten()
}
