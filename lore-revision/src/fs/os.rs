// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! OS-backed filesystem provider implementation.
//!
//! This module provides a zero-cost filesystem provider that delegates directly to
//! the operating system via the lore-io driver.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_error_set::WrapInternal;

use super::filesystem_provider::FileDifferenceFromNode;
use super::filesystem_provider::FileInfo;
use super::filesystem_provider::FileModifiedCheck;
use super::filesystem_provider::FilesystemPath;
use super::filesystem_provider::FilesystemProvider;
use super::filesystem_provider::FsError;
use super::filesystem_provider::InstanceOperation;
use super::filesystem_provider::StaticDispatchInstanceOperation;
use crate::change::NodeChange;
use crate::filter::FilterMode;
use crate::immutable;
use crate::lore_trace;
use crate::merge::MergeTextMode;
use crate::merge::merge3_text_by_path;
use crate::node::Node;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::repository::RepositoryContext;
use crate::state::FilesystemDiffStats;
use crate::state::State;
use crate::util;
use crate::util::path::RelativePath;

/// OS-backed filesystem provider.
pub struct OsFilesystem {
    repo_path: PathBuf,
}

impl OsFilesystem {
    /// Create a new OS-backed filesystem provider.
    pub fn new(repo_path: impl AsRef<Path>) -> Self {
        Self {
            repo_path: repo_path.as_ref().to_path_buf(),
        }
    }
}

#[async_trait]
impl FilesystemProvider for OsFilesystem {
    async fn begin_operation(&self) -> Result<Arc<StaticDispatchInstanceOperation>, FsError> {
        Ok(Arc::new(StaticDispatchInstanceOperation::Os(OsOperation {
            repo_path: self.repo_path.clone(),
        })))
    }
}

/// OS-backed filesystem operation context.
pub struct OsOperation {
    repo_path: PathBuf,
}

impl OsOperation {
    fn absolute_path(&self, path: FilesystemPath<'_>) -> PathBuf {
        path.to_absolute(&self.repo_path)
    }
}

/// All operations delegate to the regular OS file system.
impl InstanceOperation for OsOperation {
    async fn changes_from_filesystem_to_state(
        &self,
        repository_from: Arc<RepositoryContext>,
        state_from: Arc<State>,
        repository_current: Arc<RepositoryContext>,
        state_current: Arc<State>,
        node_path: RelativePath,
        root_node_from: NodeID,
        root_node_to: NodeID,
        filter_mode: FilterMode,
    ) -> Result<(Vec<NodeChange>, FilesystemDiffStats), FsError> {
        Ok(crate::state::diff_filesystem_subtree(
            repository_from,
            state_from,
            repository_current,
            state_current,
            node_path,
            root_node_from,
            root_node_to,
            filter_mode,
            std::sync::Arc::new(Vec::new()),
        )
        .await
        .internal("Failed to diff filesystem")?)
    }

    async fn file_info(&self, path: FilesystemPath<'_>) -> Result<FileInfo, FsError> {
        match lore_io::IoDriver::global()
            .metadata(self.absolute_path(path))
            .await
        {
            Ok(metadata) => {
                let (mtime, size) = crate::util::fs::file_mtime_and_size(&metadata);
                let executable = crate::util::fs::file_is_executable(&metadata);
                Ok(FileInfo {
                    exists: true,
                    is_file: metadata.is_file(),
                    is_dir: metadata.is_dir(),
                    executable,
                    size,
                    mtime,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileInfo::default()),
            Err(e) => Err(e.into()),
        }
    }

    async fn is_file_modified(
        &self,
        repository: Arc<RepositoryContext>,
        node_change: &NodeChange,
        force_full_check: bool,
    ) -> Result<FileModifiedCheck, FsError> {
        let info = self
            .file_info(FilesystemPath::Repository(&node_change.path))
            .await?;

        if !info.exists {
            return Ok(FileModifiedCheck::default());
        }

        let from_node = if node_change.from.node.is_valid_node_id() {
            Some(
                node_change
                    .from
                    .get_node()
                    .await
                    .internal("Failed to find node")?,
            )
        } else {
            None
        };

        // Only check content modification if both filesystem and node are files
        let modification = if info.is_file
            && let Some(from_node) = from_node.as_ref()
            && from_node.is_file()
        {
            lore_trace!(
                "Path {} type change {}, node size {}, file size {}",
                node_change.path,
                info.is_file != from_node.is_file(),
                from_node.size,
                info.size
            );
            if from_node.is_file() {
                let (modified, hash) = crate::state::is_file_modified(
                    repository,
                    from_node,
                    info.mtime,
                    info.size,
                    &node_change.path,
                    force_full_check,
                )
                .await
                .internal("Failed to check file modification")?;
                Some(FileDifferenceFromNode { modified, hash })
            } else {
                None
            }
        } else {
            None
        };

        Ok(FileModifiedCheck {
            info,
            from_node,
            modification,
        })
    }

    async fn file_hash(
        &self,
        repository: Arc<RepositoryContext>,
        path: &RelativePath,
        node_hint: Option<&Node>,
    ) -> Result<Hash, FsError> {
        let absolute_path = self.absolute_path(FilesystemPath::Repository(path));
        Ok(immutable::hash_file(
            repository.clone(),
            absolute_path.as_path(),
            node_hint.and_then(|node| {
                if !node.address.is_zero() {
                    Some(node.address)
                } else {
                    None
                }
            }),
            node_hint.and_then(|node| {
                if !node.size > 0 {
                    Some(node.size as usize)
                } else {
                    None
                }
            }),
        )
        .await
        .unwrap_or_default())
    }

    async fn file_compare(
        &self,
        repository: Arc<RepositoryContext>,
        address: Address,
        path: &RelativePath,
        known_disk_file_size: u64,
    ) -> Result<bool, FsError> {
        Ok(crate::state::is_file_content_equal(
            repository,
            address,
            &self.absolute_path(FilesystemPath::Repository(path)),
            known_disk_file_size,
        )
        .await)
    }

    async fn make_executable(&self, path: FilesystemPath<'_>) -> Result<(), FsError> {
        #[cfg(unix)]
        {
            let absolute_path = self.absolute_path(path);
            use std::os::unix::fs::PermissionsExt;
            let metadata = lore_io::IoDriver::global().metadata(&absolute_path).await?;
            let mut permissions = metadata.permissions();
            let mode = permissions.mode();
            permissions.set_mode(mode | 0o111); // Add execute permission for user, group, others
            lore_io::IoDriver::global()
                .set_permissions(&absolute_path, permissions)
                .await?;
        }

        // No-op on Windows
        #[cfg(not(unix))]
        {
            let _ = path; // Suppress unused variable warning
        }

        Ok(())
    }

    async fn create_dir_all(&self, path: FilesystemPath<'_>) -> Result<(), FsError> {
        lore_io::IoDriver::global()
            .create_dir_all(self.absolute_path(path))
            .await?;
        Ok(())
    }

    async fn create_file(&self, path: FilesystemPath<'_>) -> Result<(), FsError> {
        lore_io::IoDriver::global()
            .write_file_bytes(
                self.absolute_path(path).as_path(),
                bytes::Bytes::new(),
                false,
            )
            .await?;
        Ok(())
    }

    async fn unify_case_rename(
        &self,
        from: FilesystemPath<'_>,
        to: FilesystemPath<'_>,
    ) -> Result<(), FsError> {
        let from_abs = self.absolute_path(from);
        let to_abs = self.absolute_path(to);
        util::fs::unify_name_case_rename(&from_abs, &to_abs).await?;
        Ok(())
    }

    async fn remove(&self, path: FilesystemPath<'_>) -> Result<(), FsError> {
        util::fs::unlink(self.absolute_path(path).as_path()).await?;
        Ok(())
    }

    async fn remove_recursive(&self, path: FilesystemPath<'_>) -> Result<(), FsError> {
        util::fs::unlink_recursive(self.absolute_path(path).as_path()).await?;
        Ok(())
    }

    async fn set_file_to_immutable_store_contents(
        &self,
        repository: Arc<RepositoryContext>,
        node: &Node,
        path: FilesystemPath<'_>,
    ) -> Result<(), FsError> {
        if node.size > 0 {
            let options = immutable::read_options_from_repository(&repository);
            immutable::read_into_file(
                repository,
                node.address,
                self.absolute_path(path).as_path(),
                options,
            )
            .await
            .internal("Failed to read file")?;
        }
        Ok(())
    }

    async fn copy_to_scratch_file(
        &self,
        source_path: FilesystemPath<'_>,
        destination_path: impl AsRef<Path> + Send,
    ) -> Result<(), FsError> {
        lore_io::IoDriver::global()
            .copy(self.absolute_path(source_path), destination_path.as_ref())
            .await?;
        Ok(())
    }

    async fn merge3_text_by_path(
        &self,
        base: &RelativePath,
        mine: &RelativePath,
        theirs: &RelativePath,
        result: &RelativePath,
        mode: MergeTextMode<'_>,
    ) -> Result<bool, FsError> {
        Ok(merge3_text_by_path(&self.repo_path, base, mine, theirs, result, mode).await?)
    }

    async fn infer_is_diffable(&self, path: FilesystemPath<'_>) -> Result<bool, FsError> {
        Ok(
            crate::infer::infer_is_diffable_by_path(&self.absolute_path(path))
                .await
                .unwrap_or(false),
        )
    }

    async fn finalize(&self, _success: bool) -> Result<(), FsError> {
        // No-op for OS filesystem
        Ok(())
    }
}
