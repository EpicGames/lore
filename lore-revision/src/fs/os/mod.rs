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
use bytes::Bytes;
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

    async fn write_file(&self, path: &RelativePath, contents: Bytes) -> Result<(), FsError> {
        let path = self.absolute(path);
        lore_io::IoDriver::global()
            .write_file_bytes(path, contents, false)
            .await?;
        Ok(())
    }

    async fn rename(&self, from: &RelativePath, to: &RelativePath) -> Result<(), FsError> {
        let (from, to) = (self.absolute(from), self.absolute(to));
        rename_unifying(&from, &to).await?;
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

        write_node_content(repository, node, &path).await?;

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
        let path = self.absolute(path);
        let (fragment, metadata) = write_node_content(repository, node, &path).await?;
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

/// Puts the content `node` addresses at `path`, with the fragment it came from and what the write
/// left there where the write reported it.
///
/// A node addressing nothing -- the zero hash every empty file carries -- leaves an empty file:
/// the store answers for it without being read, there being no content to find.
async fn write_node_content(
    repository: Arc<RepositoryContext>,
    node: &Node,
    path: &Path,
) -> Result<(Fragment, Option<std::fs::Metadata>), FsError> {
    let options = immutable::read_options_from_repository(&repository);
    immutable::read_into_file(repository, node.address, path, None, options)
        .await
        .forward_any::<FsError>("Failed to read file")
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
/// `to_path`, by rename where one lands and by hand where none does.
///
/// A file goes to `to_path` whether or not a file is there, one that is there being removed first.
/// A directory hands each child over under these same rules, a child whose name the destination
/// also holds recursing again, before it goes; a destination that is not there is created to take
/// them. A `from_path` and `to_path` of different kinds are refused, there being no move that
/// leaves one of them.
///
/// Boxed because it recurses.
fn rename_unifying<'a>(
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
        let to_metadata = match driver.metadata(to_path).await {
            Ok(metadata) => Some(metadata),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err),
        };

        if let Some(to_metadata) = &to_metadata
            && from_metadata.is_dir() != to_metadata.is_dir()
        {
            return Err(tokio::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Unable to rename, file/directory mismatch",
            ));
        }

        if from_metadata.is_file() {
            if to_metadata.is_some() {
                driver.remove_file(to_path).await?;
                if driver.rename(from_path, to_path).await.is_ok() {
                    return Ok(());
                }
            }
            copy_and_unlink(from_path, to_path).await?;
        } else {
            if to_metadata.is_none() {
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

    /// An empty directory of its own on a filesystem other than the one holding `beside`, or
    /// `None` where the machine offers no second one. `/dev/shm` is the tmpfs a Linux system
    /// mounts apart from the one temporary directories come from.
    ///
    /// Named per call, the tests sharing a process and running at the same time, and per process,
    /// a previous run having left one behind where it failed. The caller removes it: it lies
    /// outside the temporary directory that would have.
    #[cfg(target_os = "linux")]
    fn second_filesystem_directory(beside: &Path) -> Option<PathBuf> {
        use std::os::unix::fs::MetadataExt;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;

        static TAKEN: AtomicUsize = AtomicUsize::new(0);

        let shm = Path::new("/dev/shm");
        if std::fs::metadata(shm).ok()?.dev() == std::fs::metadata(beside).ok()?.dev() {
            return None;
        }
        let directory = shm.join(format!(
            "lore-fs-os-test-{}-{}",
            std::process::id(),
            TAKEN.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir(&directory).ok()?;
        Some(directory)
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
        let operation = os_operation(dir.path()).await;

        assert_eq!(
            operation
                .names_folding_to(&relative(""), "Test.file")
                .await
                .expect("a name the filesystem holds must resolve"),
            vec!["Test.file".to_string()]
        );
    }

    /// The reason the member exists: a caller holding a name in one case needs the one the
    /// filesystem kept. The directory is read and the names folded rather than looked up, so the
    /// case variation on disk is reported whether or not the filesystem would itself have found
    /// the file under the one asked about.
    #[tokio::test]
    async fn names_answers_with_the_stored_case_variation() {
        let dir = temp_dir();
        std::fs::write(dir.path().join("Test.file"), b"").expect("write file");
        let operation = os_operation(dir.path()).await;

        assert_eq!(
            operation
                .names_folding_to(&relative(""), "test.FILE")
                .await
                .expect("a case variation must resolve"),
            vec!["Test.file".to_string()]
        );
    }

    #[tokio::test]
    async fn names_reports_a_name_that_is_not_there_in_any_case() {
        let dir = temp_dir();
        std::fs::write(dir.path().join("Test.file"), b"").expect("write file");
        let operation = os_operation(dir.path()).await;

        assert!(
            operation
                .names_folding_to(&relative(""), "other.file")
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
        let operation = os_operation(dir.path()).await;

        assert!(
            operation
                .names_folding_to(&relative(""), "Test.file.")
                .await
                .expect("reading the directory must succeed")
                .is_empty(),
            "a trailing dot names a file that is not there"
        );
    }

    /// A link is not a spelling the repository holds, so it is not reported as one even where its
    /// name is the only thing that folds to the one asked about.
    #[tokio::test]
    #[cfg(target_family = "unix")]
    async fn names_leaves_out_a_link() {
        let dir = temp_dir();
        std::fs::write(dir.path().join("target"), b"").expect("write target");
        std::os::unix::fs::symlink(dir.path().join("target"), dir.path().join("Test.file"))
            .expect("create link");
        let operation = os_operation(dir.path()).await;

        assert!(
            operation
                .names_folding_to(&relative(""), "test.file")
                .await
                .expect("reading the directory must succeed")
                .is_empty(),
            "a link must not be reported as a case variation"
        );
    }

    /// Every variation comes back, the exact one among them: resolving the ambiguity is the
    /// caller's to do, and one that has to tell a collision from a resolution needs to see both.
    /// Only a case-sensitive filesystem can hold the two files this needs.
    #[tokio::test]
    async fn names_reports_every_case_variation_that_coexists() {
        let dir = temp_dir();
        if case_insensitive(dir.path()) {
            return;
        }
        std::fs::write(dir.path().join("Test.file"), b"").expect("write Test.file");
        std::fs::write(dir.path().join("test.file"), b"").expect("write test.file");
        let operation = os_operation(dir.path()).await;

        for asked in ["TEST.FILE", "test.file"] {
            let mut found = operation
                .names_folding_to(&relative(""), asked)
                .await
                .expect("the variations must resolve");
            found.sort();
            assert_eq!(
                found,
                vec!["Test.file".to_string(), "test.file".to_string()],
                "asking about {asked} must report every case variation, not pick one"
            );
        }
    }

    /// A file lands on the name even where the name is taken, replacing what was there.
    #[tokio::test]
    async fn rename_replaces_the_file_at_the_destination() {
        let dir = temp_dir();
        std::fs::write(dir.path().join("from"), b"carried").expect("write source");
        std::fs::write(dir.path().join("to"), b"replaced").expect("write destination");
        let operation = os_operation(dir.path()).await;

        operation
            .rename(&relative("from"), &relative("to"))
            .await
            .expect("the move must land");

        assert_eq!(
            b"carried".to_vec(),
            std::fs::read(dir.path().join("to")).expect("read destination")
        );
        assert!(!dir.path().join("from").exists(), "the source must be gone");
    }

    /// Text is diffable and an opaque format is not, read through the content the operation names
    /// at the path.
    #[tokio::test]
    async fn infer_is_diffable_reads_the_content_at_the_path() {
        let dir = temp_dir();
        std::fs::write(dir.path().join("text"), b"one\ntwo\n").expect("write text");
        std::fs::write(dir.path().join("opaque"), b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR")
            .expect("write opaque");
        let operation = os_operation(dir.path()).await;

        assert!(
            operation
                .infer_is_diffable(&relative("text"))
                .await
                .expect("reading the content must succeed")
        );
        assert!(
            !operation
                .infer_is_diffable(&relative("opaque"))
                .await
                .expect("reading the content must succeed")
        );
    }

    /// Content that is not there is not diffable, there being nothing to diff.
    #[tokio::test]
    async fn infer_is_diffable_reports_content_that_is_not_there() {
        let dir = temp_dir();
        let operation = os_operation(dir.path()).await;

        assert!(
            !operation
                .infer_is_diffable(&relative("absent"))
                .await
                .expect("an unreadable path is an answer, not a failure")
        );
    }

    /// A move between two filesystems is no rename, so the content is copied and the source
    /// unlinked, whether or not the destination is already taken. The source is reached through a
    /// link, the two paths an operation names lying under one root.
    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn rename_carries_a_file_across_filesystems() {
        for occupied in [false, true] {
            let dir = temp_dir();
            let Some(elsewhere) = second_filesystem_directory(dir.path()) else {
                return;
            };
            std::os::unix::fs::symlink(&elsewhere, dir.path().join("elsewhere")).expect("link");
            std::fs::write(elsewhere.join("from"), b"carried").expect("write source");
            if occupied {
                std::fs::write(dir.path().join("to"), b"replaced").expect("write destination");
            }
            let operation = os_operation(dir.path()).await;

            let moved = operation
                .rename(&relative("elsewhere/from"), &relative("to"))
                .await;

            let landed = std::fs::read(dir.path().join("to")).ok();
            let source_left = elsewhere.join("from").exists();
            let _ = std::fs::remove_dir_all(&elsewhere);

            moved.expect("the move must land");
            assert_eq!(
                Some(b"carried".to_vec()),
                landed,
                "the destination must hold the source, occupied {occupied}"
            );
            assert!(!source_left, "the source must be gone, occupied {occupied}");
        }
    }

    /// A directory crosses a filesystem the same way, its children carried one at a time into a
    /// destination created to take them.
    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn rename_carries_a_directory_across_filesystems() {
        let dir = temp_dir();
        let Some(elsewhere) = second_filesystem_directory(dir.path()) else {
            return;
        };
        std::os::unix::fs::symlink(&elsewhere, dir.path().join("elsewhere")).expect("link");
        std::fs::create_dir_all(elsewhere.join("from").join("nested")).expect("create source");
        std::fs::write(
            elsewhere.join("from").join("nested").join("leaf"),
            b"carried",
        )
        .expect("write nested child");
        let operation = os_operation(dir.path()).await;

        let moved = operation
            .rename(&relative("elsewhere/from"), &relative("to"))
            .await;

        let landed = std::fs::read(dir.path().join("to").join("nested").join("leaf")).ok();
        let source_left = elsewhere.join("from").exists();
        let _ = std::fs::remove_dir_all(&elsewhere);

        moved.expect("the move must land");
        assert_eq!(Some(b"carried".to_vec()), landed);
        assert!(!source_left, "the source must be gone");
    }

    /// A directory whose name is taken hands its children over one at a time, leaving what the
    /// destination already held beside them, and a child whose name is taken too is merged under
    /// the same rules rather than refused.
    #[tokio::test]
    async fn rename_merges_a_directory_into_the_one_at_the_destination() {
        let dir = temp_dir();
        let from = dir.path().join("from");
        let to = dir.path().join("to");
        std::fs::create_dir_all(from.join("shared")).expect("create source");
        std::fs::create_dir_all(to.join("shared")).expect("create destination");
        std::fs::write(from.join("carried"), b"").expect("write child");
        std::fs::write(to.join("held"), b"").expect("write held child");
        std::fs::write(from.join("shared").join("nested"), b"").expect("write nested child");
        std::fs::write(to.join("shared").join("kept"), b"").expect("write nested held child");
        let operation = os_operation(dir.path()).await;

        operation
            .rename(&relative("from"), &relative("to"))
            .await
            .expect("the merge must land");

        assert!(to.join("carried").exists());
        assert!(to.join("held").exists());
        assert!(to.join("shared").join("nested").exists());
        assert!(to.join("shared").join("kept").exists());
        assert!(!from.exists(), "the source must be gone");
    }

    /// A file and a directory are not two spellings of one thing, so there is no move that leaves
    /// one of them.
    #[tokio::test]
    async fn rename_refuses_a_destination_of_another_kind() {
        let dir = temp_dir();
        std::fs::write(dir.path().join("from"), b"").expect("write source");
        std::fs::create_dir(dir.path().join("to")).expect("create destination");
        let operation = os_operation(dir.path()).await;

        operation
            .rename(&relative("from"), &relative("to"))
            .await
            .expect_err("a file must not land on a directory");

        assert!(dir.path().join("from").is_file(), "the source must survive");
        assert!(
            dir.path().join("to").is_dir(),
            "the destination must survive"
        );
    }
}
