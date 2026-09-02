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
use super::filesystem_provider::FilesystemPath;
use super::filesystem_provider::FilesystemProvider;
use super::filesystem_provider::FsError;
use super::filesystem_provider::InstanceOperation;
use super::filesystem_provider::InstanceOperationImpl;
use super::filesystem_provider::StaticDispatchInstanceOperation;
use crate::change::NodeChange;
use crate::filter::FilterMode;
use crate::immutable;
use crate::merge::MergeTextMode;
use crate::merge::merge3_text_by_path;
use crate::node::Node;
use crate::node::NodeID;
use crate::repository::RepositoryContext;
use crate::state::FilesystemDiffStats;
use crate::state::NodeComparison;
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

    fn begin_operation(&self) -> Result<Arc<InstanceOperationImpl>, FsError> {
        Ok(Arc::new(InstanceOperationImpl::new(
            StaticDispatchInstanceOperation::Os(OsOperation {
                repo_path: self.repo_path.clone(),
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
    repo_path: PathBuf,
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
        crate::state::diff_filesystem_subtree(
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
        .forward_any::<FsError>("Failed to diff filesystem")
    }

    async fn file_info(&self, path: FilesystemPath<'_>) -> Result<FileInfo, FsError> {
        match lore_io::IoDriver::global()
            .metadata(path.as_absolute_path())
            .await
        {
            Ok(metadata) => Ok(FileInfo::from_metadata(metadata)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileInfo::default()),
            Err(e) => Err(e.into()),
        }
    }

    async fn file_hash(
        &self,
        repository: Arc<RepositoryContext>,
        path: FilesystemPath<'_>,
        node_hint: Option<&Node>,
    ) -> Result<Hash, FsError> {
        Ok(immutable::hash_file(
            repository.clone(),
            path.as_absolute_path(),
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

    async fn make_executable(
        &self,
        path: FilesystemPath<'_>,
        executable: bool,
    ) -> Result<(), FsError> {
        #[cfg(unix)]
        {
            let absolute_path = path.as_absolute_path();
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

    async fn create_dir_all(&self, path: FilesystemPath<'_>) -> Result<(), FsError> {
        lore_io::IoDriver::global()
            .create_dir_all(path.as_absolute_path())
            .await?;
        Ok(())
    }

    async fn create_file(&self, path: FilesystemPath<'_>) -> Result<(), FsError> {
        lore_io::IoDriver::global()
            .write_file_bytes(path.as_absolute_path(), bytes::Bytes::new(), false)
            .await?;
        Ok(())
    }

    async fn unify_case_rename(
        &self,
        from: FilesystemPath<'_>,
        to: FilesystemPath<'_>,
    ) -> Result<(), FsError> {
        util::fs::unify_name_case_rename(from.as_absolute_path(), to.as_absolute_path()).await?;
        Ok(())
    }

    async fn remove(&self, path: FilesystemPath<'_>) -> Result<(), FsError> {
        util::fs::unlink(path.as_absolute_path()).await?;
        Ok(())
    }

    async fn remove_recursive(&self, path: FilesystemPath<'_>) -> Result<(), FsError> {
        util::fs::unlink_recursive(path.as_absolute_path()).await?;
        Ok(())
    }

    async fn set_file_to_immutable_store_contents(
        &self,
        repository: Arc<RepositoryContext>,
        node: &Node,
        path: FilesystemPath<'_>,
    ) -> Result<(Fragment, Option<FileInfo>), FsError> {
        let options = immutable::read_options_from_repository(&repository);
        let (fragment, metadata) = immutable::read_into_file(
            repository,
            node.address,
            path.as_absolute_path(),
            None,
            options,
        )
        .await
        .forward_any::<FsError>("Failed to read file")?;
        Ok((fragment, metadata.map(FileInfo::from_metadata)))
    }

    async fn copy_to_scratch_file(
        &self,
        source_path: FilesystemPath<'_>,
        destination_path: impl AsRef<Path> + Send,
    ) -> Result<(), FsError> {
        lore_io::IoDriver::global()
            .copy(source_path.as_absolute_path(), destination_path.as_ref())
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
            crate::infer::infer_is_diffable_by_path(path.as_absolute_path())
                .await
                .unwrap_or(false),
        )
    }

    async fn finalize(&self, _success: bool) -> Result<(), FsError> {
        // No-op for OS filesystem
        Ok(())
    }
}
