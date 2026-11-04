// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fs::Metadata;
use std::future::Future;
#[cfg(target_family = "unix")]
use std::os::unix::fs::PermissionsExt;
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
use crate::lore_warn;
use crate::node::NodeFileMode;
use crate::repository::DOT_LORE;
use crate::repository::TEMP_FILE_EXTENSION;
use crate::repository::is_reserved_node_name;
use crate::util::time::Retry;
use crate::util::time::RetryPolicy;

const FILE_MODE_USER_EXEC: u32 = 0o100;
#[cfg(not(target_family = "windows"))]
const FILE_MODE_ALL_EXEC: u32 = 0o111;

/// As [`set_executable`], logging a failure as a warning rather than returning it.
pub async fn metadata_set_executable(
    path: impl AsRef<Path>,
    metadata: Option<&Metadata>,
    executable: bool,
) {
    let path = path.as_ref();
    if let Err(err) = set_executable(path, metadata, executable).await {
        lore_warn!(
            "Failed to set the executable bit of {}: {err}",
            path.display()
        );
    }
}

/// Does nothing: Windows has no executable bit.
#[cfg(target_family = "windows")]
pub async fn set_executable(
    _path: impl AsRef<Path>,
    _metadata: Option<&Metadata>,
    _executable: bool,
) -> std::io::Result<()> {
    Ok(())
}

/// Gives the file at `path` the executable bit `executable` asks for, for user, group and
/// others, where the user's bit differs from it: the bit a revision stores, read as
/// [`file_is_executable`] reads it. The mode comes from `metadata`, or from a stat of `path`
/// where `metadata` is `None`.
#[cfg(not(target_family = "windows"))]
pub async fn set_executable(
    path: impl AsRef<Path>,
    metadata: Option<&Metadata>,
    executable: bool,
) -> std::io::Result<()> {
    let path = path.as_ref();
    let permissions = match metadata {
        Some(metadata) => executable_permissions(metadata, executable),
        None => executable_permissions(
            &lore_io::IoDriver::global().metadata(path).await?,
            executable,
        ),
    };
    match permissions {
        Some(permissions) => {
            lore_io::IoDriver::global()
                .set_permissions(path, permissions)
                .await
        }
        None => Ok(()),
    }
}

/// The permissions that give a file with `metadata` the executable bit `executable` asks for,
/// for user, group and others, or `None` where the user's bit already matches.
#[cfg(not(target_family = "windows"))]
fn executable_permissions(metadata: &Metadata, executable: bool) -> Option<std::fs::Permissions> {
    if file_is_executable(metadata) == executable {
        return None;
    }
    let mut permissions = metadata.permissions();
    let mode = permissions.mode();
    permissions.set_mode(if executable {
        mode | FILE_MODE_ALL_EXEC
    } else {
        mode & !FILE_MODE_ALL_EXEC
    });
    Some(permissions)
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
    modified_millis(metadata.modified().ok())
}

/// Milliseconds from the Unix epoch to `modified`: to now where there is no time, and zero
/// for a time before the epoch.
pub fn modified_millis(modified: Option<std::time::SystemTime>) -> u64 {
    modified
        .unwrap_or_else(std::time::SystemTime::now)
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Whether the permission bits `mode` carry the user's executable bit, `None` where there are
/// no bits to read.
pub fn mode_executable_observed(mode: Option<u32>) -> Option<bool> {
    mode.map(|mode| mode & FILE_MODE_USER_EXEC != 0)
}

#[cfg(target_family = "windows")]
pub fn file_is_executable(_metadata: &Metadata) -> bool {
    false
}

#[cfg(target_family = "unix")]
pub fn file_is_executable(metadata: &Metadata) -> bool {
    (metadata.permissions().mode() & FILE_MODE_USER_EXEC) != 0
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

    #[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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

/// Removes the file or empty directory at `absolute_path`, a symbolic link as a link, retrying
/// once where [`Removal::retries`] says a retry can succeed, and answers `Ok` for a path that does
/// not exist or cannot be stat'd.
pub fn unlink<P: AsRef<Path>>(absolute_path: P) -> impl Future<Output = tokio::io::Result<()>> {
    unlink_as(absolute_path, Removal::EmptyDirectory)
}

/// Removes the file, or the directory and everything below it, at `absolute_path`, a symbolic
/// link as a link, retrying once where [`Removal::retries`] says a retry can succeed, and answers
/// `Ok` for a path that does not exist or cannot be stat'd.
pub fn unlink_recursive<P: AsRef<Path>>(
    absolute_path: P,
) -> impl Future<Output = tokio::io::Result<()>> {
    unlink_as(absolute_path, Removal::Tree)
}

/// Removes what `absolute_path` names, a directory as `directory` says and anything else as a
/// file, through [`unlink_retrying`].
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn unlink_as<P: AsRef<Path>>(
    absolute_path: P,
    directory: Removal,
) -> impl Future<Output = tokio::io::Result<()>> {
    async move {
        let Some(is_dir) = removal_is_dir(absolute_path.as_ref()).await else {
            return Ok(());
        };
        let removal = if is_dir { directory } else { Removal::File };
        unlink_retrying(absolute_path.as_ref(), removal).await
    }
}

/// Whether the name `absolute_path` is a directory, which decides how a removal removes it, or
/// `None` for a path that does not exist or cannot be stat'd, which a removal takes as nothing to
/// remove. A symbolic link is not followed, so a removal takes the link and leaves what it stands
/// for: on Unix a link is removed as a file, and on Windows a link made to a directory is removed
/// as a directory, which removes the link alone.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn removal_is_dir(absolute_path: &Path) -> impl Future<Output = Option<bool>> + '_ {
    async move {
        lore_trace!("Deleting {}", absolute_path.display());
        match lore_io::IoDriver::global()
            .symlink_stat(absolute_path)
            .await
        {
            Ok(stat) => Some(stat.kind == lore_io::StatKind::Directory),
            Err(err) if err.kind() == tokio::io::ErrorKind::NotFound => {
                lore_trace!("Path to delete does not exist: {}", absolute_path.display());
                None
            }
            Err(err) => {
                lore_debug!(
                    "Stat of path to delete {} failed: {}",
                    absolute_path.display(),
                    err
                );
                None
            }
        }
    }
}

/// What a removal removes, which decides the call that removes it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Removal {
    /// A file.
    File,
    /// An empty directory. One that holds entries is not retried, since no retry can let it be
    /// removed.
    EmptyDirectory,
    /// A directory and everything below it.
    Tree,
}

impl Removal {
    /// Whether a refused removal can succeed after [`make_removable`]: on Windows every removal,
    /// a read-only attribute refusing a directory's, and on Unix only a tree's, since there a
    /// name's own permissions decide nothing but whether a directory's entries can go.
    fn retries(self) -> bool {
        cfg!(target_family = "windows") || self == Removal::Tree
    }
}

/// Removes what `absolute_path` names as `removal` says, retrying once after [`make_removable`]
/// where [`Removal::retries`] says a retry can succeed, and answers `Ok` for a path that is
/// already gone.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn unlink_retrying(
    absolute_path: &Path,
    removal: Removal,
) -> impl Future<Output = tokio::io::Result<()>> + '_ {
    async move {
        let mut retry = false;
        loop {
            if retry {
                make_removable(absolute_path).await;
            }
            let removed = match removal {
                Removal::File => lore_io::IoDriver::global().remove_file(absolute_path).await,
                Removal::EmptyDirectory => {
                    lore_io::IoDriver::global().remove_dir(absolute_path).await
                }
                Removal::Tree => {
                    lore_io::IoDriver::global()
                        .remove_dir_all(absolute_path)
                        .await
                }
            };
            match removed {
                Ok(()) => {
                    lore_trace!("Deleted {}", absolute_path.display());
                    return Ok(());
                }
                Err(err) if err.kind() == tokio::io::ErrorKind::NotFound => {
                    lore_trace!("Path to delete does not exist: {}", absolute_path.display());
                    return Ok(());
                }
                Err(err)
                    if retry
                        || !removal.retries()
                        || (removal == Removal::EmptyDirectory
                            && err.kind() == tokio::io::ErrorKind::DirectoryNotEmpty) =>
                {
                    lore_debug!("Error deleting {}: {}", absolute_path.display(), err);
                    return Err(err);
                }
                Err(err) => {
                    lore_debug!(
                        "Error deleting {}: {} - retry after making it removable",
                        absolute_path.display(),
                        err
                    );
                }
            }
            retry = true;
        }
    }
}

/// Makes the name `absolute_path` itself writable by its owner, for a failed removal to retry, as
/// [`lore_io::IoDriver::make_owner_writable`] does: on Unix a directory gains its owner's write
/// permission, and on Windows the read-only attribute is cleared, a symbolic link never followed.
/// A name that cannot be changed is left as it is.
///
/// Only a failed removal retries, a cold path, so the change is boxed: inline, it would make every
/// removal's future as large as its own.
fn make_removable(absolute_path: &Path) -> crate::BoxFuture<'_, ()> {
    Box::pin(async move {
        let _ = lore_io::IoDriver::global()
            .make_owner_writable(absolute_path)
            .await;
    })
}

/// Removes what `absolute_path` holds as [`unlink_recursive`] does, but keeps each nested
/// repository in it, a directory holding `.lore`, and each entry [`is_reserved_node_name`] answers
/// for, such as `.lore` or `.urc`, with the directories leading to them. A legacy `.urc` is no
/// boundary, as `state::is_nested_repository_root` describes, so the entries beside it go. A failed
/// removal is retried once after [`make_removable`].
pub async fn unlink_recursive_keeping_repositories(absolute_path: &Path) -> tokio::io::Result<()> {
    let driver = lore_io::IoDriver::global();
    let Err(err) = driver
        .remove_dir_all_keeping(absolute_path, DOT_LORE, is_reserved_node_name)
        .await
    else {
        lore_trace!(
            "Deleted {} around nested repositories",
            absolute_path.display()
        );
        return Ok(());
    };
    lore_debug!(
        "Error deleting {}: {} - retry after making it removable",
        absolute_path.display(),
        err
    );
    make_removable(absolute_path).await;
    driver
        .remove_dir_all_keeping(absolute_path, DOT_LORE, is_reserved_node_name)
        .await
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
