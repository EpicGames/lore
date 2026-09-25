// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::errors::InvalidArguments;
use crate::event;
use crate::event::EventError;
use crate::fs::filesystem_provider::FileInfo;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::with_operation;
use crate::immutable;
use crate::interface::LoreError;
use crate::interface::LoreString;
use crate::lore::Hash;
use crate::repository::RepositoryContext;
use crate::util::path::repository_relative_path;

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

/// The content hash of the file at `path`, reported as an event along with the file's size.
///
/// A path the working tree holds is read through an operation on it, so a virtual filesystem
/// answers for it. One outside the root, or under the dot directory, is read from the host
/// filesystem as named: the command hashes any file the caller can name, not only tracked ones.
// TODO(mjansson): If this is a file in the repository, get the current address
pub async fn hash(
    repository: Arc<RepositoryContext>,
    path: impl AsRef<Path>,
) -> Result<Hash, HashError> {
    let tracked = repository
        .path()
        .and_then(|root| repository_relative_path(root, &path.as_ref().to_string_lossy()));

    let (size, hash) = if let Some(tracked) = &tracked {
        with_operation::<_, HashError, _>(repository.file_system(), async |operation| {
            let size = hashed_file_size(
                operation
                    .file_info(tracked)
                    .await
                    .forward_any::<HashError>("reading file information")?,
            )?;
            let hash = immutable::hash_file(repository.clone(), &operation.content_source(tracked))
                .await
                .forward_any::<HashError>("hashing file")?;
            Ok((size, hash))
        })
        .await?
    } else {
        let metadata = lore_io::IoDriver::global()
            .metadata(path.as_ref())
            .await
            .map_err(|err| InvalidArguments {
                reason: format!("path does not exist or is not accessible: {err}"),
            })?;
        let size = hashed_file_size(FileInfo::from_metadata(&metadata))?;
        let hash = immutable::hash_file(
            repository.clone(),
            &lore_storage::ContentSource::file(path.as_ref()),
        )
        .await
        .forward_any::<HashError>("hashing file")?;
        (size, hash)
    };

    event::LoreEvent::FileHash(LoreFileHashEventData {
        path: LoreString::from_path(path),
        size,
        hash,
    })
    .send();

    Ok(hash)
}

/// The size of the file `file_info` describes, refusing a path holding anything else: it has no
/// content to hash.
fn hashed_file_size(file_info: FileInfo) -> Result<u64, HashError> {
    match file_info {
        FileInfo::File { size, .. } => Ok(size),
        FileInfo::NotExist | FileInfo::Directory => Err(InvalidArguments {
            reason: "path is not a file".into(),
        }
        .into()),
    }
}
