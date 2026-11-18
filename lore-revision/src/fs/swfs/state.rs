use std::collections::BTreeSet;
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

/// Whether the mount holds something at `path`, asked with a permit from the bound on syscalls on a
/// mount (ADR-00035).
async fn mount_holds(path: &Path) -> bool {
    let _permit = MountIo::global().permit().await;
    lore_io::IoDriver::global().metadata(path).await.is_ok()
}

/// Builds the SWFS files describing `written_paths` and the directories leading to them, as
/// `served` serves them: below a link or layer, from the repository mounted there.
///
/// A path `served` serves nothing at was removed, and is left out unless the mount still holds
/// something there: an entry naming it reads to SWFS as a file that exists, an empty one, so one
/// SWFS holds deleted already (`held_deleted`), or that the mount holds nothing at, is not named.
/// A link or layer is described as the directory it mounts. Directory entries are stamped with
/// `directory_time`. Returns each path named other than as a directory, which takes the source's
/// state and comes off SWFS's list of what it holds modified.
pub async fn written_files(
    files: &mut Vec<SwfsFile>,
    served: &ServedTree,
    mount_path: &Path,
    held_deleted: &DashSet<RelativePathKey>,
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
        while !path.is_empty() && paths.insert(path.to_owned()) {
            path = path.rsplit_once('/').map_or("", |(parent, _)| parent);
        }
    }

    for path in paths {
        let relative = RelativePath::new_from_initial_path(&path)
            .forward::<SwfsWorkError>("Parsing written path")?;
        let node = if removed_below_mounts.contains(&RelativePathKey(relative.clone())) {
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
        if node.is_none()
            && (held_deleted.contains(&RelativePathKey(relative.clone()))
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
