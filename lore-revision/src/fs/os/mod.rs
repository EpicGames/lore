// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! OS-backed filesystem provider implementation.
//!
//! This module provides a zero-cost filesystem provider that delegates directly to
//! the operating system via the lore-io driver. The directory listings and name lookups
//! the operation is built on sit here with it, reaching the driver for this provider alone.

use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use lore_base::error::InvalidPath;
use lore_base::types::Address;
use lore_base::types::Fragment;
use lore_error_set::prelude::*;

use super::filesystem_provider::DirectoryEntry;
use super::filesystem_provider::DirectoryListing;
use super::filesystem_provider::FileInfo;
use super::filesystem_provider::FilesystemDiffContext;
use super::filesystem_provider::FilesystemProvider;
use super::filesystem_provider::FsError;
use super::filesystem_provider::InstanceOperation;
use super::filesystem_provider::InstanceOperationImpl;
use super::filesystem_provider::StaticDispatchDirectoryListing;
use super::filesystem_provider::StaticDispatchInstanceOperation;
use crate::hash::hash_string;
use crate::immutable;
use crate::lore_debug;
use crate::merge::MergeTextMode;
use crate::merge::merge3_text_by_path;
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
        )))
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
                        info: FileInfo::from_metadata(&item.metadata),
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
    /// `NotFound`, so both report a non-existent path.
    async fn file_info(&self, path: &RelativePath) -> Result<FileInfo, FsError> {
        let path = self.absolute(path);
        match lore_io::IoDriver::global().metadata(path).await {
            Ok(metadata) => Ok(FileInfo::from_metadata(&metadata)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileInfo::NotExist),
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

    async fn holds_name_exactly(&self, path: &RelativePath) -> Option<bool> {
        lore_io::IoDriver::global()
            .holds_name_exactly(self.absolute(path))
            .await
    }

    async fn names_folding_to(
        &self,
        path: &RelativePath,
        name: &str,
    ) -> Result<Vec<String>, FsError> {
        let path = self.absolute(path);
        Ok(names_folding_to(path, name).await?)
    }

    async fn read_directory(&self, path: &RelativePath) -> Result<DirectoryListing, FsError> {
        let path = self.absolute(path);
        Ok(DirectoryListing::new(StaticDispatchDirectoryListing::Os(
            OsDirectoryListing {
                entries: lore_io::IoDriver::global().read_dir(path).await?,
            },
        )))
    }

    fn content_source(&self, path: &RelativePath) -> lore_storage::ContentSource<'static> {
        lore_storage::ContentSource::owned_file(self.absolute(path))
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
        let source = lore_storage::ContentSource::owned_file(self.absolute(path));
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
        unify_name_case_rename(&from, &to).await?;
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
        Ok(crate::infer::infer_is_diffable(&self.content_source(path))
            .await
            .unwrap_or(false))
    }
}

/// Represents a single filesystem item.
/// Used for directory children enumeration and single file metadata.
pub struct FileListItem {
    /// The name of the file/directory (not the full path).
    pub name: String,
    /// Filesystem metadata (size, timestamps, permissions, etc.).
    pub metadata: std::fs::Metadata,
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
    if !metadata.is_file() && !metadata.is_dir() {
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

    let Ok(metadata) = driver.metadata(path.as_path()).await else {
        return Ok(PathListingResult::NotFound);
    };

    if metadata.is_file() {
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

/// Every spelling of `name` the directory `path` holds that folds to the same name, the
/// exact one alone if it is there, and empty if no spelling is, which is the OS arm of
/// [`InstanceOperation::names_folding_to`].
///
/// Reads the directory and compares every child, which is what answering for
/// variations other than the one asked about takes. A caller that only needs to
/// know whether the case it has is the one on disk should ask
/// [`InstanceOperation::holds_name_exactly`] instead.
pub async fn names_folding_to(
    path: impl AsRef<Path>,
    name: &str,
) -> tokio::io::Result<Vec<String>> {
    let path = path.as_ref();

    let mut matches = vec![];
    let match_name = name.to_lowercase();
    let mut listing = lore_io::IoDriver::global().read_dir(path).await?;
    while let Some(entry) = listing.next().await {
        let entry_name = entry_name(entry?.file_name).map_err(tokio::io::Error::other)?;
        if entry_name == name {
            // Exact match
            return Ok(vec![entry_name]);
        }
        let entry_lowercase_name = entry_name.to_lowercase();
        if entry_lowercase_name == match_name {
            matches.push(entry_name);
        }
    }

    if !matches.is_empty() {
        if matches.len() == 1 {
            lore_debug!(
                "Found case variations for file {name} in path {}: {}",
                path.display(),
                matches[0]
            );
        } else {
            let mut message = format!(
                "Found case variations for file {name} in path {}:",
                path.display()
            );
            for entry in matches.iter() {
                message.push_str(format!("\n  {entry}").as_str());
            }
            lore_debug!("{message}");
        }
        return Ok(matches);
    }

    lore_debug!(
        "Found NO case variation for file {name} in path {}",
        path.display()
    );
    Ok(vec![])
}

/// Helper function to rename files during name case unification handling. Will try to rename
/// the "from" file/directory to "to" name. If the "to" name already exist in the file system
/// it will try to handle it as follows:
/// - if the "from"/"to" is a file it will overwrite the "to" file with the "from" file, then remove
///   the "from" file
/// - if the "from"/"to" is a directory it will recurse and call `unify_name_case_rename` on each
///   child item in the "from" directory to move it to the "to" directory, applying the same
///   rules to each subitem (replacing files, recursing directories).
pub fn unify_name_case_rename<'a>(
    from_path: &'a Path,
    to_path: &'a Path,
) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + 'a>> {
    Box::pin(async move {
        let driver = lore_io::IoDriver::global();
        lore_debug!(
            "Try rename {} -> {}",
            from_path.display(),
            to_path.display()
        );
        if driver.rename(from_path, to_path).await.is_ok() {
            lore_debug!("Renamed {} -> {}", from_path.display(), to_path.display());
            return Ok(());
        }

        let from_metadata = driver.metadata(from_path).await?;
        let to_metadata = driver.metadata(to_path).await?;

        if from_metadata.is_dir() != to_metadata.is_dir() {
            return Err(tokio::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Unable to rename, file/directory mismatch",
            ));
        }

        if from_metadata.is_file() {
            lore_debug!(
                "Failed rename {} -> {}, replacing",
                from_path.display(),
                to_path.display()
            );
            driver.remove_file(to_path).await?;
            if let Err(err) = driver.rename(from_path, to_path).await {
                lore_debug!(
                    "Failed rename {} -> {}, try copy and delete: {err}",
                    from_path.display(),
                    to_path.display(),
                );
                driver.copy(from_path, to_path).await?;
                driver.remove_file(from_path).await?;
            }
        } else {
            lore_debug!(
                "Failed rename {} -> {}, try recursive directory unification",
                from_path.display(),
                to_path.display()
            );
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
                unify_name_case_rename(&from_path, &to_path).await?;
            }
            driver.remove_dir_all(from_path).await?;
        }

        lore_debug!("Renamed {} -> {}", from_path.display(), to_path.display());
        Ok(())
    })
}

#[cfg(test)]
// Fixtures build filesystem state directly; what these test is how the primitives read it.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn temp_dir() -> lore_base::test_util::TempDir {
        lore_base::test_util::TempDir::new("lore-fs-os-test-")
    }

    /// An operation rooted at `root`, which is what a caller names its paths against.
    async fn os_operation(root: &Path) -> Arc<InstanceOperationImpl> {
        FilesystemProvider::begin_operation(&OsFilesystem::new(root))
            .await
            .expect("beginning an operation over the OS filesystem")
    }

    fn relative(path: &str) -> RelativePath {
        RelativePath::new_from_initial_path(path).expect("relative path")
    }

    /// Whether the filesystem under the temporary directory holds one case variation of a name
    /// and answers lookups in any other. Windows and macOS do by default and Linux does not, but a
    /// mount can be either on any of them, so the tests below ask rather than assume — and the
    /// two behaviours are different enough that a test written for one is not a test of the
    /// other.
    fn case_insensitive(dir: &Path) -> bool {
        let probe = dir.join("CaseProbe");
        std::fs::write(&probe, b"").expect("write probe");
        let insensitive = std::fs::metadata(dir.join("caseprobe")).is_ok();
        std::fs::remove_file(&probe).expect("remove probe");
        insensitive
    }

    /// What the lookup answers where the platform has one, and `None` where it
    /// has not — macOS can say nothing about a case variation short of the
    /// directory read this exists to avoid, so every expectation collapses to that
    /// there.
    fn verdict(held: bool) -> Option<bool> {
        cfg!(any(target_os = "linux", target_family = "windows")).then_some(held)
    }

    #[tokio::test]
    async fn list_path_yields_a_directory_listing() {
        let dir = temp_dir();
        std::fs::write(dir.path().join("child"), b"data").expect("write child");

        let PathListingResult::Directory { mut listing } =
            list_path(dir.path().to_path_buf()).await.expect("listing")
        else {
            panic!("a directory must list");
        };
        let mut names = Vec::new();
        while let Some(entry) = listing.next().await {
            if let Some(item) = file_list_item(entry).expect("entry name") {
                names.push(item.name);
            }
        }
        assert_eq!(names, vec!["child".to_string()]);
    }

    /// A link is not a kind the repository tracks, and the listing describes what a name holds
    /// rather than what it points at, so one naming a file is skipped as the link it is.
    #[cfg(target_family = "unix")]
    #[tokio::test]
    async fn a_listing_skips_a_link_to_a_file_it_would_otherwise_track() {
        let dir = temp_dir();
        std::fs::write(dir.path().join("target"), b"data").expect("write target");
        std::os::unix::fs::symlink(dir.path().join("target"), dir.path().join("link"))
            .expect("link the target");

        let PathListingResult::Directory { mut listing } =
            list_path(dir.path().to_path_buf()).await.expect("listing")
        else {
            panic!("a directory must list");
        };
        let mut names = Vec::new();
        while let Some(entry) = listing.next().await {
            if let Some(item) = file_list_item(entry).expect("entry name") {
                names.push(item.name);
            }
        }
        assert_eq!(names, vec!["target".to_string()]);
    }

    /// A name a listing can yield that has no text spelling: bytes that are not UTF-8 on unix, an
    /// unpaired surrogate on Windows.
    #[cfg(target_family = "unix")]
    fn name_that_is_not_text() -> std::ffi::OsString {
        use std::os::unix::ffi::OsStringExt as _;

        std::ffi::OsString::from_vec(vec![0xff])
    }

    #[cfg(target_family = "windows")]
    fn name_that_is_not_text() -> std::ffi::OsString {
        use std::os::windows::ffi::OsStringExt as _;

        std::ffi::OsString::from_wide(&[0xd800])
    }

    /// A name that is not text is reported, and named lossily in the report, rather than passed
    /// over: it hashes to a node the tree does not hold, so nothing can be done with it that is
    /// not a guess.
    ///
    /// The entry is assembled rather than read off a disk so this runs wherever the crate builds.
    /// A filesystem enforcing UTF-8 -- ZFS with `utf8only=on`, APFS -- refuses to create such a
    /// name at all, which leaves
    /// [`crate::fs::filesystem_provider::tests::a_name_that_is_not_text_is_reported`], the
    /// end-to-end cover over a real listing, with nothing to list there.
    #[test]
    fn a_listed_name_that_is_not_text_is_reported() {
        let dir = temp_dir();
        let path = dir.path().join("named");
        std::fs::write(&path, b"data").expect("write file");
        // A real file's metadata: an entry carrying none, or naming neither a file nor a
        // directory, is passed over before its name is read.
        let metadata = std::fs::metadata(&path).expect("read metadata");

        let listed = file_list_item(Ok(lore_io::DirEntry {
            file_name: name_that_is_not_text(),
            metadata: Some(metadata),
            is_symlink: false,
        }));

        let Err(error) = listed else {
            panic!("a name with no text spelling must be reported, not listed");
        };
        assert!(
            error.to_string().contains('\u{fffd}'),
            "the report names the entry lossily, not by a spelling it does not have: {error}"
        );
    }

    #[tokio::test]
    async fn list_path_describes_a_file_by_its_own_name() {
        let dir = temp_dir();
        let path = dir.path().join("lonely.txt");
        std::fs::write(&path, b"data").expect("write file");

        let PathListingResult::File { item } = list_path(path).await.expect("listing") else {
            panic!("a file must be described, not listed");
        };
        assert_eq!(item.name, "lonely.txt");
        assert_eq!(item.metadata.len(), 4);
    }

    #[tokio::test]
    async fn list_path_reports_a_missing_path() {
        let dir = temp_dir();
        assert!(
            list_path(dir.path().join("absent"))
                .await
                .expect("listing")
                .is_not_found(),
            "a missing path is neither a file nor a directory"
        );
    }

    /// The distinction the whole thing rests on: this answers for the case
    /// variation asked about, where `Path::exists` answers for the file whatever
    /// it is called. A case-insensitive filesystem finds the file under either name,
    /// and must still say no to the one it does not hold.
    #[tokio::test]
    async fn a_name_is_held_only_in_the_case_variation_on_disk() {
        let dir = temp_dir();
        let operation = os_operation(dir.path()).await;
        std::fs::write(dir.path().join("Test.file"), b"").expect("write file");

        assert_eq!(
            operation.holds_name_exactly(&relative("Test.file")).await,
            verdict(true)
        );
        assert_eq!(
            operation.holds_name_exactly(&relative("test.FILE")).await,
            verdict(false),
            "a case variation the filesystem does not hold is not a match, whether or not it would find the file"
        );
        assert_eq!(
            operation.holds_name_exactly(&relative("other.file")).await,
            verdict(false)
        );
        assert_eq!(
            operation.holds_name_exactly(&relative("Test.file.")).await,
            verdict(false),
            "a trailing dot names a file that is not there"
        );
        assert_eq!(
            operation
                .holds_name_exactly(&relative("absent/Test.file"))
                .await,
            verdict(false),
            "a missing directory holds nothing"
        );
    }

    /// A directory is what most components resolve to, and the lookup has to
    /// answer for one as readily as for a file.
    #[tokio::test]
    async fn a_directory_name_is_held() {
        let dir = temp_dir();
        let operation = os_operation(dir.path()).await;
        std::fs::create_dir(dir.path().join("Assets")).expect("create dir");

        assert_eq!(
            operation.holds_name_exactly(&relative("Assets")).await,
            verdict(true)
        );
    }

    #[tokio::test]
    async fn names_answers_with_the_case_variation_it_was_given() {
        let dir = temp_dir();
        std::fs::write(dir.path().join("Test.file"), b"").expect("write file");

        assert_eq!(
            names_folding_to(dir.path(), "Test.file")
                .await
                .expect("a name the filesystem holds must resolve"),
            vec!["Test.file".to_string()]
        );
    }

    /// The reason the helper exists: a caller holding a name in one case needs the one the
    /// filesystem kept. The directory is read and the names compared here rather than looked up,
    /// so the case variation on disk is reported whether or not the filesystem would itself
    /// have found the file under the one asked about.
    #[tokio::test]
    async fn names_answers_with_the_stored_case_variation() {
        let dir = temp_dir();
        std::fs::write(dir.path().join("Test.file"), b"").expect("write file");

        assert_eq!(
            names_folding_to(dir.path(), "test.FILE")
                .await
                .expect("a case variation must resolve"),
            vec!["Test.file".to_string()]
        );
    }

    #[tokio::test]
    async fn names_reports_a_name_that_is_not_there_in_any_case() {
        let dir = temp_dir();
        std::fs::write(dir.path().join("Test.file"), b"").expect("write file");

        assert!(
            names_folding_to(dir.path(), "other.file")
                .await
                .expect("reading the directory must succeed")
                .is_empty(),
            "a name no case variation of which is there must not resolve"
        );
    }

    /// Win32 trims trailing dots and spaces from a path before it looks it up, so asking about a
    /// name with one can be answered about the neighbouring name. That is a different file, and
    /// must not be reported as a case variation of the name asked about.
    #[tokio::test]
    async fn names_does_not_answer_with_a_neighbouring_name() {
        let dir = temp_dir();
        std::fs::write(dir.path().join("Test.file"), b"").expect("write file");

        assert!(
            names_folding_to(dir.path(), "Test.file.")
                .await
                .expect("reading the directory must succeed")
                .is_empty(),
            "a trailing dot names a file that is not there"
        );
    }

    /// Where variations can coexist, every one of them comes back — resolving that ambiguity is
    /// the caller's to do — while an exact match answers for itself alone. Only a case-sensitive
    /// filesystem can hold the two files this needs.
    #[tokio::test]
    async fn names_reports_every_case_variation_that_coexists() {
        let dir = temp_dir();
        if case_insensitive(dir.path()) {
            return;
        }
        std::fs::write(dir.path().join("Test.file"), b"").expect("write Test.file");
        std::fs::write(dir.path().join("test.file"), b"").expect("write test.file");

        let mut found = names_folding_to(dir.path(), "TEST.FILE")
            .await
            .expect("the variations must resolve");
        found.sort();
        assert_eq!(
            found,
            vec!["Test.file".to_string(), "test.file".to_string()],
            "an ambiguous name must report every case variation, not pick one"
        );

        assert_eq!(
            names_folding_to(dir.path(), "test.file")
                .await
                .expect("an exact name must resolve"),
            vec!["test.file".to_string()],
            "a case variation the filesystem holds answers for itself, ambiguity or not"
        );
    }
}
