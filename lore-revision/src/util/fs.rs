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
use std::sync::Arc;

use lore_base::lore_spawn;
use rand::distr::Alphanumeric;
use rand::distr::SampleString;
use tokio::task::JoinSet;

use super::path::DepthPath;
use super::path::RelativePath;
use super::path::RelativePathBuf;
use super::path::path_depth;
use crate::MAX_CONCURRENT_TREE_TASKS;
use crate::hash::hash_string;
use crate::lore_debug;
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

    let _ = lore_io::IoDriver::global()
        .set_permissions(path, permissions)
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

/// Whether every one of `names` is present in `parent`, compared exactly rather than by the
/// filesystem's own rules — `Path::exists` is case-insensitive on Windows and macOS, which is
/// the distinction a caller resolving a case collision is asking about.
///
/// One listing answers for all of them, and it stops as soon as they are all accounted for: a
/// caller asking about several names in a directory is asking one question about it. Asking
/// about no names is answered without reading anything.
///
/// Matches are tracked in a bitmask, so at most 64 names can be asked about at once.
pub async fn filesystem_names_all_exist(parent: &Path, names: &[&str]) -> bool {
    assert!(
        names.len() <= u64::BITS as usize,
        "filesystem_names_all_exist takes at most {} names",
        u64::BITS
    );
    if names.is_empty() {
        return true;
    }

    let Ok(mut listing) = lore_io::IoDriver::global().read_dir(parent).await else {
        return false;
    };
    let wanted = if names.len() == u64::BITS as usize {
        u64::MAX
    } else {
        (1u64 << names.len()) - 1
    };
    let mut found = 0u64;
    while let Some(entry) = listing.next().await {
        let Ok(entry) = entry else {
            continue;
        };
        for (index, name) in names.iter().enumerate() {
            if entry.file_name == *name {
                found |= 1u64 << index;
            }
        }
        if found == wanted {
            return true;
        }
    }
    false
}

// TODO(mjansson): We could pass around a hashmap cache of directory to file list mappings
// while executing an operation, to reduce the number of iterations on the file system to
// find files and their existing names - used by filesystem_name, filesystem_path and list_files
/// Whether the directory `path` holds a child named exactly `name`.
///
/// The question nearly every caller actually has, and the filesystem answers it
/// directly: one lookup, no directory read, and no string to hold the answer.
/// [`filesystem_names`] is for the case this rules out, where the name is on
/// disk in some other case and the caller needs to be told which.
///
/// `Some(false)` does not mean the name is absent - it means not in this case.
/// `None` is the platform declining to say, as macOS does for every name and
/// Windows does past `MAX_PATH`; only reading the directory answers those, which
/// is what [`filesystem_names`] is for.
pub async fn filesystem_name_matches(path: impl AsRef<Path>, name: &str) -> Option<bool> {
    lore_io::IoDriver::global()
        .holds_name_exactly(path.as_ref().join(name))
        .await
}

/// Every case variation of `name` the directory `path` holds, the exact one
/// alone if it is there, and `NotFound` if no variation is.
///
/// Reads the directory and compares every child, which is what answering for
/// variations other than the one asked about takes. A caller that only needs to
/// know whether the case it has is the one on disk should ask
/// [`filesystem_name_matches`] instead.
pub async fn filesystem_names(
    path: impl AsRef<Path>,
    name: &str,
) -> tokio::io::Result<Vec<String>> {
    let path = path.as_ref();

    let mut matches = vec![];
    let match_name = name.to_lowercase();
    let mut listing = lore_io::IoDriver::global().read_dir(path).await?;
    while let Some(entry) = listing.next().await {
        let entry_file_name = entry?.file_name;
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

/// The case each directory prefix is held in on disk, resolved once for the
/// paths that share them.
///
/// A set of targets under one tree is mostly the same directories over and over:
/// 200,000 paths of nine components each are 1.8 million lookups against 29,000
/// distinct directories. Resolving each of those once and handing the answers to
/// [`filesystem_path`] leaves each path with only its own leaf to resolve.
///
/// Keys are relative to the base path they were resolved against, and only that
/// one - a map built for the repository root says nothing about a path under a
/// layer or a link mount. A prefix whose case is ambiguous or that is not
/// there is left out, so paths under it resolve as they would have without this.
#[derive(Default)]
pub struct ResolvedPrefixes {
    prefixes: std::collections::HashMap<String, String>,
}

impl ResolvedPrefixes {
    pub fn is_empty(&self) -> bool {
        self.prefixes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.prefixes.len()
    }

    /// The longest resolved prefix covering `path`, as the number of components
    /// it accounts for and the case variation to use for them.
    ///
    /// Starts from `path` itself so a prefix asked about directly answers for
    /// itself, then walks up. The parent hits on the first or second try for any
    /// path in a set that shares its directories, which is the case this is for.
    pub fn longest_prefix_of(&self, path: &str) -> Option<(usize, &str)> {
        let mut end = path.len();
        loop {
            let candidate = &path[..end];
            if let Some(resolved) = self.prefixes.get(candidate) {
                return Some((path_depth(candidate), resolved.as_str()));
            }
            end = candidate.rfind('/')?;
        }
    }

    pub(crate) fn insert(&mut self, path: String, resolved: String) {
        self.prefixes.insert(path, resolved);
    }
}

/// Resolve the on-disk case of each of `paths`, each against the case already
/// established for its parent.
///
/// `paths` must be shallowest first, so a parent is always resolved before the
/// children that are resolved against it - which the caller has anyway, since
/// that is the order shared ancestors have to be created in.
///
/// A run of paths at the same depth resolves as one batch. Nothing in such a run
/// is an ancestor of anything else in it, so none of them is waiting on another,
/// and each is one or two syscalls: doing them one at a time is one round trip to
/// the syscall pool per path.
pub(crate) async fn resolve_prefixes(
    base_path: impl AsRef<Path>,
    paths: &[DepthPath],
) -> ResolvedPrefixes {
    /// The parent a run of siblings shares, resolved once for the run.
    struct ParentRun<'a> {
        parent: &'a str,
        variation: Arc<str>,
        directory: Arc<Path>,
    }

    fn parent_of(path: &str) -> &str {
        path.rfind('/').map_or("", |separator| &path[..separator])
    }

    fn resolve_parent<'a>(
        parent: &'a str,
        base_path: &Path,
        resolved: &ResolvedPrefixes,
    ) -> ParentRun<'a> {
        // A parent left out of the map resolves to itself: either it is the root,
        // or it could not be resolved and this will not resolve either.
        let variation: Arc<str> = resolved
            .longest_prefix_of(parent)
            .map_or(parent, |(_, it)| it)
            .into();
        let mut directory = base_path.to_path_buf();
        if !variation.is_empty() {
            directory.push(variation.as_ref());
        }
        ParentRun {
            parent,
            variation,
            directory: directory.into(),
        }
    }

    fn collect(
        joined: Result<Option<(String, String)>, tokio::task::JoinError>,
        resolved: &mut ResolvedPrefixes,
    ) {
        if let Ok(Some((path, variation))) = joined {
            resolved.insert(path, variation);
        }
    }

    let base_path = base_path.as_ref();
    let mut resolved = ResolvedPrefixes::default();
    let mut level_start = 0;
    while level_start < paths.len() {
        let depth = paths[level_start].depth();
        let mut level_end = level_start;
        while level_end < paths.len() && paths[level_end].depth() == depth {
            level_end += 1;
        }

        // Every parent of a level sits above it, so nothing the level resolves
        // changes one and a run of siblings answers from the first of them.
        let mut shared = resolve_parent(parent_of(paths[level_start].path()), base_path, &resolved);

        let mut tasks: JoinSet<Option<(String, String)>> = JoinSet::new();
        for path in &paths[level_start..level_end] {
            let parent = parent_of(path.path());
            if shared.parent != parent {
                shared = resolve_parent(parent, base_path, &resolved);
            }

            let variation = shared.variation.clone();
            let directory = shared.directory.clone();
            let path = path.path().to_string();
            lore_spawn!(tasks, async move {
                let name = path
                    .rfind('/')
                    .map_or(path.as_str(), |separator| &path[separator + 1..]);
                if filesystem_name_matches(&directory, name).await == Some(true) {
                    let resolved = join_relative(&variation, name);
                    return Some((path, resolved));
                }
                // One variation is an answer; several are the ambiguity
                // `filesystem_path` forks on, and are left for it to find as it
                // always did. A platform that would not say arrives here too,
                // and the read settles it.
                if let Ok(names) = filesystem_names(&directory, name).await
                    && let [single] = names.as_slice()
                {
                    let resolved = join_relative(&variation, single);
                    return Some((path, resolved));
                }
                None
            });

            while let Some(joined) = tasks.try_join_next() {
                collect(joined, &mut resolved);
            }
            while tasks.len() >= MAX_CONCURRENT_TREE_TASKS
                && let Some(joined) = tasks.join_next().await
            {
                collect(joined, &mut resolved);
            }
        }
        while let Some(joined) = tasks.join_next().await {
            collect(joined, &mut resolved);
        }
        level_start = level_end;
    }
    resolved
}

fn join_relative(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

/// `find_path` in the case the file system holds it, relative to `base_path`.
///
/// The components are read off the file system and joined here, so the result is
/// clean by construction and a caller can walk it without validating or cleaning
/// it again.
///
/// A path the file system does not hold is an error rather than a case.
pub async fn filesystem_path(
    base_path: impl AsRef<Path>,
    find_path: &RelativePath,
    prefixes: Option<&ResolvedPrefixes>,
) -> tokio::io::Result<RelativePath> {
    filesystem_path_and_metadata(base_path, find_path, prefixes)
        .await
        .map(|(path, _)| path)
}

/// The metadata of `path`, stat'ed unless [`filesystem_path_and_metadata`]
/// already read it while resolving the path. A path the file system does not
/// hold has none, which is what the callers act on.
pub async fn metadata_or_stat(
    resolved: Option<Metadata>,
    path: impl Into<PathBuf>,
) -> Option<Metadata> {
    match resolved {
        Some(metadata) => Some(metadata),
        None => lore_io::IoDriver::global().metadata(path).await.ok(),
    }
}

/// [`filesystem_path`], and the metadata of the resolved path where establishing
/// it read that metadata, so a caller needing both reads it once.
///
/// `None` where the path was resolved a component at a time, which establishes
/// each name without reading the metadata of the whole.
pub async fn filesystem_path_and_metadata(
    base_path: impl AsRef<Path>,
    find_path: &RelativePath,
    prefixes: Option<&ResolvedPrefixes>,
) -> tokio::io::Result<(RelativePath, Option<std::fs::Metadata>)> {
    let base_path = base_path.as_ref();

    // TODO(mjansson): This should be a test for file system case sensitivity, in the sense that the file system
    //                 support multiple concurrent case variations of the same file name
    #[cfg(target_os = "linux")]
    {
        let initial_path = base_path.join(find_path.as_str());
        if let Ok(metadata) = lore_io::IoDriver::global().metadata(initial_path).await {
            return Ok((find_path.clone(), Some(metadata)));
        }
    }

    let mut full_path = base_path.to_path_buf();
    let mut remain_path = find_path.clone();
    let mut found_path = RelativePathBuf::with_capacity(find_path.len());

    // Whatever an earlier path already established is not established again.
    if let Some((components, resolved)) =
        prefixes.and_then(|prefixes| prefixes.longest_prefix_of(find_path.as_str()))
    {
        full_path.push(resolved);
        found_path.push(resolved);
        remain_path.pop_root_repeat(components);
    }

    while !remain_path.is_empty() {
        let name = remain_path.pop_root();
        // Nearly every component is already in the case the filesystem holds it,
        // and that costs one lookup to establish. Only where it is not, or where
        // the platform will not say, does the directory get read, and a name
        // allocated for what it says.
        if filesystem_name_matches(full_path.as_path(), name).await == Some(true) {
            full_path.push(name);
            found_path.push(name);
            continue;
        }
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
    Ok((found_path.freeze(), None))
}

pub fn filesystem_path_fork(
    base_path: impl AsRef<Path>,
    find_path: &RelativePath,
) -> Pin<Box<dyn Future<Output = tokio::io::Result<RelativePath>> + Send>> {
    let base_path = base_path.as_ref().to_path_buf();
    let find_path = find_path.clone();
    // The fork resolves a path under one of several case variations of a
    // directory, which is not a prefix any map here was built against.
    Box::pin(async move { filesystem_path(base_path, &find_path, None).await })
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

    /// The path did not exist, was not accessible, or was a special file type
    /// (symlink, device, etc.) that we don't handle.
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

/// Describes one listing entry, or `None` for one that says nothing about what is there: a name
/// the walk could not read, or one whose metadata would not resolve — a broken link, or a name
/// unlinked while the walk was running. A caller enumerating what is present skips those; one
/// unreadable name says nothing about the rest of the directory.
pub fn file_list_item(entry: std::io::Result<lore_io::DirEntry>) -> Option<FileListItem> {
    let entry = entry.ok()?;
    let metadata = entry.metadata?;
    let name = entry.file_name.to_string_lossy().to_string();
    let name_hash = hash_string(name.as_str());
    Some(FileListItem {
        name,
        metadata,
        name_hash,
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
pub async fn list_path(path: PathBuf) -> PathListingResult {
    let driver = lore_io::IoDriver::global();

    if let Ok(listing) = driver.read_dir(path.as_path()).await {
        return PathListingResult::Directory { listing };
    }

    let Ok(metadata) = driver.metadata(path.as_path()).await else {
        return PathListingResult::NotFound;
    };

    if metadata.is_file() {
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
/// * `Ok(listing)` - Yields an entry for each child; [`file_list_item`] describes one
/// * `Err(_)` - If path doesn't exist, isn't accessible, or isn't a directory
pub async fn list_directory(path: PathBuf) -> std::io::Result<lore_io::DirStream> {
    lore_io::IoDriver::global().read_dir(path.as_path()).await
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

pub async fn unlink<P: AsRef<Path>>(absolute_path: P) -> tokio::io::Result<()> {
    let absolute_path = absolute_path.as_ref();
    lore_trace!("Deleting {}", absolute_path.display());
    let metadata = lore_io::IoDriver::global().metadata(absolute_path).await;

    if let Ok(metadata) = metadata {
        if metadata.is_dir() {
            if let Err(err) = lore_io::IoDriver::global().remove_dir(absolute_path).await {
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
                let _ = lore_io::IoDriver::global()
                    .set_permissions(absolute_path, permissions)
                    .await;
                if let Err(err) = lore_io::IoDriver::global().remove_dir(absolute_path).await {
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
            if let Err(err) = lore_io::IoDriver::global().remove_file(absolute_path).await {
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
                let _ = lore_io::IoDriver::global()
                    .set_permissions(absolute_path, permissions)
                    .await;
                if let Err(err) = lore_io::IoDriver::global().remove_file(absolute_path).await {
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
    let metadata = lore_io::IoDriver::global().metadata(absolute_path).await;

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
        if let Err(err) = lore_io::IoDriver::global()
            .remove_dir_all(absolute_path)
            .await
        {
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
            let _ = lore_io::IoDriver::global()
                .set_permissions(absolute_path, permissions)
                .await;
            if let Err(err) = lore_io::IoDriver::global()
                .remove_dir_all(absolute_path)
                .await
            {
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
        if let Err(err) = lore_io::IoDriver::global().remove_file(absolute_path).await {
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
            let _ = lore_io::IoDriver::global()
                .set_permissions(absolute_path, permissions)
                .await;
            if let Err(err) = lore_io::IoDriver::global().remove_file(absolute_path).await {
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

#[cfg(test)]
// Fixtures build filesystem state directly; what these test is how the helpers read it.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[tokio::test]
    async fn list_path_yields_a_directory_listing() {
        let dir = temp_dir();
        std::fs::write(dir.path().join("child"), b"data").expect("write child");

        let PathListingResult::Directory { mut listing } =
            list_path(dir.path().to_path_buf()).await
        else {
            panic!("a directory must list");
        };
        let mut names = Vec::new();
        while let Some(entry) = listing.next().await {
            if let Some(item) = file_list_item(entry) {
                names.push(item.name);
            }
        }
        assert_eq!(names, vec!["child".to_string()]);
    }

    #[tokio::test]
    async fn list_path_describes_a_file_by_its_own_name() {
        let dir = temp_dir();
        let path = dir.path().join("lonely.txt");
        std::fs::write(&path, b"data").expect("write file");

        let PathListingResult::File { item } = list_path(path).await else {
            panic!("a file must be described, not listed");
        };
        assert_eq!(item.name, "lonely.txt");
        assert_eq!(item.metadata.len(), 4);
    }

    #[tokio::test]
    async fn list_path_reports_a_missing_path() {
        let dir = temp_dir();
        assert!(
            list_path(dir.path().join("absent")).await.is_not_found(),
            "a missing path is neither a file nor a directory"
        );
    }

    /// Asking about no names is satisfied by any directory, including an empty one — the check
    /// is over the names given, and there are none to be missing.
    #[tokio::test]
    async fn all_names_exist_is_true_for_no_names() {
        let dir = temp_dir();
        assert!(filesystem_names_all_exist(dir.path(), &[]).await);
    }

    #[tokio::test]
    async fn all_names_exist_requires_every_name() {
        let dir = temp_dir();
        std::fs::write(dir.path().join("one"), b"").expect("write one");
        std::fs::write(dir.path().join("two"), b"").expect("write two");

        assert!(filesystem_names_all_exist(dir.path(), &["one", "two"]).await);
        assert!(!filesystem_names_all_exist(dir.path(), &["one", "three"]).await);
        assert!(!filesystem_names_all_exist(dir.path(), &["three"]).await);
    }

    /// The comparison is exact, which is the whole reason this exists rather than `Path::exists`:
    /// on a case-insensitive filesystem that would answer for a name that is not the one asked
    /// about.
    #[tokio::test]
    async fn all_names_exist_compares_exactly() {
        let dir = temp_dir();
        std::fs::write(dir.path().join("Assets"), b"").expect("write Assets");

        assert!(filesystem_names_all_exist(dir.path(), &["Assets"]).await);
        assert!(!filesystem_names_all_exist(dir.path(), &["assets"]).await);
    }

    fn depth_paths(paths: &[&str]) -> Vec<DepthPath> {
        paths
            .iter()
            .map(|path| DepthPath::new((*path).to_string()))
            .collect()
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

    /// The distinction the whole thing rests on: this answers for the case
    /// variation asked about, where `Path::exists` answers for the file whatever
    /// it is called. A case-insensitive filesystem finds the file under either name,
    /// and must still say no to the one it does not hold.
    #[tokio::test]
    async fn name_matches_only_the_case_variation_on_disk() {
        let dir = temp_dir();
        std::fs::write(dir.path().join("Test.file"), b"").expect("write file");

        assert_eq!(
            filesystem_name_matches(dir.path(), "Test.file").await,
            verdict(true)
        );
        assert_eq!(
            filesystem_name_matches(dir.path(), "test.FILE").await,
            verdict(false),
            "a case variation the filesystem does not hold is not a match, whether or not it would find the file"
        );
        assert_eq!(
            filesystem_name_matches(dir.path(), "other.file").await,
            verdict(false)
        );
        assert_eq!(
            filesystem_name_matches(dir.path(), "Test.file.").await,
            verdict(false),
            "a trailing dot names a file that is not there"
        );
        assert_eq!(
            filesystem_name_matches(&dir.path().join("absent"), "Test.file").await,
            verdict(false),
            "a missing directory holds nothing"
        );
    }

    /// A directory is what most components resolve to, and the lookup has to
    /// answer for one as readily as for a file.
    #[tokio::test]
    async fn name_matches_a_directory() {
        let dir = temp_dir();
        std::fs::create_dir(dir.path().join("Assets")).expect("create dir");

        assert_eq!(
            filesystem_name_matches(dir.path(), "Assets").await,
            verdict(true)
        );
    }

    /// The path already in the case the filesystem holds it in - the case
    /// [`filesystem_path`] is built around - comes back unchanged.
    #[tokio::test]
    async fn path_resolves_a_path_already_in_the_case_on_disk() {
        let dir = temp_dir();
        let nested = dir.path().join("Assets").join("Meshes");
        std::fs::create_dir_all(&nested).expect("create dirs");
        std::fs::write(nested.join("Rock.mesh"), b"").expect("write file");

        let asked: RelativePath =
            std::str::FromStr::from_str("Assets/Meshes/Rock.mesh").expect("relative path");
        let (resolved, metadata) = filesystem_path_and_metadata(dir.path(), &asked, None)
            .await
            .expect("the path must resolve");
        assert_eq!(resolved.as_str(), "Assets/Meshes/Rock.mesh");
        assert_eq!(
            metadata.is_some(),
            cfg!(target_os = "linux"),
            "the metadata comes back from the platforms that settle the path by reading it whole"
        );
    }

    #[tokio::test]
    async fn names_answers_with_the_case_variation_it_was_given() {
        let dir = temp_dir();
        std::fs::write(dir.path().join("Test.file"), b"").expect("write file");

        assert_eq!(
            filesystem_names(dir.path(), "Test.file")
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
            filesystem_names(dir.path(), "test.FILE")
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
            filesystem_names(dir.path(), "other.file").await.is_err(),
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
            filesystem_names(dir.path(), "Test.file.").await.is_err(),
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

        let mut found = filesystem_names(dir.path(), "TEST.FILE")
            .await
            .expect("the variations must resolve");
        found.sort();
        assert_eq!(
            found,
            vec!["Test.file".to_string(), "test.file".to_string()],
            "an ambiguous name must report every case variation, not pick one"
        );

        assert_eq!(
            filesystem_names(dir.path(), "test.file")
                .await
                .expect("an exact name must resolve"),
            vec!["test.file".to_string()],
            "a case variation the filesystem holds answers for itself, ambiguity or not"
        );
    }

    /// A path is resolved a component at a time, so an ancestor in the wrong case has to be
    /// corrected as well as the leaf.
    #[tokio::test]
    async fn path_resolves_every_component_to_its_stored_case_variation() {
        let dir = temp_dir();
        if !case_insensitive(dir.path()) {
            return;
        }
        let nested = dir.path().join("Assets").join("Meshes");
        std::fs::create_dir_all(&nested).expect("create dirs");
        std::fs::write(nested.join("Rock.mesh"), b"").expect("write file");

        let asked = std::str::FromStr::from_str("assets/MESHES/rock.MESH")
            .expect("relative path is infallible");
        let resolved = filesystem_path(dir.path(), &asked, None)
            .await
            .expect("the path must resolve");
        assert_eq!(resolved.as_str(), "Assets/Meshes/Rock.mesh");
        assert_eq!(
            resolved.as_lowercase_str(),
            "assets/meshes/rock.mesh",
            "the lowercase form answers for the case that was resolved"
        );
    }

    /// Shared directories are resolved once, and every path under them then uses
    /// what was established rather than looking again — including when the
    /// case the caller has differs from the one on disk, which is the part
    /// that would go wrong if the answer were not carried over.
    ///
    /// Spellings are matched over a directory listing rather than by asking the
    /// filesystem to look the name up, so this holds on a case-sensitive one as
    /// much as on a case-insensitive one and is not conditioned on which it is.
    #[tokio::test]
    async fn resolved_prefixes_answer_for_the_paths_under_them() {
        let dir = temp_dir();
        let nested = dir.path().join("Assets").join("Meshes");
        std::fs::create_dir_all(&nested).expect("create dirs");
        std::fs::write(nested.join("Rock.mesh"), b"").expect("write file");

        // Shallowest first, as the caller has them.
        let shared = depth_paths(&["assets", "assets/meshes"]);
        let prefixes = resolve_prefixes(dir.path(), &shared).await;

        assert_eq!(prefixes.len(), 2);
        assert_eq!(prefixes.longest_prefix_of("assets"), Some((1, "Assets")));
        assert_eq!(
            prefixes.longest_prefix_of("assets/meshes"),
            Some((2, "Assets/Meshes"))
        );
        assert_eq!(
            prefixes.longest_prefix_of("assets/meshes/rock.MESH"),
            Some((2, "Assets/Meshes")),
            "a path is covered by the longest prefix above it, not by itself"
        );
        assert_eq!(prefixes.longest_prefix_of("elsewhere/file"), None);

        let asked: RelativePath =
            std::str::FromStr::from_str("assets/meshes/rock.MESH").expect("relative path");
        let (resolved, metadata) =
            filesystem_path_and_metadata(dir.path(), &asked, Some(&prefixes))
                .await
                .expect("the path must resolve");
        assert_eq!(resolved.as_str(), "Assets/Meshes/Rock.mesh");
        assert_eq!(
            resolved.as_lowercase_str(),
            "assets/meshes/rock.mesh",
            "the prefix the map answered with carries a lowercase form of its own"
        );
        assert!(
            metadata.is_none(),
            "a path settled a component at a time is never read whole"
        );
    }

    /// A prefix that is not there, or that several case variations answer for, is left
    /// out — so a path under it resolves as it would have with no map at all,
    /// rather than being resolved against a directory that was guessed.
    #[tokio::test]
    async fn resolved_prefixes_leave_out_what_they_cannot_settle() {
        let dir = temp_dir();
        std::fs::create_dir(dir.path().join("Assets")).expect("create dir");

        let shared = depth_paths(&["absent", "absent/deeper"]);
        let prefixes = resolve_prefixes(dir.path(), &shared).await;
        assert!(prefixes.is_empty(), "nothing there resolves");

        // The path still resolves through the walk, which reports it as missing
        // in the same way it would without a map.
        let asked: RelativePath =
            std::str::FromStr::from_str("absent/file").expect("relative path");
        assert!(
            filesystem_path(dir.path(), &asked, Some(&prefixes))
                .await
                .is_err()
        );

        if case_insensitive(dir.path()) {
            return;
        }
        std::fs::create_dir(dir.path().join("assets")).expect("create second variation");
        let shared = depth_paths(&["ASSETS"]);
        assert!(
            resolve_prefixes(dir.path(), &shared).await.is_empty(),
            "two case variations answer for it, so the caller has to fork and decide"
        );
    }

    /// The map is an answer about the filesystem as it was when it was built. A
    /// caller that renames while it stages - which `StageCaseChange::Keep` does,
    /// including to the directories these prefixes name - must not be given one,
    /// and this is what that would look like: the path still resolves, to the
    /// case variation that is no longer there.
    #[tokio::test]
    async fn a_resolved_prefix_does_not_survive_the_directory_being_renamed() {
        let dir = temp_dir();
        std::fs::create_dir(dir.path().join("Assets")).expect("create dir");
        std::fs::write(dir.path().join("Assets").join("rock.mesh"), b"").expect("write file");

        let prefixes = resolve_prefixes(dir.path(), &depth_paths(&["Assets"])).await;
        assert_eq!(prefixes.longest_prefix_of("Assets"), Some((1, "Assets")));

        std::fs::rename(dir.path().join("Assets"), dir.path().join("ASSETS")).expect("rename");

        let asked: RelativePath =
            std::str::FromStr::from_str("Assets/rock.mesh").expect("relative path");
        let afresh = filesystem_path(dir.path(), &asked, None).await.ok();
        assert_eq!(
            afresh.as_ref().map(RelativePath::as_str),
            Some("ASSETS/rock.mesh"),
            "resolving afresh finds the directory under the name it now has"
        );
        let mapped = filesystem_path(dir.path(), &asked, Some(&prefixes))
            .await
            .ok();
        assert_ne!(
            mapped.as_ref().map(RelativePath::as_str),
            Some("ASSETS/rock.mesh"),
            "the map still answers with the name the directory had"
        );
    }

    /// What the map is allowed to change is how long resolving takes, never what
    /// it resolves to. Every shape that reaches it has to come back the same
    /// either way, because everything downstream - the tree node the path is
    /// compared against, and what a case change means for it - reads the result
    /// and nothing else.
    #[tokio::test]
    async fn a_resolved_prefix_answers_exactly_as_the_walk_would() {
        let dir = temp_dir();
        let nested = dir.path().join("Assets").join("Meshes");
        std::fs::create_dir_all(&nested).expect("create dirs");
        std::fs::write(nested.join("Rock.mesh"), b"").expect("write file");
        std::fs::create_dir(dir.path().join("Assets").join("Empty")).expect("create dir");

        let shared = depth_paths(&[
            "Assets",
            "assets",
            "Assets/Meshes",
            "assets/meshes",
            "absent",
        ]);
        let prefixes = resolve_prefixes(dir.path(), &shared).await;

        for asked in [
            // Given exactly as the filesystem holds it.
            "Assets/Meshes/Rock.mesh",
            // Given in another case, at the leaf, at an ancestor, and at both.
            "Assets/Meshes/rock.MESH",
            "assets/meshes/Rock.mesh",
            "ASSETS/MESHES/ROCK.MESH",
            // Not there at all, below a prefix that is and one that is not.
            "Assets/Meshes/absent.mesh",
            "absent/deeper/absent.mesh",
            // A directory rather than a file, and a single component.
            "Assets/Empty",
            "Assets",
        ] {
            let asked: RelativePath = std::str::FromStr::from_str(asked).expect("relative path");
            assert_eq!(
                filesystem_path(dir.path(), &asked, Some(&prefixes))
                    .await
                    .ok(),
                filesystem_path(dir.path(), &asked, None).await.ok(),
                "{asked} must resolve the same with the map as without it"
            );
        }
    }

    #[tokio::test]
    async fn all_names_exist_is_false_for_an_unreadable_directory() {
        let dir = temp_dir();
        assert!(!filesystem_names_all_exist(&dir.path().join("absent"), &["any"]).await);
    }
}
