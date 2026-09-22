// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fs::Metadata;
use std::future::Future;
#[cfg(target_family = "unix")]
use std::os::unix::fs::MetadataExt;
#[cfg(target_family = "unix")]
use std::os::unix::fs::PermissionsExt;
#[cfg(target_family = "windows")]
use std::os::windows::fs::MetadataExt;
use std::path::Path;
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
use crate::fs::filesystem_provider::FileInfo;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::InstanceOperationImpl;
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

/// The mode to store on a node whose mode is `previous`: the observed executable bit
/// where the platform gave one, `previous`'s where it did not, and none for a path that
/// is not a file.
pub fn mode_from_observed(is_file: bool, executable: Option<bool>, previous: u16) -> u16 {
    if !is_file {
        return 0;
    }
    match executable {
        Some(true) => NodeFileMode::Executable.bits(),
        Some(false) => 0,
        None => previous & NodeFileMode::Executable.bits(),
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

/// Whether the file carries the executable bit, `None` where the platform has no such
/// bit to read. [`file_is_executable`] answers `false` for both and cannot tell them
/// apart, which a caller updating a node's mode needs.
#[cfg(target_family = "windows")]
pub fn file_executable_observed(_metadata: &Metadata) -> Option<bool> {
    None
}

#[cfg(target_family = "unix")]
pub fn file_executable_observed(metadata: &Metadata) -> Option<bool> {
    Some(file_is_executable(metadata))
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
/// layer or a link mount. A prefix [`spelling_to_take`] cannot settle, or that is
/// not there, is left out, so paths under it resolve as they would have without
/// this.
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
    operation: &Arc<InstanceOperationImpl>,
    base_path: impl AsRef<Path>,
    paths: &[DepthPath],
) -> ResolvedPrefixes {
    /// The parent a run of siblings shares, resolved once for the run.
    struct ParentRun<'a> {
        parent: &'a str,
        variation: Arc<str>,
        directory: Arc<RelativePath>,
    }

    fn parent_of(path: &str) -> &str {
        path.rfind('/').map_or("", |separator| &path[..separator])
    }

    fn resolve_parent<'a>(parent: &'a str, resolved: &ResolvedPrefixes) -> ParentRun<'a> {
        // A parent left out of the map resolves to itself: either it is the root,
        // or it could not be resolved and this will not resolve either.
        let variation: Arc<str> = resolved
            .longest_prefix_of(parent)
            .map_or(parent, |(_, it)| it)
            .into();
        ParentRun {
            parent,
            directory: Arc::new(RelativePath::new_from_clean_parts(variation.as_ref(), "")),
            variation,
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
    let root: Arc<Path> = Arc::from(base_path.to_path_buf());
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
        let mut shared = resolve_parent(parent_of(paths[level_start].path()), &resolved);

        let mut tasks: JoinSet<Option<(String, String)>> = JoinSet::new();
        for path in &paths[level_start..level_end] {
            let parent = parent_of(path.path());
            if shared.parent != parent {
                shared = resolve_parent(parent, &resolved);
            }

            let variation = shared.variation.clone();
            let directory = shared.directory.clone();
            let path = path.path().to_string();
            let operation = operation.clone();
            let _root = root.clone();
            lore_spawn!(tasks, async move {
                let name = path
                    .rfind('/')
                    .map_or(path.as_str(), |separator| &path[separator + 1..]);
                let candidate = candidate_path(directory.as_str(), name);
                if operation.holds_name_exactly(&candidate).await == Some(true) {
                    let resolved = join_relative(&variation, name);
                    return Some((path, resolved));
                }
                // A platform that would not say arrives here too, and the read settles it.
                if let Ok(names) = operation.names_folding_to(&directory, name).await
                    && let Some(spelling) = spelling_to_take(&names, name)
                {
                    let resolved = join_relative(&variation, spelling);
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

/// The path a candidate name has, named as the operation names paths: from the root it was
/// opened on, which is what `parent` is already a path under.
fn candidate_path(parent: &str, name: &str) -> RelativePath {
    RelativePath::new_from_clean_parts(parent, name)
}

/// The spelling to take from the ones the directory holds: the one asked for where it is among
/// them, and the sole variation otherwise.
///
/// Several variations with none of them the spelling asked for is the ambiguity
/// [`filesystem_path`] forks on, and is no answer here.
///
/// The spelling asked for is among them only where the platform declined the lookup that would
/// have settled it — macOS for every name, Windows past its path limit. A platform that answers
/// reports a name it holds as held, so a resolver reaching here has already been told the
/// spelling is not there.
fn spelling_to_take<'a>(held: &'a [String], name: &str) -> Option<&'a str> {
    if let Some(exact) = held.iter().find(|spelling| *spelling == name) {
        return Some(exact);
    }
    match held {
        [single] => Some(single.as_str()),
        _ => None,
    }
}

// TODO(mjansson): We could pass around a hashmap cache of directory to file list mappings
// while executing an operation, to reduce the number of iterations on the file system to
// find files and their existing names - used by the resolver here and by the name lookups
// and listings under `crate::fs::os`.
/// `find_path` in the case the file system holds it, relative to `base` -- itself a clean path
/// from the root the operation was opened on, and empty where the two are the same.
///
/// The components are read off the file system and joined here, so the result is
/// clean by construction and a caller can walk it without validating or cleaning
/// it again.
///
/// A path the file system does not hold is an error rather than a case.
pub async fn filesystem_path(
    operation: &InstanceOperationImpl,
    base: &str,
    find_path: &RelativePath,
    prefixes: Option<&ResolvedPrefixes>,
) -> tokio::io::Result<RelativePath> {
    filesystem_path_and_info(operation, base, find_path, prefixes)
        .await
        .map(|(path, _)| path)
}

/// [`filesystem_path`], and what the operation reported about the resolved path where
/// establishing it read that, so a caller needing both asks once.
///
/// `None` where the path was resolved a component at a time, which establishes
/// each name without reading anything about the whole.
///
/// Names are established in the space the operation names paths in, so the buffer starts at
/// `base` and the base is taken off the answer at the end -- a view moving rather than a path
/// being built.
pub async fn filesystem_path_and_info(
    operation: &InstanceOperationImpl,
    base: &str,
    find_path: &RelativePath,
    prefixes: Option<&ResolvedPrefixes>,
) -> tokio::io::Result<(RelativePath, Option<FileInfo>)> {
    // TODO(mjansson): This should be a test for file system case sensitivity, in the sense that the file system
    //                 support multiple concurrent case variations of the same file name
    #[cfg(target_os = "linux")]
    {
        let initial_path = candidate_path(base, find_path.as_str());
        if let Ok(info) = operation.file_info(&initial_path).await
            && info.exists()
        {
            return Ok((find_path.clone(), Some(info)));
        }
    }

    let mut remain_path = find_path.clone();
    let base_depth = if base.is_empty() { 0 } else { path_depth(base) };
    let mut found_path = RelativePathBuf::with_capacity(base.len() + 1 + find_path.len());
    found_path.push(base);

    // Whatever an earlier path already established is not established again.
    if let Some((components, resolved)) =
        prefixes.and_then(|prefixes| prefixes.longest_prefix_of(find_path.as_str()))
    {
        found_path.push(resolved);
        remain_path.pop_root_repeat(components);
    }

    while !remain_path.is_empty() {
        let name = remain_path.pop_root();
        // Nearly every component is already in the case the filesystem holds it,
        // and that costs one lookup to establish. Only where it is not, or where
        // the platform will not say, does the directory get read, and a name
        // allocated for what it says.
        if operation
            .holds_name_exactly(&candidate_path(found_path.as_str(), name))
            .await
            == Some(true)
        {
            found_path.push(name);
            continue;
        }
        let directory = candidate_path(found_path.as_str(), "");
        let Ok(fs_names) = operation.names_folding_to(&directory, name).await else {
            return Err(tokio::io::Error::other(
                "Failed to read the directory for case variations",
            ));
        };
        if fs_names.is_empty() {
            return Err(tokio::io::Error::new(
                tokio::io::ErrorKind::NotFound,
                "Matching file not found",
            ));
        }
        if let Some(spelling) = spelling_to_take(&fs_names, name) {
            found_path.push(spelling);
            continue;
        }
        if remain_path.is_empty() {
            lore_debug!("Found ambiguous path case variations for {find_path}");
            return Err(tokio::io::Error::other(
                "Ambiguous case variations for path {find_path}",
            ));
        }

        // Find the match in either or many of the potential variations
        let mut found_variation = false;
        for entry in fs_names.iter() {
            let next_full_path = directory.join(entry);

            lore_debug!(
                "Fork case variation check for {remain_path} in {}",
                next_full_path
            );
            if let Ok(sub_path) =
                filesystem_path_fork(operation, next_full_path.as_str(), &remain_path).await
            {
                if found_variation {
                    lore_debug!("Found ambiguous path case variations for {find_path}");
                    return Err(tokio::io::Error::other(
                        "Ambiguous case variations found for path {find_path}",
                    ));
                }

                found_path.push(entry);
                found_path.push(sub_path.as_str());

                lore_debug!(
                    "Fork found case variation {sub_path} for {remain_path} in {}",
                    next_full_path
                );
                found_variation = true;
            } else {
                lore_debug!(
                    "Fork found NO case variation for {remain_path} in {}",
                    next_full_path
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

    let mut found = found_path.freeze();
    found.pop_root_repeat(base_depth);
    log_resolved_case(found.as_str(), find_path.as_str(), base);
    Ok((found, None))
}

/// Record a resolved path: at debug where the file system holds the name in a
/// different case than the caller asked for, at trace where it matches, which is
/// every other path a walk resolves.
fn log_resolved_case(found: &str, requested: &str, base: &str) {
    if found == requested {
        lore_trace!("Resolved path {found} in {base}");
    } else {
        lore_debug!("Found full path case variation {found} for path {requested} in path {base}");
    }
}

pub fn filesystem_path_fork<'a>(
    operation: &'a InstanceOperationImpl,
    base: &str,
    find_path: &RelativePath,
) -> Pin<Box<dyn Future<Output = tokio::io::Result<RelativePath>> + Send + 'a>> {
    let base = base.to_owned();
    let find_path = find_path.clone();
    // The fork resolves a path under one of several case variations of a
    // directory, which is not a prefix any map here was built against.
    Box::pin(async move { filesystem_path(operation, &base, &find_path, None).await })
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

    fn temp_dir() -> lore_base::test_util::TempDir {
        lore_base::test_util::TempDir::new("lore-fs-test-")
    }

    /// A path of `depth` components, each named for its level.
    fn nested_path(depth: usize) -> RelativePath {
        let path = (1..=depth)
            .map(|level| format!("level{level}"))
            .collect::<Vec<_>>()
            .join("/");
        std::str::FromStr::from_str(path.as_str()).expect("relative path")
    }

    /// Resolving a path the filesystem holds costs one lookup however deep it is: the path
    /// is read whole and its components are not walked. A per-component walk reintroduced
    /// here would show as a count that grows with the depth asked about.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_held_path_is_looked_up_once_however_deep() {
        use crate::fs::filesystem_provider::FilesystemProvider;
        use crate::fs::filesystem_provider::tests::TestFilesystemProvider;

        for depth in [1usize, 9] {
            let filesystem = Arc::new(TestFilesystemProvider::holding_every_path());
            let operation = FilesystemProvider::begin_operation(filesystem.as_ref())
                .await
                .expect("beginning an operation");
            let asked = nested_path(depth);

            let (resolved, info) = filesystem_path_and_info(&operation, "", &asked, None)
                .await
                .expect("a held path must resolve");

            assert_eq!(resolved.as_str(), asked.as_str());
            assert!(
                info.is_some(),
                "the lookup that settled the path answers for it"
            );
            assert_eq!(
                1,
                filesystem.file_infos(),
                "a path at depth {depth} must cost one lookup"
            );
            assert_eq!(
                0,
                filesystem.name_lookups(),
                "no component was resolved on its own"
            );
            assert_eq!(0, filesystem.directory_reads(), "no directory was read");
        }
    }

    /// A path the filesystem does not hold falls back to resolving a component at a time,
    /// which is the only path that reads per component. It stops at the first component that
    /// is not there, so that cost does not grow with the depth asked about either.
    #[tokio::test]
    async fn a_path_that_is_not_there_stops_at_its_first_component() {
        use crate::fs::filesystem_provider::FilesystemProvider;
        use crate::fs::filesystem_provider::tests::TestFilesystemProvider;

        for depth in [1usize, 9] {
            let filesystem = Arc::new(TestFilesystemProvider::new());
            let operation = FilesystemProvider::begin_operation(filesystem.as_ref())
                .await
                .expect("beginning an operation");

            assert!(
                filesystem_path_and_info(&operation, "", &nested_path(depth), None)
                    .await
                    .is_err(),
                "a path no component of which is there must not resolve"
            );
            assert_eq!(
                1,
                filesystem.name_lookups(),
                "a path at depth {depth} must stop at its first component"
            );
            assert_eq!(1, filesystem.directory_reads(), "one directory settles it");
        }
    }

    /// An operation over the OS filesystem, which is what the resolver reads through.
    async fn os_operation(root: &Path) -> Arc<InstanceOperationImpl> {
        crate::fs::filesystem_provider::FilesystemProvider::begin_operation(
            &crate::fs::os::OsFilesystem::new(root),
        )
        .await
        .expect("beginning an operation over the OS filesystem")
    }

    #[test]
    fn a_sole_variation_is_the_spelling_to_take() {
        let held = vec!["Assets".to_string()];
        assert_eq!(Some("Assets"), spelling_to_take(&held, "assets"));
    }

    /// The spelling asked for wins over its neighbours, which is what keeps a collision between
    /// two variations from reading as an ambiguity for a caller that named one of them.
    #[test]
    fn the_spelling_asked_for_wins_over_a_coexisting_variation() {
        let held = vec!["Assets".to_string(), "assets".to_string()];
        assert_eq!(Some("assets"), spelling_to_take(&held, "assets"));
    }

    /// Several variations, none of them the one asked for, is the ambiguity the caller forks on.
    #[test]
    fn coexisting_variations_the_caller_did_not_name_are_no_answer() {
        let held = vec!["Assets".to_string(), "ASSETS".to_string()];
        assert_eq!(None, spelling_to_take(&held, "assets"));
    }

    #[test]
    fn a_directory_holding_no_spelling_is_no_answer() {
        assert_eq!(None, spelling_to_take(&[], "assets"));
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

    /// A variation held beside the spelling asked for does not disturb resolving it. Where the
    /// platform settles the lookup this never reads the directory; where it declines,
    /// [`spelling_to_take`] answers the same. Only a case-sensitive filesystem can hold the two
    /// directories this needs.
    #[tokio::test]
    async fn path_resolves_a_leaf_whose_case_variation_coexists() {
        let dir = temp_dir();
        if case_insensitive(dir.path()) {
            return;
        }
        let operation = os_operation(dir.path()).await;
        std::fs::create_dir(dir.path().join("Assets")).expect("create dir");
        std::fs::create_dir(dir.path().join("assets")).expect("create variation");

        let asked: RelativePath = std::str::FromStr::from_str("assets").expect("relative path");
        assert_eq!(
            "assets",
            filesystem_path(&operation, "", &asked, None)
                .await
                .expect("the spelling asked for must settle the directory")
                .as_str()
        );
    }

    /// The path already in the case the filesystem holds it in - the case
    /// [`filesystem_path`] is built around - comes back unchanged.
    #[tokio::test]
    async fn path_resolves_a_path_already_in_the_case_on_disk() {
        let dir = temp_dir();
        let operation = os_operation(dir.path()).await;
        let nested = dir.path().join("Assets").join("Meshes");
        std::fs::create_dir_all(&nested).expect("create dirs");
        std::fs::write(nested.join("Rock.mesh"), b"").expect("write file");

        let asked: RelativePath =
            std::str::FromStr::from_str("Assets/Meshes/Rock.mesh").expect("relative path");
        let (resolved, info) = filesystem_path_and_info(&operation, "", &asked, None)
            .await
            .expect("the path must resolve");
        assert_eq!(resolved.as_str(), "Assets/Meshes/Rock.mesh");
        assert_eq!(
            info.is_some(),
            cfg!(target_os = "linux"),
            "the metadata comes back from the platforms that settle the path by reading it whole"
        );
    }

    /// A link or layer mount hands the resolver a base, and what comes back has to be relative
    /// to that base rather than to the root the operation names paths from: a caller staging
    /// inside a mount looks the answer up in the mount's own state, and a path with the mount
    /// prefix still on it is one that state does not hold.
    #[tokio::test]
    async fn path_resolves_below_a_base_and_answers_relative_to_it() {
        let dir = temp_dir();
        let operation = os_operation(dir.path()).await;
        let nested = dir.path().join("Mount").join("Assets");
        std::fs::create_dir_all(&nested).expect("create dirs");
        std::fs::write(nested.join("Rock.mesh"), b"").expect("write file");

        let asked: RelativePath =
            std::str::FromStr::from_str("assets/rock.MESH").expect("relative path");
        let resolved = filesystem_path(&operation, "Mount", &asked, None)
            .await
            .expect("the path must resolve below the base");
        assert_eq!(resolved.as_str(), "Assets/Rock.mesh");
        assert_eq!(resolved.as_lowercase_str(), "assets/rock.mesh");
    }

    /// The lookup that settles a path already in the case on disk answers with the path as
    /// asked, not with the base composed onto it. Only this shape reaches that lookup, since a
    /// path in another case misses it and is settled a component at a time instead.
    #[tokio::test]
    async fn path_below_a_base_does_not_resolve_the_base_itself() {
        let dir = temp_dir();
        let operation = os_operation(dir.path()).await;
        let nested = dir.path().join("Mount").join("Deeper");
        std::fs::create_dir_all(&nested).expect("create dirs");
        std::fs::write(nested.join("leaf.txt"), b"").expect("write file");

        let asked: RelativePath =
            std::str::FromStr::from_str("Deeper/leaf.txt").expect("relative path");
        let resolved = filesystem_path(&operation, "Mount", &asked, None)
            .await
            .expect("the path must resolve below the base");
        assert_eq!(resolved.as_str(), "Deeper/leaf.txt");
    }

    /// A path is resolved a component at a time, so an ancestor in the wrong case has to be
    /// corrected as well as the leaf.
    #[tokio::test]
    async fn path_resolves_every_component_to_its_stored_case_variation() {
        let dir = temp_dir();
        let operation = os_operation(dir.path()).await;
        if !case_insensitive(dir.path()) {
            return;
        }
        let nested = dir.path().join("Assets").join("Meshes");
        std::fs::create_dir_all(&nested).expect("create dirs");
        std::fs::write(nested.join("Rock.mesh"), b"").expect("write file");

        let asked = std::str::FromStr::from_str("assets/MESHES/rock.MESH")
            .expect("relative path is infallible");
        let resolved = filesystem_path(&operation, "", &asked, None)
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
        let operation = os_operation(dir.path()).await;
        let nested = dir.path().join("Assets").join("Meshes");
        std::fs::create_dir_all(&nested).expect("create dirs");
        std::fs::write(nested.join("Rock.mesh"), b"").expect("write file");

        // Shallowest first, as the caller has them.
        let shared = depth_paths(&["assets", "assets/meshes"]);
        let prefixes = resolve_prefixes(&operation, dir.path(), &shared).await;

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
            filesystem_path_and_info(&operation, "", &asked, Some(&prefixes))
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
        let operation = os_operation(dir.path()).await;
        std::fs::create_dir(dir.path().join("Assets")).expect("create dir");

        let shared = depth_paths(&["absent", "absent/deeper"]);
        let prefixes = resolve_prefixes(&operation, dir.path(), &shared).await;
        assert!(prefixes.is_empty(), "nothing there resolves");

        // The path still resolves through the walk, which reports it as missing
        // in the same way it would without a map.
        let asked: RelativePath =
            std::str::FromStr::from_str("absent/file").expect("relative path");
        assert!(
            filesystem_path(&operation, "", &asked, Some(&prefixes))
                .await
                .is_err()
        );

        if case_insensitive(dir.path()) {
            return;
        }
        std::fs::create_dir(dir.path().join("assets")).expect("create second variation");
        let shared = depth_paths(&["ASSETS"]);
        assert!(
            resolve_prefixes(&operation, dir.path(), &shared)
                .await
                .is_empty(),
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
        let operation = os_operation(dir.path()).await;
        std::fs::create_dir(dir.path().join("Assets")).expect("create dir");
        std::fs::write(dir.path().join("Assets").join("rock.mesh"), b"").expect("write file");

        let prefixes = resolve_prefixes(&operation, dir.path(), &depth_paths(&["Assets"])).await;
        assert_eq!(prefixes.longest_prefix_of("Assets"), Some((1, "Assets")));

        std::fs::rename(dir.path().join("Assets"), dir.path().join("ASSETS")).expect("rename");

        let asked: RelativePath =
            std::str::FromStr::from_str("Assets/rock.mesh").expect("relative path");
        let afresh = filesystem_path(&operation, "", &asked, None).await.ok();
        assert_eq!(
            afresh.as_ref().map(RelativePath::as_str),
            Some("ASSETS/rock.mesh"),
            "resolving afresh finds the directory under the name it now has"
        );
        let mapped = filesystem_path(&operation, "", &asked, Some(&prefixes))
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
        let operation = os_operation(dir.path()).await;
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
        let prefixes = resolve_prefixes(&operation, dir.path(), &shared).await;

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
                filesystem_path(&operation, "", &asked, Some(&prefixes))
                    .await
                    .ok(),
                filesystem_path(&operation, "", &asked, None).await.ok(),
                "{asked} must resolve the same with the map as without it"
            );
        }
    }

    const EXEC: u16 = NodeFileMode::Executable.bits();

    #[test]
    fn an_observed_bit_replaces_the_stored_one() {
        assert_eq!(EXEC, mode_from_observed(true, Some(true), 0));
        assert_eq!(0, mode_from_observed(true, Some(false), EXEC));
    }

    #[test]
    fn an_unobserved_bit_keeps_the_stored_one() {
        assert_eq!(EXEC, mode_from_observed(true, None, EXEC));
        assert_eq!(0, mode_from_observed(true, None, 0));
    }

    #[test]
    fn only_a_file_carries_a_mode() {
        assert_eq!(0, mode_from_observed(false, Some(true), EXEC));
        assert_eq!(0, mode_from_observed(false, None, EXEC));
    }
}
