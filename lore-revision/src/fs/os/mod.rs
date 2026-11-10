// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! OS-backed filesystem provider implementation.
//!
//! This module provides a zero-cost filesystem provider that delegates directly to
//! the operating system via the lore-io driver. The directory listings and name lookups
//! the operation is built on sit here with it, reaching the driver for this provider alone.

#[cfg(not(feature = "test-util"))]
mod clone;
#[cfg(feature = "test-util")]
pub mod clone;

use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::FutureExt;
use lore_base::error::InvalidPath;
use lore_base::types::Address;
use lore_base::types::Fragment;
use lore_error_set::prelude::*;
use lore_io::StatKind;
use lore_io::clone::CloneStats;

use super::filesystem_provider::CloneAnswer;
use super::filesystem_provider::CloneRun;
use super::filesystem_provider::DirectoryEntry;
use super::filesystem_provider::DirectoryListing;
use super::filesystem_provider::FileInfo;
use super::filesystem_provider::FilesystemDiffContext;
use super::filesystem_provider::FilesystemProvider;
use super::filesystem_provider::FsError;
use super::filesystem_provider::InstanceCloneError;
use super::filesystem_provider::InstanceOperation;
use super::filesystem_provider::InstanceOperationImpl;
use super::filesystem_provider::StaticDispatchDirectoryListing;
use super::filesystem_provider::StaticDispatchInstanceOperation;
use crate::hash::hash_string;
use crate::immutable;
use crate::lore_debug;
use crate::node::Node;
use crate::node::NodeFileMode;
use crate::repository::RepositoryContext;
use crate::state::ChangeStream;
use crate::state::FilesystemDiffStats;
use crate::state::NodeComparison;
use crate::util;
use crate::util::path::PathError;
use crate::util::path::RelativePath;

/// OS-backed filesystem provider.
#[derive(Debug)]
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

    pub fn begin_operation(&self) -> OsOperation {
        OsOperation {
            filesystem_root: self.filesystem_root.clone(),
        }
    }
}

#[async_trait]
impl FilesystemProvider for OsFilesystem {
    async fn begin_operation(&self) -> Result<Arc<InstanceOperationImpl>, FsError> {
        Ok(Arc::new(InstanceOperationImpl::new(
            StaticDispatchInstanceOperation::Os(OsFilesystem::begin_operation(self)),
            self.holds_state(),
        )))
    }

    async fn clone_answer(
        &self,
        source: &Path,
        dot_path: &Path,
        destination: &Path,
    ) -> Result<CloneAnswer, InstanceCloneError> {
        clone::answer(source, dot_path, destination).await
    }

    async fn clone_instance(
        &self,
        source: &Path,
        dot_path: &Path,
        destination: &Path,
        answer: &CloneAnswer,
        copy_unclonable: bool,
        run: &CloneRun,
    ) -> Result<CloneStats, InstanceCloneError> {
        clone::clone(source, dot_path, destination, answer, copy_unclonable, run).await
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
    fn absolute(&self, path: &RelativePath) -> Result<PathBuf, FsError> {
        path.to_absolute_path(&self.filesystem_root)
            .forward::<FsError>("placing a path below the filesystem root")
    }

    /// Deletes a directory and its contents as [`InstanceOperation::remove_recursive`] does, but
    /// keeps each nested repository in it, with the directories leading to one.
    pub(crate) async fn remove_recursive_keeping_repositories(
        &self,
        path: &RelativePath,
    ) -> Result<(), FsError> {
        util::fs::unlink_recursive_keeping_repositories(&self.absolute(path)?).await?;
        Ok(())
    }
}

/// A directory read from the OS file system, one chunk of entries and their metadata per
/// dispatch to the io driver.
pub struct OsDirectoryListing {
    entries: lore_io::DirStream,
}

impl OsDirectoryListing {
    /// The next entry the repository tracks, passing over every entry it holds nothing for.
    pub(crate) async fn next(&mut self) -> Option<Result<DirectoryEntry, FsError>> {
        while let Some(entry) = self.entries.next().await {
            match file_list_item(entry)
                .forward_any::<FsError>("A directory entry names what is not text")
            {
                Ok(Some(item)) => {
                    return Some(Ok(DirectoryEntry {
                        name: item.name,
                        info: FileInfo::from_stat(&item.metadata),
                        name_hash: item.name_hash,
                    }));
                }
                Ok(None) => {}
                Err(err) => return Some(Err(err)),
            }
        }
        None
    }
}

/// All operations delegate to the regular OS file system.
impl InstanceOperation for OsOperation {
    fn changes_from_filesystem_to_state(
        &self,
        diff: FilesystemDiffContext,
    ) -> ChangeStream<FilesystemDiffStats> {
        ChangeStream::spawn(async move |changes| {
            crate::state::os_diff::diff_os_filesystem(diff, &changes).await
        })
    }

    /// A path mid-deletion stats as `PermissionDenied` on Windows rather than
    /// `NotFound`, and a path below a file as `NotADirectory`, so all of them report a
    /// non-existent path.
    async fn file_info(&self, path: &RelativePath) -> Result<FileInfo, FsError> {
        let path = self.absolute(path)?;
        match lore_io::IoDriver::global().stat(path).await {
            Ok(stat) => Ok(FileInfo::from_stat(&stat)),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(FileInfo::NotExist)
            }
            Err(e)
                if cfg!(target_family = "windows")
                    && e.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                Ok(FileInfo::NotExist)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// The same lookup as [`file_info`](Self::file_info): the working tree is the only view
    /// this provider has, so a tracked path and an untracked one are read alike.
    async fn untracked_file_info(&self, path: &RelativePath) -> Result<FileInfo, FsError> {
        self.file_info(path).await
    }

    /// A path through a name no node may carry, which the root is refused for, is held in no
    /// spelling.
    async fn holds_name_exactly(&self, path: &RelativePath) -> Option<bool> {
        let Ok(path) = self.absolute(path) else {
            return Some(false);
        };
        lore_io::IoDriver::global().holds_name_exactly(path).await
    }

    async fn read_directory(&self, path: &RelativePath) -> Result<DirectoryListing, FsError> {
        let path = self.absolute(path)?;
        Ok(DirectoryListing::new(StaticDispatchDirectoryListing::Os(
            OsDirectoryListing {
                entries: lore_io::IoDriver::global().read_dir(path).await?,
            },
        )))
    }

    async fn content_source(
        &self,
        path: &RelativePath,
    ) -> Result<lore_storage::ContentSource<'static>, FsError> {
        Ok(lore_storage::ContentSource::owned_file(
            self.absolute(path)?,
        ))
    }

    /// Measures the file the operation's root holds at `path` against the stored object's own
    /// fragmentation, which is the only comparison that holds: a commit may reuse a previous
    /// fragmentation, so the stored hash is a function of the content and of how it came to be
    /// chunked, and re-hashing the content from scratch does not reproduce it.
    ///
    /// Fetches fragment metadata but never content payloads, so the cost is bounded by the file
    /// however large the stored object is.
    ///
    /// The source is named per call and the hashes come from `established`, so a caller measuring
    /// one path against several addresses reads it no more than the answers require.
    async fn file_holds_content(
        &self,
        repository: Arc<RepositoryContext>,
        path: &RelativePath,
        previous: Address,
        previous_size: u64,
        established: &lore_storage::ContentHashes,
    ) -> Result<NodeComparison, FsError> {
        let source = lore_storage::ContentSource::owned_file(self.absolute(path)?);
        let matched = crate::immutable::file_matches(
            repository,
            previous,
            Some(previous_size as usize),
            &source,
            established,
        )
        .await
        .forward_any::<FsError>("Failed to compare the file to stored content")?;

        Ok(crate::state::node_comparison(matched))
    }

    async fn make_executable(&self, path: &RelativePath, executable: bool) -> Result<(), FsError> {
        util::fs::set_executable(self.absolute(path)?, None, executable).await?;
        Ok(())
    }

    async fn create_dir_all(&self, path: &RelativePath) -> Result<(), FsError> {
        let path = self.absolute(path)?;
        lore_io::IoDriver::global().create_dir_all(path).await?;
        Ok(())
    }

    async fn write_file(&self, path: &RelativePath, contents: Bytes) -> Result<(), FsError> {
        let path = self.absolute(path)?;
        lore_io::IoDriver::global()
            .write_file_bytes(path, contents, false)
            .await?;
        Ok(())
    }

    async fn rename(&self, from: &RelativePath, to: &RelativePath) -> Result<(), FsError> {
        let (from, to) = (self.absolute(from)?, self.absolute(to)?);
        rename_unifying(&from, &to).await?;
        Ok(())
    }

    /// Read from the file system's own identity of the file each path names, symbolic links
    /// followed, as [`lore_io::IoDriver::names_one_entry`] reads it.
    async fn names_one_entry(
        &self,
        from: &RelativePath,
        to: &RelativePath,
    ) -> Result<Option<bool>, FsError> {
        Ok(lore_io::IoDriver::global()
            .names_one_entry(self.absolute(from)?, self.absolute(to)?)
            .await?)
    }

    async fn remove(&self, path: &RelativePath) -> Result<(), FsError> {
        let path = self.absolute(path)?;
        util::fs::unlink(path).await?;
        Ok(())
    }

    async fn remove_recursive(&self, path: &RelativePath) -> Result<(), FsError> {
        let path = self.absolute(path)?;
        util::fs::unlink_recursive(path).await?;
        Ok(())
    }

    async fn remove_empty_directory(&self, path: &RelativePath) -> Result<(), FsError> {
        lore_io::IoDriver::global()
            .remove_dir(self.absolute(path)?)
            .await?;
        Ok(())
    }

    /// Takes the mode for the executable bit from the metadata the write captured, or from a
    /// stat by path where the write captured none, and stats the file by path for the
    /// [`FileInfo`] it answers: the write takes its metadata on the open handle, and a
    /// filesystem that settles the stamp at close gives the file a later time.
    ///
    /// The directory above the file is created where the write finds it missing, so a file
    /// written into a directory that is there costs no call on that directory.
    async fn write_node(
        &self,
        repository: Arc<RepositoryContext>,
        node: &Node,
        path: &RelativePath,
    ) -> Result<FileInfo, FsError> {
        let path = self.absolute(path)?;
        {
            let options = immutable::read_options_from_repository(&repository).creating_parent();
            let (_, written) =
                write_addressed_content(repository, node.address, &path, options).await?;
            let executable = node.mode & NodeFileMode::Executable == NodeFileMode::Executable;
            util::fs::metadata_set_executable(&path, written.as_ref(), executable).await;
        }
        Ok(FileInfo::from_stat(
            &lore_io::IoDriver::global().stat(path).await?,
        ))
    }

    async fn set_file_to_immutable_store_contents(
        &self,
        repository: Arc<RepositoryContext>,
        address: Address,
        path: &RelativePath,
    ) -> Result<(Fragment, Option<FileInfo>), FsError> {
        let path = self.absolute(path)?;
        let options = immutable::read_options_from_repository(&repository);
        let (fragment, metadata) =
            write_addressed_content(repository, address, &path, options).await?;
        Ok((fragment, metadata.as_ref().map(FileInfo::from_metadata)))
    }

    async fn copy_file(
        &self,
        source_path: &RelativePath,
        destination_path: &RelativePath,
    ) -> Result<(), FsError> {
        lore_io::IoDriver::global()
            .copy(
                self.absolute(source_path)?,
                self.absolute(destination_path)?,
            )
            .await?;
        Ok(())
    }
}

/// Represents a single filesystem item.
/// Used for directory children enumeration and single file metadata.
pub struct FileListItem {
    /// The name of the file/directory (not the full path).
    pub name: String,
    /// What the name holds: its kind, size, modification time and permission bits.
    pub metadata: lore_io::Stat,
    /// Pre-computed hash of the lowercase name for efficient lookups.
    pub name_hash: u64,
}

/// Result of listing a filesystem path.
/// Provides type-safe distinction between file and directory cases.
pub enum PathListingResult {
    /// The path was a directory.
    ///
    /// The listing yields an entry per child, named relative to the directory (just the
    /// filename, not the full path). [`file_list_item`] turns one into a [`FileListItem`].
    Directory { listing: lore_io::DirStream },

    /// The path was a regular file.
    ///
    /// The `item.name` is the filename component of the path that was queried.
    /// For example, querying `/foo/bar/file.txt` yields `item.name = "file.txt"`.
    File { item: FileListItem },

    /// The path did not exist, was not accessible, or was a special file type (device, socket)
    /// that we don't handle. A link is stat'd through to what it points at, so one naming a file
    /// or a directory answers as that rather than as a kind we hold nothing for.
    NotFound,
}

impl PathListingResult {
    /// Returns true if the path was a directory.
    pub fn is_directory(&self) -> bool {
        matches!(self, PathListingResult::Directory { .. })
    }

    /// Returns true if the path was a file.
    pub fn is_file(&self) -> bool {
        matches!(self, PathListingResult::File { .. })
    }

    /// Returns true if the path was not found or not accessible.
    pub fn is_not_found(&self) -> bool {
        matches!(self, PathListingResult::NotFound)
    }
}

/// Describes one listing entry.
///
/// `Ok(None)` is an entry the walk carries nothing for: a name it could not read, or one whose
/// metadata would not resolve — a broken link, or a name unlinked while the walk was running —
/// and one the repository does not track, which is a link and anything the file system holds as
/// neither a file nor a directory. A link is answered for by what it is rather than by what it
/// points at, so one naming a file is skipped as the link it is. A caller enumerating what is
/// present skips all of those; one unreadable name says nothing about the rest of the directory.
///
/// An error is a name that is not text. Nothing can be done with such a name that is not a
/// guess: it hashes to a node the tree does not hold, and staging it would record a name no
/// file answers to. Only the message it is reported in spells it lossily.
pub fn file_list_item(
    entry: std::io::Result<lore_io::DirEntry>,
) -> Result<Option<FileListItem>, PathError> {
    let Ok(entry) = entry else {
        return Ok(None);
    };
    if entry.is_symlink {
        return Ok(None);
    }
    let Some(metadata) = entry.metadata else {
        return Ok(None);
    };
    if metadata.kind == lore_io::StatKind::Other {
        return Ok(None);
    }
    let name = entry_name(entry.file_name)?;
    let name_hash = hash_string(name.as_str());
    Ok(Some(FileListItem {
        name,
        metadata,
        name_hash,
    }))
}

/// A directory entry's name as text, taking the bytes the listing already owns rather than a
/// copy of them. A name that is not text is an error naming it lossily.
pub fn entry_name(name: std::ffi::OsString) -> Result<String, PathError> {
    name.into_string().map_err(|name| {
        InvalidPath {
            path: name.to_string_lossy().into_owned(),
        }
        .into()
    })
}

/// Lists a filesystem path, automatically handling both file and directory cases.
///
/// # Arguments
/// * `path` - The filesystem path to list
///
/// # Returns
/// * `PathListingResult::Directory` - If path is a directory, with a listing of its children
/// * `PathListingResult::File` - If path is a single file, with its metadata
/// * `PathListingResult::NotFound` - If path doesn't exist or isn't accessible
///
/// # Path Semantics
/// - For directories: Each item's `name` is the child filename (e.g., "file.txt")
/// - For files: The item's `name` is the filename component (e.g., "file.txt" for "/foo/file.txt")
///
/// Listing is attempted before the path is described, since a caller walking a tree reaches this
/// with a directory almost every time — the walk recurses into those and compares files in place.
pub async fn list_path(path: PathBuf) -> Result<PathListingResult, PathError> {
    let driver = lore_io::IoDriver::global();

    if let Ok(listing) = driver.read_dir(path.as_path()).await {
        return Ok(PathListingResult::Directory { listing });
    }

    let Ok(metadata) = driver.stat(path.as_path()).await else {
        return Ok(PathListingResult::NotFound);
    };

    if metadata.kind == lore_io::StatKind::File {
        let file_name = match path.file_name() {
            Some(name) => entry_name(name.to_os_string())?,
            None => String::new(),
        };
        let name_hash = hash_string(file_name.as_str());

        Ok(PathListingResult::File {
            item: FileListItem {
                name: file_name,
                metadata,
                name_hash,
            },
        })
    } else {
        // Symlink or other special file type
        Ok(PathListingResult::NotFound)
    }
}

/// Puts the content `address` names at `path`, read with `options`, with the fragment it came
/// from and what the write left there where the write reported it.
///
/// The zero hash every empty file carries addresses nothing and leaves an empty file: the store
/// answers for it without being read, there being no content to find.
///
/// Returns [`immutable::read_into_file`]'s future with its error forwarded, without a future of
/// its own.
fn write_addressed_content(
    repository: Arc<RepositoryContext>,
    address: Address,
    path: &Path,
    options: lore_storage::ReadOptions,
) -> impl Future<Output = Result<(Fragment, Option<std::fs::Metadata>), FsError>> {
    immutable::read_into_file(repository, address, path, None, options)
        .map(|written| written.forward_any::<FsError>("Failed to read file"))
}

/// Carries the file at `from_path` to `to_path` without a rename, which is what a move between two
/// filesystems takes: no rename crosses one.
async fn copy_and_unlink(from_path: &Path, to_path: &Path) -> std::io::Result<()> {
    let driver = lore_io::IoDriver::global();
    lore_debug!(
        "Copying {} -> {}, no rename carries it",
        from_path.display(),
        to_path.display()
    );
    driver.copy(from_path, to_path).await?;
    driver.remove_file(from_path).await?;
    Ok(())
}

/// The OS arm of [`InstanceOperation::rename`]: moves what the file system holds at `from_path` to
/// `to_path`, by rename where one lands and by [`unify_refused_rename`] where none does. On
/// Windows a directory renamed onto a file lands: the system's rename replaces the file with the
/// directory.
///
/// Only a refused rename is boxed, the unification recursing through it, so a rename that lands
/// boxes no future. Not an `async fn`, which would hold a second copy of its arguments.
#[lore_macro::test_pub]
#[allow(clippy::manual_async_fn)]
fn rename_unifying<'a>(
    from_path: &'a Path,
    to_path: &'a Path,
) -> impl Future<Output = std::io::Result<()>> + Send + 'a {
    async move {
        lore_debug!(
            "Try rename {} -> {}",
            from_path.display(),
            to_path.display()
        );
        match lore_io::IoDriver::global().rename(from_path, to_path).await {
            Ok(()) => {
                lore_debug!("Renamed {} -> {}", from_path.display(), to_path.display());
                Ok(())
            }
            Err(refused) => {
                let unifying: Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + 'a>> =
                    Box::pin(unify_refused_rename(from_path, to_path, refused));
                unifying.await
            }
        }
    }
}

/// Moves what the file system holds at `from_path` to `to_path` by hand, the rename between them
/// having been refused with `refused`.
///
/// A file goes to `to_path` whether or not a file is there, one that is there being removed first.
/// A directory hands each child over to `to_path` through [`rename_unifying`], a child whose name
/// the destination also holds being unified again, before it goes; a destination that is not there
/// is created to take them. A `from_path` and `to_path` of different kinds are refused, there being
/// no move that leaves one of them.
///
/// A `to_path` naming the entry `from_path` does, as another spelling of its name does on a file
/// system that folds case, or one that cannot be told apart from it, is not unified: removing or
/// emptying it could remove the source. `refused` is answered and both stay as they stand.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[lore_macro::test_pub]
#[allow(clippy::manual_async_fn)]
fn unify_refused_rename<'a>(
    from_path: &'a Path,
    to_path: &'a Path,
    refused: std::io::Error,
) -> impl Future<Output = std::io::Result<()>> + Send + 'a {
    async move {
        let driver = lore_io::IoDriver::global();
        let from_kind = driver.stat(from_path).await?.kind;
        let to_kind = match driver.stat(to_path).await {
            Ok(stat) => Some(stat.kind),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err),
        };

        if let Some(to_kind) = to_kind
            && (from_kind == StatKind::Directory) != (to_kind == StatKind::Directory)
        {
            return Err(tokio::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Unable to rename, file/directory mismatch",
            ));
        }

        if to_kind.is_some()
            && !matches!(
                driver.names_one_entry(from_path, to_path).await,
                Ok(Some(false) | None)
            )
        {
            return Err(refused);
        }

        if from_kind == StatKind::File {
            if to_kind.is_some() {
                driver.remove_file(to_path).await?;
                if driver.rename(from_path, to_path).await.is_ok() {
                    return Ok(());
                }
            }
            copy_and_unlink(from_path, to_path).await?;
        } else {
            if to_kind.is_none() {
                driver.create_dir_all(to_path).await?;
            }
            // The listing is drained before the directory goes, so nothing is still walking it.
            let names = {
                let mut listing = driver.read_dir(from_path).await?;
                let mut names = Vec::new();
                while let Some(entry) = listing.next().await {
                    names.push(entry?.file_name);
                }
                names
            };
            for name in names {
                let from_path = from_path.join(&name);
                let to_path = to_path.join(&name);
                rename_unifying(&from_path, &to_path).await?;
            }
            driver.remove_dir_all(from_path).await?;
        }

        lore_debug!("Renamed {} -> {}", from_path.display(), to_path.display());
        Ok(())
    }
}
