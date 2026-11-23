use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;

use dashmap::DashSet;
use lore_error_set::ForwardStrict;
use lore_error_set::WrapInternal;

use crate::fs::filesystem_provider::FileInfo;
use crate::fs::swfs::api_interface::swfs_api::swfs_u64;
use crate::fs::swfs::file::SwfsFile;
use crate::fs::swfs::mount_io::MountIo;
use crate::fs::swfs::mount_resources::SwfsWorkError;
use crate::fs::swfs::paths::RelativePathKey;
use crate::fs::swfs::paths::SwfsPath;
use crate::fs::swfs::paths::self_and_ancestors;
use crate::fs::swfs::served_tree::Served;
use crate::fs::swfs::served_tree::ServedTree;
use crate::state::file_modified_time;
use crate::util::path::RelativePath;

/// What `served` serves at `path`, which is nothing rather than an error where it serves nothing.
pub async fn tracked_file_info(
    served: &ServedTree,
    path: &RelativePath,
) -> Result<FileInfo, SwfsWorkError> {
    match served
        .resolve(path)
        .await
        .internal("Unable to find file node")?
    {
        None => Ok(FileInfo::NotExist),
        Some(Served::Node {
            repository, node, ..
        }) if !node.is_directory() => {
            let mtime = file_modified_time(repository, path).await;
            Ok(FileInfo::from_node_and_mtime(&node, mtime))
        }
        Some(_) => Ok(FileInfo::Directory),
    }
}

/// The removed paths a thaw is told of, by what removed them.
#[derive(Default)]
pub struct ThawRemovals {
    /// Paths not named where the served tree holds nothing there: those SWFS holds deleted, which
    /// the user deleted and no commit took in, the directories the operation removed from the
    /// mount itself, and those a commit deleted where SWFS froze something since, the user's.
    pub held_deleted: HashSet<RelativePathKey>,
    /// What a commit removed that SWFS lists as deleted, spelled as SWFS lists it: the files it
    /// deleted, the deleted directories they lay in, what lay below a directory it deleted whole,
    /// and where the files it took in renamed came from.
    pub committed: HashSet<RelativePathKey>,
}

/// The deletions SWFS lists, each under its lowercase path and spelled as SWFS lists it.
pub type ListedDeletions = HashMap<String, RelativePath>;

/// Notes in `removals` what a commit's thaw names of what the commit removed: each path in
/// `removals.committed`.
///
/// Only a deletion SWFS lists (`listed`) is named, in SWFS's spelling: an entry naming one
/// commits it, and one naming any other path has nothing to clear, so SWFS makes an empty file
/// there. A file the commit removed (`removed`) is named where SWFS lists it, or else by the
/// nearest directory above it SWFS lists, as where the user deleted that directory whole. Every
/// deletion SWFS lists at or below a directory the commit deleted whole (`removed_trees`) is
/// named, as the directory's entry commits nothing below it. A path SWFS `froze` something at
/// since holds the user's, so it goes to `removals.held_deleted` and is not named.
pub fn name_committed_removals(
    removals: &mut ThawRemovals,
    listed: &ListedDeletions,
    removed: impl IntoIterator<Item = RelativePath>,
    removed_trees: &[RelativePath],
    froze: impl Fn(&RelativePath) -> bool,
) {
    let name = |removals: &mut ThawRemovals, listed: &RelativePath| {
        let key = RelativePathKey(listed.clone());
        if froze(listed) {
            removals.held_deleted.insert(key);
        } else {
            removals.committed.insert(key);
        }
    };
    if !removed_trees.is_empty() {
        let trees: HashSet<&str> = removed_trees
            .iter()
            .map(RelativePath::as_lowercase_str)
            .collect();
        for (lowercase, path) in listed {
            if self_and_ancestors(lowercase).any(|at| trees.contains(at)) {
                name(removals, path);
            }
        }
    }
    for path in removed {
        if froze(&path) {
            removals.held_deleted.insert(RelativePathKey(path));
            continue;
        }
        if let Some(listed) =
            self_and_ancestors(path.as_lowercase_str()).find_map(|at| listed.get(at))
        {
            name(removals, listed);
        }
    }
}

/// Whether the mount holds something at `path`, asked with a permit from the bound on syscalls on a
/// mount (ADR-00035).
async fn mount_holds(path: &Path) -> bool {
    let _permit = MountIo::global().permit().await;
    lore_io::IoDriver::global().metadata(path).await.is_ok()
}

/// Builds the SWFS files describing `written_paths` and the directories leading to them, other
/// than to what a commit removed, as `served` serves them: below a link or layer, from the
/// repository mounted there.
///
/// A path `served` serves nothing at was removed, and is left out unless the mount still holds
/// something there: an entry naming it reads to SWFS as a file that exists, an empty one, so one
/// SWFS holds deleted already (`removals.held_deleted`), or that the mount holds nothing at, is not
/// named. A path a commit removed (`removals.committed`) is named where `served` serves nothing
/// there, as the source then holds nothing there either: the entry takes SWFS's deletion off its
/// list, which otherwise pairs with a later rename of the file as where it came from. One
/// `served` serves something at is not named, as the commit did not take in its removal. A link
/// or layer is described as the directory it mounts. Directory entries are stamped with
/// `directory_time`. Returns each path named other than as a directory, which takes the source's
/// state and comes off SWFS's list of what it holds modified.
pub async fn written_files(
    files: &mut Vec<SwfsFile>,
    served: &ServedTree,
    mount_path: &Path,
    removals: &ThawRemovals,
    removed_below_mounts: &DashSet<RelativePathKey>,
    written_paths: &DashSet<RelativePathKey>,
    directory_time: swfs_u64,
) -> Result<Vec<RelativePath>, SwfsWorkError> {
    let mut named = Vec::new();
    // Sorted, so a directory always precedes what it holds, which SWFS needs to see in that
    // order. Built before any await so no `DashSet` shard guard is held across one.
    let mut paths = BTreeSet::new();
    for written in written_paths.iter() {
        let mut path = written.0.as_str();
        // What a commit removed is named alone. It is spelled as SWFS lists it, which may be
        // how a directory above it was spelled before the user renamed it, and an entry naming
        // that directory would give it the spelling back. One the user deleted whole is the
        // user's deletion still, which an entry naming it would commit.
        if removals.committed.contains(&*written) {
            paths.insert(path.to_owned());
            continue;
        }
        while !path.is_empty() && paths.insert(path.to_owned()) {
            path = path.rsplit_once('/').map_or("", |(parent, _)| parent);
        }
    }

    for path in paths {
        let relative = RelativePath::new_from_initial_path(&path)
            .forward::<SwfsWorkError>("Parsing written path")?;
        let key = RelativePathKey(relative.clone());
        let node = if removed_below_mounts.contains(&key) {
            None
        } else {
            served
                .resolve(&relative)
                .await
                .forward::<SwfsWorkError>("Unable to find written node")?
        };
        let file_name = SwfsPath::c_string_from_relative_path(&relative)
            .forward::<SwfsWorkError>("Parsing path")?;
        // `from_node_and_mtime` describes every node as a file, so a directory is named here.
        let info = match &node {
            Some(Served::Node { node, .. }) if !node.is_directory() => {
                FileInfo::from_node_and_mtime(node, 0)
            }
            Some(_) => FileInfo::Directory,
            None => FileInfo::NotExist,
        };
        let committed_removal = removals.committed.contains(&key);
        if committed_removal && node.is_some() {
            continue;
        }
        if node.is_none()
            && !committed_removal
            && (removals.held_deleted.contains(&key)
                || !mount_holds(&mount_path.join(relative.as_str())).await)
        {
            continue;
        }
        let mut file = SwfsFile::new(file_name, &info, None)
            .forward::<SwfsWorkError>("Creating SWFS file struct")?;
        if info.is_dir() {
            file.set_times(directory_time);
        } else {
            named.push(relative);
        }
        files.push(file);
    }

    Ok(named)
}
