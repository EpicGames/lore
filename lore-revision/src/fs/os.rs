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
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_error_set::prelude::*;

use super::filesystem_provider::FileInfo;
use super::filesystem_provider::FilesystemDiffContext;
use super::filesystem_provider::FilesystemProvider;
use super::filesystem_provider::FsError;
use super::filesystem_provider::InstanceOperation;
use super::filesystem_provider::InstanceOperationImpl;
use super::filesystem_provider::StaticDispatchInstanceOperation;
use crate::change::NodeChange;
use crate::immutable;
use crate::merge::MergeTextMode;
use crate::merge::merge3_text_by_path;
use crate::node::Node;
use crate::node::NodeFileMode;
use crate::repository::RepositoryContext;
use crate::state::FilesystemDiffStats;
use crate::state::NodeComparison;
use crate::util;
use crate::util::path::RelativePath;

/// OS-backed filesystem provider.
pub struct OsFilesystem {
    filesystem_root: PathBuf,
}

impl OsFilesystem {
    /// Create a new OS-backed filesystem provider.
    pub fn new(filesystem_root: impl AsRef<Path>) -> Self {
        Self {
            filesystem_root: filesystem_root.as_ref().to_path_buf(),
        }
    }

    fn begin_operation(&self) -> Result<Arc<InstanceOperationImpl>, FsError> {
        Ok(Arc::new(InstanceOperationImpl::new(
            StaticDispatchInstanceOperation::Os(OsOperation {
                filesystem_root: self.filesystem_root.clone(),
            }),
        )))
    }
}

#[async_trait]
impl FilesystemProvider for OsFilesystem {
    async fn begin_operation(&self) -> Result<Arc<InstanceOperationImpl>, FsError> {
        OsFilesystem::begin_operation(self)
    }
}

/// OS-backed filesystem operation context.
pub struct OsOperation {
    /// Where the mounted filesystem starts, which every repository in it shares: a link
    /// or layer context inherits its parent's path, so this is not a repository's root.
    filesystem_root: PathBuf,
}

impl OsOperation {
    /// Where `path` is on disk, under the root the operation was opened on -- the top-level
    /// repository every link and layer in it shares.
    fn absolute(&self, path: &RelativePath) -> PathBuf {
        path.to_absolute_path(&self.filesystem_root)
    }
}

/// All operations delegate to the regular OS file system.
impl InstanceOperation for OsOperation {
    async fn changes_from_filesystem_to_state(
        &self,
        diff: FilesystemDiffContext,
        changes: &mut Vec<NodeChange>,
    ) -> Result<FilesystemDiffStats, FsError> {
        crate::state::diff_os_filesystem(diff, changes)
            .await
            .forward_any::<FsError>("Failed to diff filesystem")
    }

    /// A path mid-deletion stats as `PermissionDenied` on Windows rather than
    /// `NotFound`, so both report a non-existent path.
    async fn file_info(&self, path: &RelativePath) -> Result<FileInfo, FsError> {
        let path = self.absolute(path);
        match lore_io::IoDriver::global().metadata(path).await {
            Ok(metadata) => Ok(FileInfo::from_metadata(&metadata)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileInfo::default()),
            Err(e)
                if cfg!(target_family = "windows")
                    && e.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                Ok(FileInfo::default())
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn holds_name_exactly(&self, path: &RelativePath) -> Option<bool> {
        let path = self.absolute(path);
        crate::util::fs::holds_name_exactly(path).await
    }

    async fn names_folding_to(
        &self,
        path: &RelativePath,
        name: &str,
    ) -> Result<Vec<String>, FsError> {
        let path = self.absolute(path);
        Ok(crate::util::fs::names_folding_to(path, name).await?)
    }

    async fn file_hash(
        &self,
        repository: Arc<RepositoryContext>,
        path: &RelativePath,
        node_hint: Option<&Node>,
    ) -> Result<Hash, FsError> {
        Ok(immutable::hash_file(
            repository.clone(),
            self.absolute(path),
            node_hint.and_then(|node| {
                if !node.address.is_zero() {
                    Some(node.address)
                } else {
                    None
                }
            }),
            node_hint.and_then(|node| {
                if node.size > 0 {
                    Some(node.size as usize)
                } else {
                    None
                }
            }),
        )
        .await
        .unwrap_or_default())
    }

    async fn compare_file_to_node(
        &self,
        repository: Arc<RepositoryContext>,
        node: &Node,
        path: &RelativePath,
        file_size: u64,
        content: &lore_storage::ContentHashMemo<'_>,
    ) -> Result<NodeComparison, FsError> {
        crate::state::file_matches_node(repository, node, file_size, path, Some(content))
            .await
            .forward_any::<FsError>("Failed to compare file to node")
    }

    async fn make_executable(&self, path: &RelativePath, executable: bool) -> Result<(), FsError> {
        let path = self.absolute(path);
        #[cfg(unix)]
        {
            let absolute_path = &path;
            use std::os::unix::fs::PermissionsExt;
            let metadata = lore_io::IoDriver::global().metadata(&absolute_path).await?;
            let mut permissions = metadata.permissions();
            let mode = permissions.mode();
            if executable {
                permissions.set_mode(mode | 0o111); // Add execute permission for user, group, others
            } else {
                permissions.set_mode(mode & !0o111); // Add execute permission for user, group, others
            }
            lore_io::IoDriver::global()
                .set_permissions(&absolute_path, permissions)
                .await?;
        }

        // No-op on Windows
        #[cfg(not(unix))]
        {
            // Suppress unused variable warnings
            let _ = path;
            let _ = executable;
        }

        Ok(())
    }

    async fn create_dir_all(&self, path: &RelativePath) -> Result<(), FsError> {
        let path = self.absolute(path);
        lore_io::IoDriver::global().create_dir_all(path).await?;
        Ok(())
    }

    async fn create_file(&self, path: &RelativePath) -> Result<(), FsError> {
        let path = self.absolute(path);
        lore_io::IoDriver::global()
            .write_file_bytes(path, bytes::Bytes::new(), false)
            .await?;
        Ok(())
    }

    async fn unify_case_rename(
        &self,
        from: &RelativePath,
        to: &RelativePath,
    ) -> Result<(), FsError> {
        let (from, to) = (self.absolute(from), self.absolute(to));
        util::fs::unify_name_case_rename(&from, &to).await?;
        Ok(())
    }

    async fn remove(&self, path: &RelativePath) -> Result<(), FsError> {
        let path = self.absolute(path);
        util::fs::unlink(path).await?;
        Ok(())
    }

    async fn remove_recursive(&self, path: &RelativePath) -> Result<(), FsError> {
        let path = self.absolute(path);
        util::fs::unlink_recursive(path).await?;
        Ok(())
    }

    async fn write_node(
        &self,
        repository: Arc<RepositoryContext>,
        node: &Node,
        path: &RelativePath,
    ) -> Result<FileInfo, FsError> {
        let path = self.absolute(path);
        if let Some(parent) = path.parent() {
            lore_io::IoDriver::global().create_dir_all(parent).await?;
        }

        if node.size > 0 {
            let options = immutable::read_options_from_repository(&repository);
            immutable::read_into_file(repository, node.address, &path, None, options)
                .await
                .forward_any::<FsError>("Failed to read file")?;
        } else {
            lore_io::IoDriver::global()
                .write_file_bytes(&path, bytes::Bytes::new(), false)
                .await?;
        }

        let written = lore_io::IoDriver::global().metadata(&path).await?;
        let executable = node.mode & NodeFileMode::Executable == NodeFileMode::Executable;
        util::fs::metadata_set_executable(&path, &written, executable).await;
        Ok(FileInfo::from_metadata(
            &lore_io::IoDriver::global().metadata(&path).await?,
        ))
    }

    async fn set_file_to_immutable_store_contents(
        &self,
        repository: Arc<RepositoryContext>,
        node: &Node,
        path: &RelativePath,
    ) -> Result<(Fragment, Option<FileInfo>), FsError> {
        let options = immutable::read_options_from_repository(&repository);
        let path = self.absolute(path);
        let (fragment, metadata) =
            immutable::read_into_file(repository, node.address, &path, None, options)
                .await
                .forward_any::<FsError>("Failed to read file")?;
        Ok((fragment, metadata.as_ref().map(FileInfo::from_metadata)))
    }

    async fn copy_file(
        &self,
        source_path: &RelativePath,
        destination_path: &RelativePath,
    ) -> Result<(), FsError> {
        lore_io::IoDriver::global()
            .copy(self.absolute(source_path), self.absolute(destination_path))
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
        Ok(merge3_text_by_path(&self.filesystem_root, base, mine, theirs, result, mode).await?)
    }

    async fn infer_is_diffable(&self, path: &RelativePath) -> Result<bool, FsError> {
        let path = self.absolute(path);
        Ok(crate::infer::infer_is_diffable_by_path(&path)
            .await
            .unwrap_or(false))
    }

    async fn finalize(&self, _success: bool) -> Result<(), FsError> {
        // No-op for OS filesystem
        Ok(())
    }
}
