// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fs::Metadata;
use std::future::Future;
#[cfg(target_family = "unix")]
use std::os::unix::fs::MetadataExt;
#[cfg(target_family = "unix")]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(target_family = "unix")]
use std::os::unix::fs::PermissionsExt;
#[cfg(target_family = "windows")]
use std::os::windows::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;

use rand::distr::Alphanumeric;
use rand::distr::SampleString;

use super::path::RelativePath;
use super::path::RelativePathBuf;
use crate::hash::hash_string;
use crate::lore_debug;
use crate::lore_spawn_blocking;
use crate::lore_trace;
#[cfg(not(target_family = "windows"))]
use crate::lore_warn;
use crate::node::NodeFileMode;
use crate::repository::TEMP_FILE_EXTENSION;
use crate::util::time::Retry;
use crate::util::time::RetryPolicy;

#[cfg(not(target_family = "windows"))]
const FILE_MODE_USER_EXEC: u32 = 0o100;
#[cfg(not(target_family = "windows"))]
const FILE_MODE_ALL_EXEC: u32 = 0o111;

// On Windows we do not care about executable bit
#[cfg(target_family = "windows")]
pub async fn metadata_set_executable(
    _path: impl AsRef<Path>,
    _metadata: &Metadata,
    _executable: bool,
) {
}

#[cfg(not(target_family = "windows"))]
#[allow(unused_variables)]
pub async fn metadata_set_executable(
    path: impl AsRef<Path>,
    metadata: &Metadata,
    executable: bool,
) {
    let path = path.as_ref();
    let mut permissions = metadata.permissions();

    let mode = if executable {
        permissions.mode() | FILE_MODE_ALL_EXEC
    } else {
        permissions.mode() & !FILE_MODE_ALL_EXEC
    };
    permissions.set_mode(mode);

    let _ = tokio::fs::set_permissions(path, permissions)
        .await
        .map_err(|err| {
            lore_warn!(
                "Failed to set executable mode {} for {}: {err}",
                mode,
                path.to_path_buf().display()
            );
        });
}

#[cfg(target_family = "windows")]
pub fn metadata_to_mode(metadata: &Metadata, previous: u16) -> u16 {
    // On Windows we just preserve the previous mode for files
    if metadata.is_file() {
        previous & NodeFileMode::Executable.bits()
    } else {
        0
    }
}

#[cfg(target_family = "unix")]
pub fn metadata_to_mode(metadata: &Metadata, _previous: u16) -> u16 {
    if metadata.is_file() && ((metadata.permissions().mode() & FILE_MODE_USER_EXEC) != 0) {
        NodeFileMode::Executable.bits()
    } else {
        0
    }
}

pub fn mode_changed(from: u16, to: u16) -> bool {
    // Only care about the executable bit
    (from & NodeFileMode::Executable.bits()) != (to & NodeFileMode::Executable.bits())
}

pub fn file_mtime(metadata: &Metadata) -> u64 {
    metadata
        .modified()
        .unwrap_or(std::time::SystemTime::now())
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn file_size(metadata: &Metadata) -> u64 {
    #[cfg(target_family = "windows")]
    let size = metadata.file_size();
    #[cfg(target_family = "unix")]
    let size = metadata.size();

    size
}

pub fn file_mtime_and_size(metadata: &Metadata) -> (u64, u64) {
    (file_mtime(metadata), file_size(metadata))
}

#[cfg(target_family = "windows")]
pub fn file_is_executable(_metadata: &Metadata) -> bool {
    false
}

#[cfg(target_family = "unix")]
pub fn file_is_executable(metadata: &Metadata) -> bool {
    (metadata.permissions().mode() & FILE_MODE_USER_EXEC) != 0
}

/// Check if an entry with the given exact name exists in the directory.
/// Unlike `Path::exists()`, this performs a case-sensitive check on all platforms
/// by reading the directory entries and comparing names exactly.
pub fn filesystem_name_exists(parent: &Path, name: &str) -> bool {
    let Ok(reader) = std::fs::read_dir(parent) else {
        return false;
    };
    for entry in reader.flatten() {
        if entry.file_name() == name {
            return true;
        }
    }
    false
}

// TODO(mjansson): We could pass around a hashmap cache of directory to file list mappings
// while executing an operation, to reduce the number of iterations on the file system to
// find files and their existing names - used by filesystem_name, filesystem_path and list_files
pub async fn filesystem_names(
    path: impl AsRef<Path>,
    name: &str,
) -> tokio::io::Result<Vec<String>> {
    let path = path.as_ref();

    // TODO(mjansson): This should be a test for file system case sensitivity, in the sense that the file system
    //                 support multiple concurrent case variations of the same file name
    #[cfg(target_os = "linux")]
    {
        let initial_path = path.join(name);
        if tokio::fs::metadata(initial_path.as_path()).await.is_ok() {
            return Ok(vec![name.to_string()]);
        }
    }

    let mut matches = vec![];
    let match_name = name.to_lowercase();
    let mut reader = tokio::fs::read_dir(path).await?;
    while let Some(entry) = reader.next_entry().await? {
        let entry_file_name = entry.file_name();
        let entry_name = entry_file_name.to_string_lossy();
        if entry_name == name {
            // Exact match
            return Ok(vec![entry_name.to_string()]);
        }
        let entry_lowercase_name = entry_name.to_lowercase();
        if entry_lowercase_name == match_name {
            matches.push(entry_name.to_string());
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
    Err(tokio::io::Error::new(
        tokio::io::ErrorKind::NotFound,
        "Matching file not found",
    ))
}

pub async fn filesystem_path(
    base_path: impl AsRef<Path>,
    find_path: &RelativePath,
) -> tokio::io::Result<String> {
    let base_path = base_path.as_ref();

    // TODO(mjansson): This should be a test for file system case sensitivity, in the sense that the file system
    //                 support multiple concurrent case variations of the same file name
    #[cfg(target_os = "linux")]
    {
        let initial_path = base_path.join(find_path.as_str());
        if tokio::fs::metadata(initial_path.as_path()).await.is_ok() {
            return Ok(find_path.as_str().to_string());
        }
    }

    let mut full_path = base_path.to_path_buf();
    let mut remain_path = find_path.clone();
    let mut found_path = RelativePathBuf::new();
    while !remain_path.is_empty() {
        let name = remain_path.pop_root();
        let fs_names = filesystem_names(full_path.as_path(), name).await?;
        if fs_names.len() > 1 {
            if remain_path.is_empty() {
                lore_debug!("Found ambiguous path case variations for {find_path}");
                return Err(tokio::io::Error::other(
                    "Ambiguous case variations for path {find_path}",
                ));
            }

            // Find the match in either or many of the potential variations
            let mut found_variation = false;
            for entry in fs_names.iter() {
                let next_full_path = full_path.join(entry);

                lore_debug!(
                    "Fork case variation check for {remain_path} in {}",
                    next_full_path.display()
                );
                if let Ok(sub_path) =
                    filesystem_path_fork(next_full_path.as_path(), &remain_path).await
                {
                    if found_variation {
                        lore_debug!("Found ambiguous path case variations for {find_path}");
                        return Err(tokio::io::Error::other(
                            "Ambiguous case variations found for path {find_path}",
                        ));
                    }

                    full_path.push(entry);
                    full_path.push(sub_path.as_str());

                    found_path.push(entry);
                    found_path.push(sub_path.as_str());

                    lore_debug!(
                        "Fork found case variation {sub_path} for {remain_path} in {}",
                        next_full_path.display()
                    );
                    found_variation = true;
                } else {
                    lore_debug!(
                        "Fork found NO case variation for {remain_path} in {}",
                        next_full_path.display()
                    );
                }
            }

            if !found_variation {
                return Err(tokio::io::Error::new(
                    tokio::io::ErrorKind::NotFound,
                    "Matching file not found",
                ));
            }

            break;
        }

        full_path.push(fs_names[0].as_str());
        found_path.push(fs_names[0].as_str());
    }

    lore_debug!(
        "Found full path case variation {} for path {} in path {}",
        found_path.as_str(),
        find_path.as_str(),
        base_path.display()
    );
    Ok(found_path.as_str().to_string())
}

pub fn filesystem_path_fork(
    base_path: impl AsRef<Path>,
    find_path: &RelativePath,
) -> Pin<Box<dyn Future<Output = tokio::io::Result<String>> + Send>> {
    let base_path = base_path.as_ref().to_path_buf();
    let find_path = find_path.clone();
    Box::pin(async move { filesystem_path(base_path, &find_path).await })
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
    /// The receiver yields `FileListItem` for each child in the directory.
    /// Each item's `name` is relative to the directory (just the filename,
    /// not the full path).
    Directory {
        receiver: tokio::sync::mpsc::UnboundedReceiver<FileListItem>,
    },

    /// The path was a regular file.
    ///
    /// The `item.name` is the filename component of the path that was queried.
    /// For example, querying `/foo/bar/file.txt` yields `item.name = "file.txt"`.
    File { item: FileListItem },

    /// The path did not exist, was not accessible, or was a special file type
    /// (symlink, device, etc.) that we don't handle.
    NotFound,
}

impl PathListingResult {
    /// Returns true if the path was a directory.
    pub fn is_directory(&self) -> bool {
        matches!(self, Self::Directory { .. })
    }

    /// Returns true if the path was a file.
    pub fn is_file(&self) -> bool {
        matches!(self, Self::File { .. })
    }

    /// Returns true if the path was not found or not accessible.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound)
    }
}

/// Resolve metadata for a directory entry, following symlinks.
///
/// `DirEntry::metadata()` on Linux returns the symlink's own metadata
/// rather than the target's. When the entry is a symlink, this falls
/// back to `std::fs::metadata()` which follows the link and returns
/// the target's metadata. Broken symlinks (dead target) return `None`.
fn resolve_entry_metadata(entry: &std::fs::DirEntry) -> Option<std::fs::Metadata> {
    let metadata = entry.metadata().ok()?;
    if metadata.is_symlink() {
        std::fs::metadata(entry.path()).ok()
    } else {
        Some(metadata)
    }
}

/// Lists a filesystem path, automatically handling both file and directory cases.
///
/// # Arguments
/// * `path` - The filesystem path to list
///
/// # Returns
/// * `PathListingResult::Directory` - If path is a directory, with channel for children
/// * `PathListingResult::File` - If path is a single file, with its metadata
/// * `PathListingResult::NotFound` - If path doesn't exist or isn't accessible
///
/// # Path Semantics
/// - For directories: Each item's `name` is the child filename (e.g., "file.txt")
/// - For files: The item's `name` is the filename component (e.g., "file.txt" for "/foo/file.txt")
pub fn list_path(path: PathBuf) -> PathListingResult {
    // Check what kind of path we have first (synchronous check)
    let metadata = match std::fs::metadata(path.as_path()) {
        Ok(m) => m,
        Err(_) => return PathListingResult::NotFound,
    };

    if metadata.is_dir() {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();

        lore_spawn_blocking!(move || {
            if let Ok(reader) = std::fs::read_dir(path.as_path()) {
                for entry in reader.flatten() {
                    let file_name = entry.file_name();
                    if let Some(entry_metadata) = resolve_entry_metadata(&entry) {
                        let name = file_name.to_string_lossy().to_string();
                        let name_hash = hash_string(name.as_str());
                        let _ = sender.send(FileListItem {
                            name,
                            metadata: entry_metadata,
                            name_hash,
                        });
                    }
                }
            }
        });

        PathListingResult::Directory { receiver }
    } else if metadata.is_file() {
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let name_hash = hash_string(file_name.as_str());

        PathListingResult::File {
            item: FileListItem {
                name: file_name,
                metadata,
                name_hash,
            },
        }
    } else {
        // Symlink or other special file type
        PathListingResult::NotFound
    }
}

/// Lists only directory children. Returns an error if path is not a directory.
/// This is the preferred function when you know you're working with a directory.
///
/// # Arguments
/// * `path` - The filesystem path to list (must be a directory)
///
/// # Returns
/// * `Ok(receiver)` - Channel that yields `FileListItem` for each child
/// * `Err(_)` - If path doesn't exist, isn't accessible, or isn't a directory
pub fn list_directory(
    path: PathBuf,
) -> std::io::Result<tokio::sync::mpsc::UnboundedReceiver<FileListItem>> {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    lore_spawn_blocking!(move || {
        if let Ok(reader) = std::fs::read_dir(path.as_path()) {
            for entry in reader.flatten() {
                let file_name = entry.file_name();
                if let Some(metadata) = resolve_entry_metadata(&entry) {
                    let name = file_name.to_string_lossy().to_string();
                    let name_hash = hash_string(name.as_str());
                    let _ = sender.send(FileListItem {
                        name,
                        metadata,
                        name_hash,
                    });
                }
            }
        }
    });
    Ok(receiver)
}

/// Helper function to rename files during name case unification handling. Will try to rename
/// the "from" file/directory to "to" name. If the "to" name already exist in the file system
/// it will try to handle it as follows:
/// - if the "from"/"to" is a file it will overwrite the "to" file with the "from" file, then remove
///   the "from" file
/// - if the "from"/"to" is a directory it will recurse and call `unify_name_case_rename` on each
///   child item in the "from" directory to move it to the "to" directory, applying the same
///   rules to each subitem (replacing files, recursing directories).
pub fn unify_name_case_rename(from_path: &Path, to_path: &Path) -> std::io::Result<()> {
    lore_debug!(
        "Try rename {} -> {}",
        from_path.display(),
        to_path.display()
    );
    let result = lore_storage::fs_util::rename_file(from_path, to_path);
    if result.is_ok() {
        lore_debug!("Renamed {} -> {}", from_path.display(), to_path.display());
        return Ok(());
    }

    let from_metadata = std::fs::metadata(from_path)?;
    let to_metadata = std::fs::metadata(to_path)?;

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
        #[allow(clippy::disallowed_methods)]
        // Authorized fs helper for case-insensitive rename fallback.
        std::fs::remove_file(to_path)?;
        if let Err(err) = lore_storage::fs_util::rename_file(from_path, to_path) {
            lore_debug!(
                "Failed rename {} -> {}, try copy and delete: {err}",
                from_path.display(),
                to_path.display(),
            );
            std::fs::copy(from_path, to_path)?;
            #[allow(clippy::disallowed_methods)]
            // Authorized fs helper for case-insensitive rename fallback.
            std::fs::remove_file(from_path)?;
        }
    } else {
        lore_debug!(
            "Failed rename {} -> {}, try recursive directory unification",
            from_path.display(),
            to_path.display()
        );
        // Make sure reader runs out of scope before removing directory
        {
            let reader = std::fs::read_dir(from_path)?;
            for entry in reader.flatten() {
                let entry_file_name = entry.file_name();
                let file_name = entry_file_name.to_string_lossy();
                let from_path = from_path.join(file_name.as_ref());
                let to_path = to_path.join(file_name.as_ref());
                unify_name_case_rename(&from_path, &to_path)?;
            }
        }
        #[allow(clippy::disallowed_methods)]
        // Authorized fs helper for case-insensitive rename fallback.
        std::fs::remove_dir_all(from_path)?;
    }

    lore_debug!("Renamed {} -> {}", from_path.display(), to_path.display());
    Ok(())
}

pub async fn unlink<P: AsRef<Path>>(absolute_path: P) -> tokio::io::Result<()> {
    let absolute_path = absolute_path.as_ref();
    lore_trace!("Deleting {}", absolute_path.display());
    let metadata = tokio::fs::metadata(absolute_path).await;

    if let Ok(metadata) = metadata {
        if metadata.is_dir() {
            if let Err(err) = tokio::fs::remove_dir(absolute_path).await {
                if err.kind() == tokio::io::ErrorKind::NotFound {
                    lore_trace!(
                        "Path does not exist anymore after removing recursively {}: {}",
                        absolute_path.display(),
                        err
                    );
                    return Ok(());
                }
                lore_debug!(
                    "Error deleting directory {}: {} - retry after setting write permission",
                    absolute_path.display(),
                    err
                );

                let mut permissions = metadata.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                permissions.set_readonly(false);
                let _ = tokio::fs::set_permissions(absolute_path, permissions).await;
                if let Err(err) = tokio::fs::remove_dir(absolute_path).await {
                    if err.kind() == tokio::io::ErrorKind::NotFound {
                        lore_trace!(
                            "Path does not exist anymore after trying remove recursively with write permissions: {}",
                            absolute_path.display()
                        );
                        return Ok(());
                    } else {
                        lore_debug!(
                            "Error deleting directory with write permissions {}: {}",
                            absolute_path.display(),
                            err
                        );
                    }
                    return Err(err);
                }
            }
        } else {
            if let Err(err) = tokio::fs::remove_file(absolute_path).await {
                if err.kind() == tokio::io::ErrorKind::NotFound {
                    lore_trace!(
                        "Path does not exist anymore after removing file with write permissions: {}",
                        absolute_path.display()
                    );
                    return Ok(());
                }
                lore_debug!(
                    "Error deleting file {}: {} - retry after setting write permission",
                    absolute_path.display(),
                    err
                );

                let mut permissions = metadata.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                permissions.set_readonly(false);
                let _ = tokio::fs::set_permissions(absolute_path, permissions).await;
                if let Err(err) = tokio::fs::remove_file(absolute_path).await {
                    if err.kind() == tokio::io::ErrorKind::NotFound {
                        lore_trace!(
                            "Path does not exist anymore after trying remove file with write permissions: {}",
                            absolute_path.display()
                        );
                        return Ok(());
                    } else {
                        lore_debug!(
                            "Error deleting file with write permissions {}: {}",
                            absolute_path.display(),
                            err
                        );
                    }
                    return Err(err);
                }
            }
            lore_trace!("Deleted file {}", absolute_path.display(),);
        }
    } else if let Some(err) = metadata.err() {
        if err.kind() == tokio::io::ErrorKind::NotFound {
            lore_trace!(
                "Path does not exist anymore after metadata query: {}",
                absolute_path.display()
            );
        } else {
            lore_debug!(
                "Delete metadata query failed for {}: {}",
                absolute_path.display(),
                err
            );
        }
    }

    Ok(())
}

pub async fn unlink_recursive<P: AsRef<Path>>(absolute_path: P) -> tokio::io::Result<()> {
    let absolute_path = absolute_path.as_ref();
    lore_trace!("Deleting {}", absolute_path.display());
    let metadata = tokio::fs::metadata(absolute_path).await;

    if let Err(err) = metadata {
        if err.kind() == tokio::io::ErrorKind::NotFound {
            lore_trace!(
                "Path does not exist anymore after metadata query: {}",
                absolute_path.display()
            );
            return Ok(());
        } else {
            lore_trace!(
                "Delete metadata query failed for {}: {}",
                absolute_path.display(),
                err
            );
            return Ok(());
        }
    }

    let metadata = metadata.unwrap();
    if metadata.is_dir() {
        if let Err(err) = tokio::fs::remove_dir_all(absolute_path).await {
            if err.kind() == tokio::io::ErrorKind::NotFound {
                lore_trace!(
                    "Path does not exist anymore after removing recursively {}: {}",
                    absolute_path.display(),
                    err
                );
                return Ok(());
            }
            lore_debug!(
                "Error deleting directory {}: {} - retry after setting write permission",
                absolute_path.display(),
                err
            );

            let mut permissions = metadata.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            let _ = tokio::fs::set_permissions(absolute_path, permissions).await;
            if let Err(err) = tokio::fs::remove_dir_all(absolute_path).await {
                if err.kind() == tokio::io::ErrorKind::NotFound {
                    lore_trace!(
                        "Path does not exist anymore after trying remove recursively with write permissions: {}",
                        absolute_path.display()
                    );
                    return Ok(());
                } else {
                    lore_debug!(
                        "Error deleting directory with write permissions {}: {}",
                        absolute_path.display(),
                        err
                    );
                }
                return Err(err);
            }
        }
        lore_trace!("Recursively deleted directory {}", absolute_path.display(),);
    } else {
        if let Err(err) = tokio::fs::remove_file(absolute_path).await {
            if err.kind() == tokio::io::ErrorKind::NotFound {
                lore_trace!(
                    "Path does not exist anymore after removing file with write permissions: {}",
                    absolute_path.display()
                );
                return Ok(());
            }
            lore_debug!(
                "Error deleting file {}: {} - retry after setting write permission",
                absolute_path.display(),
                err
            );

            let mut permissions = metadata.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            let _ = tokio::fs::set_permissions(absolute_path, permissions).await;
            if let Err(err) = tokio::fs::remove_file(absolute_path).await {
                if err.kind() == tokio::io::ErrorKind::NotFound {
                    lore_trace!(
                        "Path does not exist anymore after trying remove file with write permissions: {}",
                        absolute_path.display()
                    );
                    return Ok(());
                } else {
                    lore_debug!(
                        "Error deleting file with write permissions {}: {}",
                        absolute_path.display(),
                        err
                    );
                }
                return Err(err);
            }
        }
        lore_trace!("Deleted file {}", absolute_path.display(),);
    }

    Ok(())
}

#[cfg(not(target_family = "windows"))]
pub fn sync_dir<P: AsRef<Path>>(path: P) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(path.as_ref())?;
    let fd = dir.as_raw_fd();
    // SAFETY: Safe to call libc function to flush directory changes
    let result = unsafe { libc::fsync(fd) };
    if result == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(target_family = "windows")]
pub fn sync_dir<P: AsRef<Path>>(_path: P) -> tokio::io::Result<()> {
    // No-op on Windows, there is no API to flush a directory
    Ok(())
}

pub fn file_unlink_retry() -> Retry {
    RetryPolicy::builder()
        .with_initial_backoff_millis(2)
        .with_max_backoff_millis(500)
        .with_limit(10)
        .build()
        .retry()
}

pub fn generate_temppath(prefix: &str) -> std::path::PathBuf {
    let name = format!(
        "{prefix}-{}{TEMP_FILE_EXTENSION}",
        Alphanumeric.sample_string(&mut rand::rng(), 16).as_str()
    );
    let mut path = std::env::temp_dir();
    path.push(name);
    path
}
