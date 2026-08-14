// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::error::LoreResultExt;
use crate::errors::InvalidArguments;
use crate::event;
use crate::event::EventError;
use crate::immutable;
use crate::interface::LoreError;
use crate::interface::LoreString;
use crate::lore::Hash;
use crate::repository::RepositoryContext;
use crate::util;

#[error_set]
pub enum HashError {
    InvalidArguments,
}

impl EventError for HashError {
    fn translated(&self) -> LoreError {
        LoreError::Internal
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Data for the event reporting the hash of a single file.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileHashEventData {
    /// Path of the file.
    pub path: LoreString,
    /// Size of the file in bytes.
    pub size: u64,
    /// Content hash of the file.
    pub hash: Hash,
}

pub async fn hash(
    repository: Arc<RepositoryContext>,
    path: impl AsRef<Path>,
) -> Result<Hash, HashError> {
    let metadata = lore_io::IoDriver::global()
        .metadata(path.as_ref())
        .await
        .emit_map_err(InvalidArguments {
            reason: "path does not exist or is not accessible".into(),
        })?;
    if !metadata.is_file() {
        return Err(InvalidArguments {
            reason: "path is not a file".into(),
        }
        .into());
    }

    // TODO(mjansson): If this is a file in the repository, get the current address
    let hash = immutable::hash_file(repository.clone(), path.as_ref(), None, None)
        .await
        .internal("hashing file")?;

    event::LoreEvent::FileHash(LoreFileHashEventData {
        path: LoreString::from_path(path),
        size: util::fs::file_size(&metadata),
        hash,
    })
    .send();

    Ok(hash)
}
