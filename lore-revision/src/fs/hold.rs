// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Holding a target revision, for a provider that serves the working tree from a `State` it
//! holds ([`FilesystemProvider::holds_state`]) and is handed the target
//! ([`InstanceOperationImpl::hold_state`]). Three States take part: the current one, the revision
//! the held State stands at, read clean; the held one, that revision with the local changes the
//! provider serves; and the target. [`changes`] diffs the current State against the target,
//! [`check`] refuses a change that would replace a local one, and [`report`] reports what the
//! hold changes as a sync reports what it realizes. The provider applies the changes to the State
//! it holds.
//!
//! [`FilesystemProvider::holds_state`]: crate::fs::filesystem_provider::FilesystemProvider::holds_state
//! [`InstanceOperationImpl::hold_state`]: crate::fs::filesystem_provider::InstanceOperationImpl::hold_state

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_base::error::InvalidArguments;
use lore_error_set::prelude::*;

use crate::change::FileAction;
use crate::change::Flags;
use crate::change::NodeChange;
use crate::change::NodeChangeState;
use crate::errors::LocalModifications;
use crate::event;
use crate::filter::FilterMode;
use crate::fs::filesystem_provider::FsError;
use crate::fs::filesystem_provider::HoldOptions;
use crate::hash::hash_string;
use crate::lore_error;
use crate::lore_info;
use crate::node::INVALID_NODE;
use crate::node::Node;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::ROOT_NODE;
use crate::repository::RepositoryContext;
use crate::revision::sync::LoreRevisionSyncFileEventData;
use crate::revision::sync::LoreRevisionSyncProgressEventData;
use crate::revision::sync::SyncRealizeStats;
use crate::state;
use crate::state::State;
use crate::state::StateError;
use crate::util::path::RelativePath;

/// The changes that carry `held`, a State at a revision with its local changes, to `target`: the
/// unfiltered diff from that revision, read clean, so neither a view nor the local changes decide
/// it, with each path of `unsettled`, which an incomplete apply changed, applied again. Sorted by
/// path.
pub async fn changes(
    held: (&Arc<RepositoryContext>, &Arc<State>),
    target: (&Arc<RepositoryContext>, &Arc<State>),
    unsettled: &BTreeMap<String, bool>,
) -> Result<Vec<NodeChange>, StateError> {
    let revision = held.1.revision();
    let moves = target.1.revision() != revision;
    if !moves && unsettled.is_empty() {
        return Ok(Vec::new());
    }
    let mut changes = if moves {
        let current = State::deserialize(held.0.clone(), revision).await?;
        state::diff_collect(
            held.0.clone(),
            current,
            target.0.clone(),
            target.1.clone(),
            None,
            FilterMode::empty(),
        )
        .await?
    } else {
        Vec::new()
    };
    if !unsettled.is_empty() {
        changes.retain(|change| {
            change.move_source().is_some() || !unsettled.contains_key(change.path().as_str())
        });
        for (path, &adds_back) in unsettled {
            let path = RelativePath::new_from_clean_parts(path, "");
            changes.extend(replacement(held, target, &path, adds_back).await?);
        }
        crate::change::sort_by_path(&mut changes);
    }
    Ok(changes)
}

/// The changes of `changes` the view of `repository` shows; a move shows where either side does.
pub fn in_view<'a>(
    repository: &RepositoryContext,
    changes: &'a [NodeChange],
) -> Cow<'a, [NodeChange]> {
    if repository.filter.view.is_empty() {
        return Cow::Borrowed(changes);
    }
    let shows = |path: &RelativePath, side: &NodeChangeState| {
        !repository
            .filter
            .excludes_tree(path, !is_file(side), FilterMode::View)
    };
    Cow::Owned(
        changes
            .iter()
            .filter(|change| {
                shows(change.path(), change.resolved_side())
                    || change
                        .move_source()
                        .is_some_and(|source| shows(source, &change.from))
            })
            .cloned()
            .collect(),
    )
}

/// The held State's paths resolved through an index of each directory's children by name hash,
/// built once per directory however many of its children a hold reads. The index is never edited,
/// so while a resolver lives the State changes only by detaching nodes without reusing their
/// slots: a detached child reads back `Discarded` and counts as absent.
pub struct Resolver<'a> {
    state: &'a State,
    repository: &'a Arc<RepositoryContext>,
    by_path: BTreeMap<String, Option<NodeID>>,
    children: BTreeMap<NodeID, Vec<(u64, NodeID)>>,
    /// Node reads and children walked so far.
    reads: u64,
}

impl<'a> Resolver<'a> {
    /// Resolves the paths of `state`, the held State, read under `repository`.
    pub fn new((repository, state): (&'a Arc<RepositoryContext>, &'a Arc<State>)) -> Self {
        Self {
            state,
            repository,
            by_path: BTreeMap::new(),
            children: BTreeMap::new(),
            reads: 0,
        }
    }

    async fn node(&mut self, node: NodeID) -> Result<Node, StateError> {
        self.reads += 1;
        self.state.node(self.repository.clone(), node).await
    }

    /// The children of `dir` by name hash, sorted, indexed on the first call.
    async fn index(&mut self, dir: NodeID) -> Result<&[(u64, NodeID)], StateError> {
        if !self.children.contains_key(&dir) {
            let mut index: Vec<(u64, NodeID)> = self
                .state
                .node_children_with_name_hash(self.repository.clone(), dir)
                .await?
                .into_iter()
                .map(|(node, name_hash)| (name_hash, node))
                .collect();
            self.reads += index.len() as u64;
            index.sort_unstable_by_key(|&(name_hash, _)| name_hash);
            self.children.insert(dir, index);
        }
        Ok(self.children.get(&dir).map_or(&[], Vec::as_slice))
    }

    /// The directory at `path`; `None` where there is none. Walks down from the deepest directory
    /// resolved before.
    async fn dir(&mut self, path: &RelativePath) -> Result<Option<NodeID>, StateError> {
        let path = path.as_str();
        let (mut dir, mut start) = (ROOT_NODE, 0);
        for end in std::iter::once(path.len()).chain(path.rmatch_indices('/').map(|(at, _)| at)) {
            match self.by_path.get(&path[..end]) {
                Some(None) => return Ok(None),
                Some(&Some(found)) => {
                    (dir, start) = (found, end + 1);
                    break;
                }
                None => {}
            }
        }
        while start < path.len() {
            let end = path[start..].find('/').map_or(path.len(), |at| start + at);
            let found = match self.child(dir, &path[start..end]).await? {
                Some((node, held)) if held.is_directory() => Some(node),
                _ => None,
            };
            self.by_path.insert(path[..end].to_string(), found);
            let Some(found) = found else {
                return Ok(None);
            };
            (dir, start) = (found, end + 1);
        }
        Ok(Some(dir))
    }

    /// The live child of `dir` named `name`, with its record. Where children share the folded
    /// name, as a local replacement and the node it displaced, or two committed spellings, do, one
    /// holding a local change is taken first, then one not deleted.
    async fn child(
        &mut self,
        dir: NodeID,
        name: &str,
    ) -> Result<Option<(NodeID, Node)>, StateError> {
        let name_hash = hash_string(name);
        let index = self.index(dir).await?;
        let start = index.partition_point(|&(hash, _)| hash < name_hash);
        let end = start + index[start..].partition_point(|&(hash, _)| hash == name_hash);
        let mut found: Option<(NodeID, Node)> = None;
        for at in start..end {
            let node = self.children[&dir][at].1;
            let held = self.node(node).await?;
            if held.is_discarded() || held.parent != dir {
                continue;
            }
            if is_local(&held) {
                return Ok(Some((node, held)));
            }
            if found
                .as_ref()
                .is_none_or(|(_, kept)| kept.is_dirty_delete() && !held.is_dirty_delete())
            {
                found = Some((node, held));
            }
        }
        Ok(found)
    }

    /// What stands at `path`, with its record.
    async fn at(&mut self, path: &RelativePath) -> Result<Option<(NodeID, Node)>, StateError> {
        match self.dir(&path.parent_path()).await? {
            Some(parent) => self.child(parent, path.name()).await,
            None => Ok(None),
        }
    }

    /// Whether a local file or link stands above `path` in place of a directory, which a change at
    /// `path` would replace.
    async fn below_local(&mut self, path: &RelativePath) -> Result<bool, StateError> {
        let parent = path.parent_path();
        if self.dir(&parent).await?.is_some() {
            return Ok(false);
        }
        let parent = parent.as_str();
        let mut dir = ROOT_NODE;
        for end in parent
            .match_indices('/')
            .map(|(at, _)| at)
            .chain(std::iter::once(parent.len()))
        {
            let ancestor = RelativePath::new_from_clean_parts(&parent[..end], "");
            if let Some(found) = self.dir(&ancestor).await? {
                dir = found;
                continue;
            }
            return Ok(self
                .child(dir, ancestor.name())
                .await?
                .is_some_and(|(_, held)| !held.is_directory() && is_local(&held)));
        }
        Ok(false)
    }

    /// Whether a child of the root is dirty. A local change dirties each of its ancestors but the
    /// root, so where none is, the State holds no local change a hold could replace.
    async fn root_holds_local(&mut self) -> Result<bool, StateError> {
        let count = self.index(ROOT_NODE).await?.len();
        for at in 0..count {
            let node = self.children[&ROOT_NODE][at].1;
            let held = self.node(node).await?;
            if !held.is_discarded() && held.parent == ROOT_NODE && held.is_dirty() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether `node`, whose record is `record`, is or holds a local change that removing or
    /// replacing it would lose: a local file or link, or a directory holding one. Without
    /// `additions`, as for a directory a removal keeps for what was added in it, a local addition
    /// counts as none. Only dirty directories are read.
    async fn holds_local_change(
        &mut self,
        node: NodeID,
        record: &Node,
        additions: bool,
    ) -> Result<bool, StateError> {
        let lost = |held: &Node| {
            is_local(held)
                && if held.is_directory() {
                    additions && held.is_dirty_add()
                } else {
                    additions || !held.is_dirty_add()
                }
        };
        let walked = |held: &Node| {
            held.is_directory() && held.is_dirty() && (additions || !held.is_dirty_add())
        };
        if lost(record) {
            return Ok(true);
        }
        if !walked(record) {
            return Ok(false);
        }
        let mut pending = vec![node];
        while let Some(dir) = pending.pop() {
            let count = self.index(dir).await?.len();
            for at in 0..count {
                let child = self.children[&dir][at].1;
                let held = self.node(child).await?;
                if held.is_discarded() || held.parent != dir {
                    continue;
                }
                if lost(&held) {
                    return Ok(true);
                }
                if walked(&held) {
                    pending.push(child);
                }
            }
        }
        Ok(false)
    }

    /// The State reads made so far.
    pub fn reads(&self) -> u64 {
        self.reads
    }
}

/// Refuses `changes` where one would remove or replace a local change in the held State `paths`
/// resolves, unless `options` keeps or overwrites local changes, which skips the check. A local
/// deletion refuses nothing, and no content is compared. The held State marks the ancestors of
/// each local change dirty, as `State::node_mark` does.
pub async fn check(
    paths: &mut Resolver<'_>,
    changes: &[NodeChange],
    options: HoldOptions,
) -> Result<(), FsError> {
    if options.overwrite_local || options.keep_local || changes.is_empty() {
        return Ok(());
    }
    lore_info!("Verifying {} changes with the held state", changes.len());
    let checked = async {
        if !paths.root_holds_local().await? {
            return Ok(Vec::new());
        }
        let mut refused = Vec::new();
        for change in changes {
            refused.extend(replaces_local(paths, change).await?);
        }
        Ok::<_, StateError>(refused)
    };
    let refused = checked
        .await
        .forward_any::<FsError>("checking the held State for local changes")?;
    if refused.is_empty() {
        return Ok(());
    }
    for path in refused {
        lore_error!("{path} has local changes the target revision replaces");
    }
    Err(LocalModifications.into())
}

/// The path at which `change` removes or replaces a local change in the held State `paths`
/// resolves: a local file or link it removes or replaces, a directory it removes holding a local
/// change other than an addition, which keeps the directory, a directory it replaces with a file or
/// link holding any local change, or a local file or link standing above the path.
async fn replaces_local<'c>(
    paths: &mut Resolver<'_>,
    change: &'c NodeChange,
) -> Result<Option<&'c RelativePath>, StateError> {
    let removed = match change.action {
        FileAction::Delete => Some(change.path()),
        _ => change.move_source(),
    };
    if let Some(path) = removed {
        let replaced = match paths.at(path).await? {
            Some((node, record)) => paths.holds_local_change(node, &record, false).await?,
            None => paths.below_local(path).await?,
        };
        if replaced {
            return Ok(Some(path));
        }
    }
    if change.action == FileAction::Delete {
        return Ok(None);
    }
    let path = change.path();
    let replaced = match paths.at(path).await? {
        Some((_, record)) if record.is_directory() && change.to.flags.is_directory() => false,
        Some((node, record)) => paths.holds_local_change(node, &record, true).await?,
        None => paths.below_local(path).await?,
    };
    Ok(replaced.then_some(path))
}

/// Reports each change of `changes` as a sync reports the changes it realizes: an event for each,
/// carrying the size the target holds, then the totals. A change a hold keeping local changes
/// leaves local is reported too.
pub async fn report(changes: &[NodeChange]) -> Result<(), StateError> {
    let stats = Arc::new(SyncRealizeStats::default());
    for change in changes {
        let (size, file) = if change.action == FileAction::Delete {
            stats.complete.file_delete.fetch_add(1, Ordering::Relaxed);
            stats
                .complete
                .file_delete_total
                .fetch_add(1, Ordering::Relaxed);
            (0, is_file(&change.from))
        } else {
            let side = &change.to.mapping;
            let node = side.state.node(side.repository.clone(), side.node).await?;
            if node.is_file() {
                stats.discovery.total_files.fetch_add(1, Ordering::Relaxed);
                stats
                    .discovery
                    .total_bytes
                    .fetch_add(node.size, Ordering::Relaxed);
                stats.complete.file_update.fetch_add(1, Ordering::Relaxed);
                stats
                    .complete
                    .bytes_update
                    .fetch_add(node.size, Ordering::Relaxed);
            }
            (node.size, node.is_file())
        };
        event::LoreEvent::RevisionSyncFile(LoreRevisionSyncFileEventData::new(change, size, file))
            .send();
    }
    stats.discovery.complete.store(true, Ordering::Relaxed);
    event::LoreEvent::RevisionSyncProgress(LoreRevisionSyncProgressEventData::new(&stats)).send();
    Ok(())
}

/// The change that applies `target` again at `path`, which an incomplete apply changed, from what
/// `held` holds there now: an add where the path may lack the target's node (`adds_back`), a keep
/// otherwise, so a path deleted since stays deleted, and a delete where the target holds nothing.
/// `None` where either side reaches `path` through a link.
async fn replacement(
    held: (&Arc<RepositoryContext>, &Arc<State>),
    target: (&Arc<RepositoryContext>, &Arc<State>),
    path: &RelativePath,
    adds_back: bool,
) -> Result<Option<NodeChange>, StateError> {
    let (Some(from), Some(to)) = (side_at(held, path).await?, side_at(target, path).await?) else {
        return Ok(None);
    };
    let action = match (to.mapping.node != INVALID_NODE, adds_back) {
        (true, true) => FileAction::Add,
        (true, false) => FileAction::Keep,
        (false, _) => FileAction::Delete,
    };
    Ok(Some(NodeChange {
        action,
        flags: Flags::empty(),
        from,
        to,
    }))
}

/// The side `state` holds at `path`, one holding nothing where it holds nothing there; `None`
/// where `path` reaches through a link. Any read error but a missing node fails.
async fn side_at(
    (repository, state): (&Arc<RepositoryContext>, &Arc<State>),
    path: &RelativePath,
) -> Result<Option<NodeChangeState>, StateError> {
    let found = state
        .find_relative_node_link(repository.clone(), ROOT_NODE, path.as_str())
        .await;
    let link = match found {
        Ok(link) if link.revision != state.revision() => return Ok(None),
        Ok(link) => Some(link),
        Err(error) if error.is_node_not_found() => None,
        Err(error) => return Err(error),
    };
    let absent = NodeChangeState {
        mapping: state::NodeMapping {
            repository: repository.clone(),
            state: state.clone(),
            path: path.clone(),
            node: INVALID_NODE,
        },
        observed: None,
        flags: NodeFlags::NoFlags,
        address: Default::default(),
        mode: 0,
    };
    let Some(link) = link else {
        return Ok(Some(absent));
    };
    let node = state.node(repository.clone(), link.node).await?;
    Ok(Some(absent.from_child(link.node, &node, path.clone())))
}

/// The refusal of what a provider holding whole revisions cannot do, `what` phrased as a verb.
#[lore_macro::test_pub]
pub(crate) fn refused(what: &str) -> InvalidArguments {
    InvalidArguments {
        reason: format!("Unable to {what} on a provider that holds whole revisions"),
    }
}

/// Whether `side` is a file.
pub(crate) fn is_file(side: &NodeChangeState) -> bool {
    side.flags.contains(NodeFlags::File)
}

/// Whether `node` is or holds a local change: a file a hold overwrites only where forced, or a
/// directory holding one. A local deletion is none: a hold that keeps no local change restores
/// the file.
fn is_local(node: &Node) -> bool {
    node.is_dirty() && !node.is_dirty_delete()
}
