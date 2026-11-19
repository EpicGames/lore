use std::sync::Arc;

use crate::filter::FilterMode;
use crate::fs::filesystem_provider::FilesystemDiffContext;
use crate::fs::swfs::modified_tree::ModifiedTree;
use crate::fs::swfs::paths::at_or_below;
use crate::node::ROOT_NODE;
use crate::state::ChangeSender;
use crate::state::ChangeStream;
use crate::state::FilesystemDiffStats;
use crate::state::StateError;
use crate::state::collect_dirty_paths;
use crate::state::diff_paths;
use crate::state::emit;
use crate::state::os_diff::diff_os_filesystem;
use crate::util::path::RelativePath;
use crate::util::path::RelativePathBuf;

/// Whether `diff` is one the modified-file record can answer for.
///
/// A diff of the whole tree, which the record covers, that leaves ignored paths out, as the record
/// does, and that does not reach across a layer, whose paths the states compared hold nothing of.
/// One across a link is answered: the record holds what SWFS reports below a link as below any
/// directory, and each path is diffed through the link's pin in the states compared, which is the
/// pin the mount serves (ADR-00036). The links a dirty marker leads into are followed by the dirty
/// paths' walk.
pub fn answers(diff: &FilesystemDiffContext) -> bool {
    diff.filesystem_path.is_empty()
        && diff.from.node == ROOT_NODE
        && diff.layer_mounts.is_empty()
        && diff.filter_mode.contains(FilterMode::Ignore)
}

/// The changes of `diff`, a diff of the whole tree, found by diffing the paths `record` holds
/// rather than walking the tree.
///
/// The operation rebuilt the record as it began where it was untrusted, and fails where it could
/// not, so the record holds every change up to the freeze. One a callback distrusts since is diffed
/// as it stands: what the callback could not record came after the freeze, which nothing the
/// operation reads holds, and the next operation rebuilds the record from SWFS's own list.
pub fn changes_from_record(
    record: Arc<ModifiedTree>,
    diff: FilesystemDiffContext,
) -> ChangeStream<FilesystemDiffStats> {
    ChangeStream::spawn(move |changes| diff_from_record(record, diff, changes))
}

async fn diff_from_record(
    record: Arc<ModifiedTree>,
    diff: FilesystemDiffContext,
    changes: ChangeSender,
) -> Result<FilesystemDiffStats, StateError> {
    // The record holds differences from its revision, which a scan measures the working tree
    // against directly or through the staged state built on it. A diff from another revision, such
    // as the one a forced sync measures the working tree against, differs wherever the two
    // revisions do, which the record does not hold, so it walks the tree.
    if !measured_from_record_base(&record, &diff).await {
        return diff_os_filesystem(diff, &changes).await;
    }
    let mut candidates = record.paths().await?;
    // What the state compared against holds dirty, such as a path `lore dirty` marked or an
    // earlier scan found changed, may since have been restored, which leaves nothing for the
    // record to hold. A scan settles those as well.
    collect_dirty_paths(
        diff.from.state.clone(),
        diff.from.repository.clone(),
        diff.from.node,
        RelativePathBuf::new(),
        &mut candidates,
    )
    .await?;
    // A path at or below a link's mount path is diffed from the directory holding the link: the
    // walk crosses a link from there, keeping the links nested in it and reporting a link added
    // or removed, which a diff started inside the link or at its node does not. A scan whose
    // changes all lie outside the links reads nothing of their trees.
    let candidates: Vec<RelativePath> = with_ancestors(candidates)
        .into_iter()
        .map(|path| {
            match diff
                .link_mounts
                .iter()
                .find(|mount| at_or_below(&path, &mount.mount_path))
            {
                Some(mount) => mount.mount_path.parent_path(),
                None => path,
            }
        })
        .collect();
    let mut listed = diff_paths(diff, candidates);
    loop {
        let Some(change) = listed.next().await else {
            break;
        };
        emit(&changes, || change).await?;
    }
    listed.finish().await
}

/// `paths` and the directories leading to them.
///
/// A directory the revision lacks can be missing from `paths` though it is changed:
/// - [`ModifiedTree::paths`] and [`collect_dirty_paths`] leave out a created directory that holds
///   a node, for the paths below it to stand for it.
/// - The record holds a directory only as the way to a change below it where SWFS reported that
///   change and not the directory's creation.
///
/// Naming each such directory has the diff report it added on every scan, not only on one that
/// finds it missing from the state it compares against. One the revision holds unchanged yields
/// no change.
#[lore_macro::test_pub]
fn with_ancestors(mut paths: Vec<RelativePath>) -> Vec<RelativePath> {
    let mut named: std::collections::HashSet<String> =
        paths.iter().map(|path| path.as_str().to_owned()).collect();
    let mut ancestors = Vec::new();
    for path in &paths {
        let mut parent = path.parent_path();
        while !parent.is_empty() && named.insert(parent.as_str().to_owned()) {
            let next = parent.parent_path();
            ancestors.push(parent);
            parent = next;
        }
    }
    paths.extend(ancestors);
    paths
}

/// Whether `diff` measures the working tree from the revision the record holds the differences
/// from, or from the staged state built on it.
async fn measured_from_record_base(record: &ModifiedTree, diff: &FilesystemDiffContext) -> bool {
    let from = diff.from.state.revision();
    if from == record.revision().unwrap_or_default() {
        return true;
    }
    matches!(
        crate::instance::load_staged_revision(&diff.from.repository).await,
        Ok(Some(staged)) if staged == from
    )
}
