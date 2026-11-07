// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Holding a target revision, for a provider that serves the working tree from a `State` it
//! holds ([`FilesystemProvider::holds_state`]) and is handed the target
//! ([`InstanceOperationImpl::hold_state`]). Three States take part: the current one, the revision
//! the held State stands at, read clean; the held one, that revision with the local changes the
//! provider serves; and the target. [`changes`] diffs the current State against the target,
//! [`check`] refuses a change that would replace a local one, [`apply`] carries the held State to
//! the target in place, keeping local changes, and [`report`] reports what the hold changes as a
//! sync reports what it realizes. What the provider keeps beside the held State, the content of
//! its local files, [`apply`] changes through [`HoldHost`].
//!
//! [`FilesystemProvider::holds_state`]: crate::fs::filesystem_provider::FilesystemProvider::holds_state
//! [`InstanceOperationImpl::hold_state`]: crate::fs::filesystem_provider::InstanceOperationImpl::hold_state

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_base::error::InvalidArguments;
use lore_base::types::Address;
use lore_error_set::prelude::*;

use crate::BoxFuture;
use crate::change::FileAction;
use crate::change::Flags;
use crate::change::NodeChange;
use crate::change::NodeChangeState;
use crate::errors::LocalModifications;
use crate::errors::SlowDown;
use crate::event;
use crate::filter::FilterMode;
use crate::fs::filesystem_provider::FsError;
use crate::fs::filesystem_provider::HoldOptions;
use crate::hash::hash_string;
use crate::lore_error;
use crate::lore_info;
use crate::node::INVALID_NODE;
use crate::node::Node;
use crate::node::NodeExtFlags;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::ROOT_NODE;
use crate::repository::RepositoryContext;
use crate::revision::sync::LoreRevisionSyncFileEventData;
use crate::revision::sync::LoreRevisionSyncProgressEventData;
use crate::revision::sync::SyncRealizeStats;
use crate::state;
use crate::state::AttachCheck;
use crate::state::AttachOutcome;
use crate::state::Content;
use crate::state::Detach;
use crate::state::Rewrite;
use crate::state::State;
use crate::state::StateError;
use crate::state::TreeAttach;
use crate::state::TreeBuild;
use crate::util::path::RelativePath;

/// The revision a swap moves from, as [`changes`] read it under the held State's repository: a
/// settle after the swap spells paths against it. `None` where the revision does not move, and
/// the target spells.
pub type Before = Option<Arc<State>>;

/// The changes that carry `held`, a State at a revision with its local changes, to `target`: the
/// unfiltered diff from that revision, read clean, so neither a view nor the local changes decide
/// it, with each path of `unsettled`, which an incomplete apply changed, applied again. Sorted by
/// path. With them, that revision's State, read clean, where the revision moves.
pub async fn changes(
    held: (&Arc<RepositoryContext>, &Arc<State>),
    target: (&Arc<RepositoryContext>, &Arc<State>),
    unsettled: &BTreeMap<String, bool>,
) -> Result<(Vec<NodeChange>, Before), StateError> {
    let revision = held.1.revision();
    let moves = target.1.revision() != revision;
    if !moves && unsettled.is_empty() {
        return Ok((Vec::new(), None));
    }
    let (mut changes, before) = if moves {
        let current = State::deserialize(held.0.clone(), revision).await?;
        let changes = state::diff_collect(
            held.0.clone(),
            current.clone(),
            target.0.clone(),
            target.1.clone(),
            None,
            FilterMode::empty(),
        )
        .await?;
        (changes, Some(current))
    } else {
        (Vec::new(), None)
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
    Ok((changes, before))
}

/// What carrying `held`, a State at a revision with its local changes, to `target` changes in what
/// each view shows: the diff from that revision, read clean under `held`'s view, to `target` under
/// its own, so a path leaving the view is a deletion and a path entering it an addition, as an
/// ordinary sync changing the view realizes them. `before` is that revision's State where
/// [`changes`] read it.
pub async fn shown_changes(
    held: (&Arc<RepositoryContext>, &Arc<State>),
    target: (&Arc<RepositoryContext>, &Arc<State>),
    before: &Before,
) -> Result<Vec<NodeChange>, StateError> {
    let current = match before {
        Some(current) => current.clone(),
        None => State::deserialize(held.0.clone(), held.1.revision()).await?,
    };
    state::diff_collect(
        held.0.clone(),
        current,
        target.0.clone(),
        target.1.clone(),
        None,
        FilterMode::View,
    )
    .await
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
/// slots, a detached child reading back `Discarded` and counting as absent, and by attaching
/// nodes, a directory an apply attaches being recorded by its path.
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

/// The local files whose content is what a change of a hold replaces or brings, by path, with the
/// address and size of what each held: losing them loses nothing, so an apply gives them the
/// target's content, or removes them, as a forced hold does, where they still hold it.
pub type Matching = BTreeMap<String, (Address, u64)>;

/// Refuses `changes` where one would remove or replace a local change in the held State `paths`
/// resolves, unless `options` keeps or overwrites local changes, which skips the check. A local
/// deletion refuses nothing. A local edit of a tracked file is compared, through `host`, with what
/// the change replaces and with what it brings, and one holding either loses nothing: it is
/// answered in what matches. The held State marks the ancestors of each local change dirty, as
/// `State::node_mark` does.
pub async fn check<H: HoldHost>(
    (host, paths): (&H, &mut Resolver<'_>),
    changes: &[NodeChange],
    options: HoldOptions,
) -> Result<Matching, FsError> {
    let mut matching = Matching::new();
    if options.overwrite_local || options.keep_local || changes.is_empty() {
        return Ok(matching);
    }
    lore_info!("Verifying {} changes with the held state", changes.len());
    let checked = async {
        if !paths.root_holds_local().await? {
            return Ok(Vec::new());
        }
        let mut refused = Vec::new();
        for change in changes {
            refused.extend(replaces_local((host, &mut *paths), change, &mut matching).await?);
        }
        Ok::<_, StateError>(refused)
    };
    let refused = checked
        .await
        .forward_any::<FsError>("checking the held State for local changes")?;
    if refused.is_empty() {
        return Ok(matching);
    }
    for path in refused {
        lore_error!("{path} has local changes the target revision replaces");
    }
    Err(LocalModifications.into())
}

/// The path at which `change` removes or replaces a local change in the held State `paths`
/// resolves: a local file or link it removes or replaces, a directory it removes holding a local
/// change other than an addition, which keeps the directory, a directory it replaces with a file or
/// link holding any local change, or a local file or link standing above the path. A local edit
/// of a tracked file holding what the change removes or brings there is none, and goes to
/// `matching`, as does one below a directory the change removes holding what the revision holds
/// at its path.
async fn replaces_local<'c, H: HoldHost>(
    (host, paths): (&H, &mut Resolver<'_>),
    change: &'c NodeChange,
    matching: &mut Matching,
) -> Result<Option<&'c RelativePath>, StateError> {
    let removed = match change.action {
        FileAction::Delete => Some(change.path()),
        _ => change.move_source(),
    };
    if let Some(path) = removed {
        let replaced = match paths.at(path).await? {
            Some((node, record)) if record.is_directory() && change.from.flags.is_directory() => {
                Box::pin(removal_loses(
                    (host, &mut *paths),
                    (node, &record, path),
                    &change.from.mapping,
                    matching,
                ))
                .await?
            }
            Some((node, record)) => {
                paths.holds_local_change(node, &record, false).await?
                    && !holds_side(host, path, &record, [Some(&change.from), None], matching)
                        .await?
            }
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
        Some((node, record)) => {
            // What stands at the path in the revision is what the change replaces only where it
            // keeps the path; a move or a copy brings a node to a path that held another.
            let replaced = (change.action == FileAction::Keep).then_some(&change.from);
            paths.holds_local_change(node, &record, true).await?
                && !holds_side(host, path, &record, [replaced, Some(&change.to)], matching).await?
        }
        None => paths.below_local(path).await?,
    };
    Ok(replaced.then_some(path))
}

/// Whether `record`, the held State's at `path`, is a local edit of a tracked file whose content
/// is the content one of `sides` holds there, a file; noted in `matching` where it is. A file added
/// locally or a link is never compared: an apply that loses nothing keeps both.
async fn holds_side<H: HoldHost>(
    host: &H,
    path: &RelativePath,
    record: &Node,
    sides: [Option<&NodeChangeState>; 2],
    matching: &mut Matching,
) -> Result<bool, StateError> {
    if !record.is_file()
        || record.is_dirty_add()
        || record.ext_flags().contains(NodeExtFlags::Symlink)
    {
        return Ok(false);
    }
    let established = lore_storage::ContentHashes::default();
    for side in sides.into_iter().flatten().filter(|side| is_file(side)) {
        let (address, size) = {
            let mapping = &side.mapping;
            let node = mapping
                .state
                .node(mapping.repository.clone(), mapping.node)
                .await?;
            if node.ext_flags().contains(NodeExtFlags::Symlink) {
                continue;
            }
            (node.address, node.size)
        };
        // Boxed: a cold path, whose comparison would otherwise sit in every check's future.
        if Box::pin(host.holds_content(path, address, size, &established))
            .await
            .forward_any::<StateError>("comparing a local file with a side of a change")?
        {
            matching.insert(path.as_str().to_string(), (address, size));
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether removing directory `node`, whose record is `record`, at `path`, loses a local change,
/// as [`Resolver::holds_local_change`] answers without additions, apart from a local edit of a
/// tracked file holding what the revision holds at its path, `from` being the directory's side
/// there: removing it loses nothing, as a forced hold removes it, and it goes to `matching`. A
/// file renamed only in case is one. Only dirty directories are read.
async fn removal_loses<H: HoldHost>(
    (host, paths): (&H, &mut Resolver<'_>),
    (node, record, path): (NodeID, &Node, &RelativePath),
    from: &state::NodeMapping,
    matching: &mut Matching,
) -> Result<bool, StateError> {
    let walked = |held: &Node| held.is_directory() && held.is_dirty() && !held.is_dirty_add();
    if !walked(record) {
        return Ok(false);
    }
    let established = lore_storage::ContentHashes::default();
    let mut pending = vec![(node, path.clone(), Some(from.node))];
    while let Some((dir, dir_path, revision_dir)) = pending.pop() {
        let count = paths.index(dir).await?.len();
        for at in 0..count {
            let child = paths.children[&dir][at].1;
            let held = paths.node(child).await?;
            if held.is_discarded() || held.parent != dir {
                continue;
            }
            let edited = !held.is_directory() && is_local(&held) && !held.is_dirty_add();
            if !edited && !walked(&held) {
                continue;
            }
            let name = paths
                .state
                .node_name_clone(paths.repository.clone(), child)
                .await?;
            let child_path = dir_path.join(&name);
            let revision = match revision_dir {
                Some(revision_dir) => revision_child(from, revision_dir, &name).await?,
                None => None,
            };
            if !edited {
                let below = revision.filter(|(_, node)| node.is_directory());
                pending.push((child, child_path, below.map(|(found, _)| found)));
                continue;
            }
            let tracked =
                |node: &Node| node.is_file() && !node.ext_flags().contains(NodeExtFlags::Symlink);
            let Some((_, revision)) = revision.filter(|(_, node)| tracked(node)) else {
                return Ok(true);
            };
            if !tracked(&held) {
                return Ok(true);
            }
            // Boxed: a cold path, whose comparison would otherwise sit in every check's future.
            if !Box::pin(host.holds_content(
                &child_path,
                revision.address,
                revision.size,
                &established,
            ))
            .await
            .forward_any::<StateError>("comparing a local file below a removed directory")?
            {
                return Ok(true);
            }
            matching.insert(
                child_path.as_str().to_string(),
                (revision.address, revision.size),
            );
        }
    }
    Ok(false)
}

/// The child of directory `dir` in `side`'s State whose name folds to `name`, with its record;
/// `None` where it holds none.
async fn revision_child(
    side: &state::NodeMapping,
    dir: NodeID,
    name: &str,
) -> Result<Option<(NodeID, Node)>, StateError> {
    let found = match side
        .state
        .find_subnode(side.repository.clone(), dir, hash_string(name))
        .await
    {
        Ok(found) => found,
        Err(error) if error.is_node_not_found() => return Ok(None),
        Err(error) => return Err(error),
    };
    let node = side.state.node(side.repository.clone(), found).await?;
    Ok((!node.is_discarded()).then_some((found, node)))
}

/// Reports each change of `changes` as a sync reports the changes it realizes: the totals once
/// discovered, an event for each change, carrying the size the target holds, then the totals
/// again with the work done. A change a hold keeping local changes leaves local is reported too.
pub async fn report(changes: &[NodeChange]) -> Result<(), StateError> {
    let stats = Arc::new(SyncRealizeStats::default());
    // What each change carries, read once for the totals the discovery reports first.
    let mut carried = Vec::with_capacity(changes.len());
    for change in changes {
        let (size, file) = if change.action == FileAction::Delete {
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
            }
            (node.size, node.is_file())
        };
        carried.push((size, file));
    }
    stats.discovery.complete.store(true, Ordering::Relaxed);
    event::LoreEvent::RevisionSyncProgress(LoreRevisionSyncProgressEventData::new(&stats)).send();
    for (change, &(size, file)) in changes.iter().zip(&carried) {
        if change.action == FileAction::Delete {
            stats.complete.file_delete.fetch_add(1, Ordering::Relaxed);
        } else if file {
            stats.complete.file_update.fetch_add(1, Ordering::Relaxed);
            stats
                .complete
                .bytes_update
                .fetch_add(size, Ordering::Relaxed);
        }
        event::LoreEvent::RevisionSyncFile(LoreRevisionSyncFileEventData::new(change, size, file))
            .send();
    }
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
    node.ext_flags().contains(NodeExtFlags::LocalContent)
        || (node.is_dirty() && !node.is_dirty_delete())
}

/// What a provider keeps beside the `State` it holds, which [`apply`] changes as it changes the
/// State. Paths are spelled as the State spells them.
pub trait HoldHost: Sync {
    /// Removes what the provider keeps below the directory at `path`, which an apply detached
    /// holding nothing local.
    fn remove_tree(&self, path: &RelativePath);

    /// Removes the content of the local file at `path`, which a forced hold detached.
    fn remove_file(&self, path: &RelativePath);

    /// Gives local file `node`, at `path` and held at `place`, its parent and name hash, the
    /// content `wanted` holds, as a forced hold does: the node takes it clean and its local
    /// content goes. Answers whether the node changed; one moved meanwhile is left as it stands.
    fn overwrite_local(
        &self,
        path: &RelativePath,
        node: NodeID,
        place: (NodeID, u64),
        wanted: &Node,
    ) -> impl Future<Output = Result<bool, FsError>> + Send;

    /// Whether the local file at `path` holds the content `address`, of `size` bytes, addresses.
    /// `established` carries what earlier comparisons of the same file settled.
    fn holds_content(
        &self,
        path: &RelativePath,
        address: Address,
        size: u64,
        established: &lore_storage::ContentHashes,
    ) -> impl Future<Output = Result<bool, FsError>> + Send;
}

/// Where in an apply a test's [`SwapHook`] runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwapPhase {
    /// Between removing and placing.
    Removed,
    /// Before each copy of a node the target adds attaches it.
    Copy,
    /// After a copy published its top node, with everything below it.
    Attached,
    /// Before each node a copy adds below its top node, which is not published yet.
    Fill,
    /// After a copy or a restore found its name held, before it reads the node holding it.
    Held,
    /// Before each attempt of a restore to attach a directory it restores.
    Restore,
    /// After placing, before the apply ends.
    Placed,
    /// Before a removal detaches a node it read.
    Detach,
    /// Before a file the target changes takes the target's content in place.
    Modify,
    /// Once a completed apply has compared a directory's children with the target's, before it
    /// settles the directory clean.
    Settle,
    /// Before a re-placement's exact fill-in of a directory gives a file the target's content in
    /// place.
    Refill,
}

/// A test's hook into an apply, run at each [`SwapPhase`]; `false` fails the apply there.
pub type SwapHook = Arc<dyn Fn(SwapPhase) -> BoxFuture<'static, bool> + Send + Sync>;

/// Applies `changes`, the unfiltered diff from the revision the held State `paths` resolves
/// stands at to the target, to that State in place: removed paths and move sources first,
/// parents before children, then placed paths the same way, the provider's local content beside
/// the State changed through `host`. A local change is never lost unless `options` overwrites
/// local changes: a file or directory holding local content stays where the target removes or
/// changes it, and a path the revision holds but the State does not, a local deletion, is brought
/// back only where the target changes the file and `options` keeps no local change. Records each
/// path it changes in `changed`, true where the next apply should add it back, and the State
/// reads `paths` made in `reads`. Nothing below a directory it copied is placed again. A
/// directory at a path of `unsettled`, which an earlier apply that did not complete changed, is
/// made to match the target whole; each path below it that this changes goes to `reconciled`,
/// and each local file a forced hold overwrote to `overwritten`. Each local file of `matching`,
/// which [`check`] found holding what a change replaces or brings, is overwritten or removed as a
/// forced hold does. `hook`, a test's, runs after each pass and around each copy; `false` fails
/// the apply there.
pub async fn apply<H: HoldHost>(
    (host, paths): (&H, Resolver<'_>),
    changes: &[NodeChange],
    (options, matching): (HoldOptions, &Matching),
    changed: &mut BTreeMap<String, bool>,
    reads: &mut u64,
    (unsettled, reconciled, overwritten): (
        &BTreeMap<String, bool>,
        &mut Vec<RelativePath>,
        &mut Overwritten,
    ),
    hook: Option<SwapHook>,
) -> Result<(), StateError> {
    let mut dirs = Directories::new((host, paths), (options, matching), changed, unsettled, hook);
    let result = apply_passes(&mut dirs, changes).await;
    *reads = dirs.paths.reads();
    reconciled.append(&mut dirs.reconciled);
    overwritten.append(&mut dirs.overwritten);
    result
}

async fn apply_passes<H: HoldHost>(
    dirs: &mut Directories<'_, H>,
    changes: &[NodeChange],
) -> Result<(), StateError> {
    for change in changes {
        let removed = match change.action {
            FileAction::Delete => Some(change.path()),
            FileAction::Move => change.move_source(),
            _ => None,
        };
        if let Some(path) = removed {
            dirs.remove(path).await?;
        }
    }
    dirs.phase(SwapPhase::Removed).await?;
    for change in changes {
        let path = change.path();
        let absent = match change.action {
            FileAction::Delete => continue,
            FileAction::Keep
                if !dirs.options.keep_local
                    && is_file(&change.from)
                    && is_file(&change.to)
                    && !dirs.unsettled.contains_key(path.as_str()) =>
            {
                Absent::Restore
            }
            FileAction::Keep => Absent::Leave,
            FileAction::Add | FileAction::Move | FileAction::Copy | FileAction::Graft => {
                Absent::Add
            }
        };
        if path.is_empty() || dirs.below_copied(path) {
            continue;
        }
        let parent = match dirs.paths.dir(&path.parent_path()).await? {
            Some(parent) => parent,
            None if absent == Absent::Restore => {
                let Some(parent) = dirs
                    .restore_directories(&path.parent_path(), &change.to.mapping)
                    .await?
                else {
                    // Nothing to restore into: the next apply restores nothing here either, as
                    // where a user's removal took what this one restored.
                    dirs.record(path, false);
                    continue;
                };
                parent
            }
            None => continue,
        };
        let side = &change.to.mapping;
        dirs.paths.reads += 1;
        let wanted = side.state.node(side.repository.clone(), side.node).await?;
        dirs.place(parent, path, &wanted, side, absent).await?;
    }
    dirs.phase(SwapPhase::Placed).await?;
    Ok(())
}

/// The local files a forced hold overwrote, as the State spelled them, each true where it
/// detached the file.
pub type Overwritten = Vec<(RelativePath, bool)>;

/// How many times a copy tries its name where a node found holding it left meanwhile.
const COPY_ATTEMPTS: usize = 4;

/// What a change asks for a path where nothing stands in the held State.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Absent {
    /// Nothing: the revision holds the path, so the user deleted it.
    Leave,
    /// The target adds the path.
    Add,
    /// The target changes a file the user deleted, which a hold that keeps no local change
    /// restores.
    Restore,
}

/// An apply under way: the held State's paths it resolves, and what it changed so far.
struct Directories<'a, H> {
    host: &'a H,
    options: HoldOptions,
    /// The local files a forced hold's overwrite loses nothing of ([`check`]).
    matching: &'a Matching,
    paths: Resolver<'a>,
    /// Each path changed so far, true where it may lack what the next apply should add back:
    /// what stood there went, or a copy there has not landed whole.
    changed: &'a mut BTreeMap<String, bool>,
    /// The directories this apply attached whole, by path; nothing below them is placed again.
    copied: BTreeSet<String>,
    /// The paths an earlier apply that did not complete changed; a directory there is made to
    /// match the target whole.
    unsettled: &'a BTreeMap<String, bool>,
    /// The paths below such a directory that making it match changed, and the files restored.
    reconciled: Vec<RelativePath>,
    /// The local files a forced hold overwrote.
    overwritten: Overwritten,
    hook: Option<SwapHook>,
}

impl<'a, H: HoldHost> Directories<'a, H> {
    fn new(
        (host, paths): (&'a H, Resolver<'a>),
        (options, matching): (HoldOptions, &'a Matching),
        changed: &'a mut BTreeMap<String, bool>,
        unsettled: &'a BTreeMap<String, bool>,
        hook: Option<SwapHook>,
    ) -> Self {
        Self {
            host,
            options,
            matching,
            paths,
            changed,
            copied: BTreeSet::new(),
            unsettled,
            reconciled: Vec::new(),
            overwritten: Vec::new(),
            hook,
        }
    }

    /// Runs the test hook at `phase`, failing the apply where it answers false.
    async fn phase(&self, phase: SwapPhase) -> Result<(), StateError> {
        match &self.hook {
            Some(hook) if !hook(phase).await => Err(StateError::internal(format!(
                "the swap hook failed the apply at {phase:?}"
            ))),
            _ => Ok(()),
        }
    }

    /// Whether the local file at `path` is overwritten, or removed, as a forced hold does: where
    /// the hold overwrites local changes, or the file still holds what [`check`] found it holding
    /// what the change replaces or brings, measured again here, so an edit landing since is kept.
    async fn overwrites(&self, path: &RelativePath) -> Result<bool, StateError> {
        if self.options.overwrite_local {
            return Ok(true);
        }
        let Some(&(address, size)) = self.matching.get(path.as_str()) else {
            return Ok(false);
        };
        // Boxed: a cold path, whose comparison would otherwise sit in every apply's future.
        Box::pin(self.host.holds_content(
            path,
            address,
            size,
            &lore_storage::ContentHashes::default(),
        ))
        .await
        .forward_any::<StateError>("comparing a local file again before overwriting it")
    }

    /// Records a change at `path`, `adds_back` where the next apply should add it back.
    fn record(&mut self, path: &RelativePath, adds_back: bool) {
        // Looked up first: an insert over a path recorded already would allocate a key it drops.
        match self.changed.get_mut(path.as_str()) {
            Some(recorded) => *recorded = adds_back,
            None => {
                self.changed.insert(path.as_str().to_string(), adds_back);
            }
        }
    }

    /// Records directory `dir`, which a user's removal took, and each directory above it as kept
    /// where this apply, or an earlier one that did not complete, recorded it to add back: the
    /// next apply adds none of them back where it finds them gone, and refills one that stands
    /// exactly whatever its record.
    fn keep_removed(&mut self, dir: &RelativePath) {
        let mut dir = dir.clone();
        while !dir.is_empty() {
            let adds_back = match self.changed.get(dir.as_str()) {
                Some(&recorded) => recorded,
                None => self.unsettled.get(dir.as_str()) == Some(&true),
            };
            if adds_back {
                self.record(&dir, false);
            }
            dir = dir.parent_path();
        }
    }

    /// Whether `path` stands below a directory this apply attached whole.
    fn below_copied(&self, path: &RelativePath) -> bool {
        let path = path.as_str();
        path.match_indices('/')
            .any(|(at, _)| self.copied.contains(&path[..at]))
    }

    /// Copies `node` to `path` below `parent`, recorded as one the next apply adds back until
    /// the copy lands whole. A live node found at the name, at its place, stays: a directory
    /// there where `node` is one, as a user's MKDIR or RENAME racing the copy puts there, takes
    /// what it lacks of the target's, but nothing of what it holds changes, as it is the user's,
    /// and it is not recorded as copied, so the changes below it are placed too (H23); below
    /// anything else nothing is placed. A holder removed or moved meanwhile leaves the name free,
    /// and the copy tries again, at most [`COPY_ATTEMPTS`] times, then [`SlowDown`]. A `parent`
    /// removed meanwhile, a user's removal, is the user's deletion: nothing is copied, and the
    /// next apply adds back neither `path` nor `parent`, nor any directory above it this apply or
    /// an earlier incomplete one recorded to add back (H11).
    /// Answers the node standing at `path` where the copy or a directory found there stands, for
    /// the resolver to take.
    async fn copy(
        &mut self,
        parent: NodeID,
        path: &RelativePath,
        node: &Node,
        side: &state::NodeMapping,
    ) -> Result<Option<NodeID>, StateError> {
        self.record(path, true);
        // A counter, not a range, which the future would hold across every await.
        let mut attempts = 0u8;
        loop {
            if attempts as usize == COPY_ATTEMPTS {
                return Err(StateError::from(SlowDown));
            }
            attempts += 1;
            let holder = match copy_subtree(self, parent, path.name(), node, side).await? {
                Copied::Attached(copied) => {
                    self.record(path, false);
                    if node.is_directory() {
                        self.copied.insert(path.as_str().to_string());
                    }
                    return Ok(Some(copied));
                }
                Copied::Exists(holder) => holder,
                Copied::ParentGone => {
                    self.record(path, false);
                    self.keep_removed(&path.parent_path());
                    return Ok(None);
                }
            };
            self.phase(SwapPhase::Held).await?;
            let held = self.paths.node(holder).await?;
            if held.is_discarded() || held.parent != parent || held.name_hash != node.name_hash {
                continue;
            }
            if node.is_directory() {
                if held.is_directory() {
                    // The holder is the user's: recorded kept before the merge, so one that fails
                    // midway leaves no add of the target's directory over a removal of it, and a
                    // holder that stands is refilled exactly either way.
                    self.record(path, false);
                    Box::pin(fill_in(self, holder, side, path, false)).await?;
                    return Ok(Some(holder));
                }
                self.copied.insert(path.as_str().to_string());
            }
            self.record(path, false);
            return Ok(None);
        }
    }

    /// Attaches each directory on `path` that the State lacks, a user deleted it, as the target
    /// `side` holds it, so a file the target changes below it is restored as an ordinary sync
    /// restores it. Each attached directory is published dirty, its parent marked first: it holds
    /// only what the hold restores into it. None is recorded: a replay restores one again only for
    /// a change below it, which the diff gives again, where a record would have it refilled
    /// exactly, or added whole, bringing back what the user deleted below it that the target does
    /// not change. A directory found at a level's name, at its place, takes what lies
    /// below it; a holder removed or moved meanwhile leaves the name free, and the level is tried
    /// again, at most [`COPY_ATTEMPTS`] times, then [`SlowDown`]. Answers the directory at
    /// `path`; `None` where the target holds none there, a user's file holds a level's name, or a
    /// user's removal took a level, which, with each directory above it, the next apply adds back
    /// no more (H11).
    async fn restore_directories(
        &mut self,
        path: &RelativePath,
        side: &state::NodeMapping,
    ) -> Result<Option<NodeID>, StateError> {
        let mut dir = ROOT_NODE;
        let mut at = RelativePath::new();
        for name in path.as_str().split('/').filter(|name| !name.is_empty()) {
            at = at.join(name);
            if let Some(found) = self.paths.dir(&at).await? {
                dir = found;
                continue;
            }
            let wanted = match side
                .state
                .find_relative_node_link(side.repository.clone(), ROOT_NODE, at.as_str())
                .await
            {
                Ok(link) if link.revision == side.state.revision() => link.node,
                Ok(_) => return Ok(None),
                Err(error) if error.is_node_not_found() => return Ok(None),
                Err(error) => return Err(error),
            };
            self.paths.reads += 1;
            // Published dirty, its parent marked first where it is not, as a served create
            // publishes: no settle may take it for the target's directory, since it holds only
            // what the hold restores into it, and no moment leaves it published clean (C11c).
            // The target's record in a block of its own, so the attempts below hold only the copy.
            let restored = {
                let node = side.state.node(side.repository.clone(), wanted).await?;
                if !node.is_directory() {
                    return Ok(None);
                }
                let restored = copy_of(&node);
                Node {
                    flags: restored.flags | NodeFlags::Dirty.bits(),
                    ..restored
                }
            };
            let mut attempts = 0u8;
            let attached = loop {
                if attempts as usize == COPY_ATTEMPTS {
                    return Err(StateError::from(SlowDown));
                }
                attempts += 1;
                self.phase(SwapPhase::Restore).await?;
                let outcome = match self
                    .paths
                    .state
                    .served()
                    .node_attach(
                        self.paths.repository.clone(),
                        dir,
                        restored,
                        name,
                        AttachCheck::Walk,
                    )
                    .await
                {
                    Ok(outcome) => outcome,
                    // `dir` removed meanwhile, a user's removal: the user's deletion (H11).
                    Err(error) if error.is_node_not_found() => {
                        self.keep_removed(&at.parent_path());
                        return Ok(None);
                    }
                    Err(error) => return Err(error),
                };
                let holder = match outcome {
                    Some(AttachOutcome::Attached(attached)) => break attached,
                    Some(AttachOutcome::Exists(holder)) => holder,
                    None => return Err(unloaded_attach()),
                };
                self.phase(SwapPhase::Held).await?;
                // The record read in a block, so no later await holds it.
                let (moved, directory) = {
                    let held = self.paths.node(holder).await?;
                    (
                        held.is_discarded()
                            || held.parent != dir
                            || held.name_hash != restored.name_hash,
                        held.is_directory(),
                    )
                };
                if moved {
                    continue;
                }
                // The user's file at the name stays, and nothing is restored below it.
                if !directory {
                    return Ok(None);
                }
                match self
                    .paths
                    .state
                    .served()
                    .node_mark(self.paths.repository, holder)
                    .await
                {
                    Ok(()) => break holder,
                    // The holder removed meanwhile: as above.
                    Err(error) if error.is_node_not_found() => {
                        self.keep_removed(&at);
                        return Ok(None);
                    }
                    Err(error) => return Err(error),
                }
            };
            self.paths.note_attached(&at, attached, true);
            dir = attached;
        }
        Ok(Some(dir))
    }

    /// Removes what stands at `path`, unless it holds local content: a file with local content
    /// stays, and a directory holding some keeps it and loses everything else.
    async fn remove(&mut self, path: &RelativePath) -> Result<(), StateError> {
        let Some((node, held)) = self.paths.at(path).await? else {
            return Ok(());
        };
        if Box::pin(self.remove_node(node, held, path)).await? {
            self.paths.note_detached(path);
        }
        Ok(())
    }

    /// Detaches `node`, whose record is `held`, at `path`, and answers true unless it holds
    /// local content, in which case only the rest of it goes. A directory that is not dirty
    /// holds none, since local content dirties every ancestor, so nothing below it is read.
    async fn remove_node(
        &mut self,
        node: NodeID,
        held: Node,
        path: &RelativePath,
    ) -> Result<bool, StateError> {
        let local = is_local(&held);
        if !local {
            let spelled = if held.is_file() {
                None
            } else {
                Some(self.paths.spelled(node, path).await)
            };
            self.phase(SwapPhase::Detach).await?;
            if detach_if(
                self.paths.state,
                self.paths.repository,
                node,
                Detach::IfClean,
            )
            .await?
            {
                self.record(path, true);
                if let Some(spelled) = spelled {
                    self.host.remove_tree(&spelled);
                }
                return Ok(true);
            }
        }
        if held.is_file() {
            // A forced hold overwrites a local edit of a file the revision holds, never a file
            // added locally, which an ordinary clone keeps too.
            if !(local
                && !held.is_dirty_add()
                && !held.ext_flags().contains(NodeExtFlags::Symlink)
                && self.overwrites(path).await?)
            {
                return Ok(false);
            }
            let spelled = self.paths.spelled(node, path).await;
            self.phase(SwapPhase::Detach).await?;
            match self
                .paths
                .state
                .swap()
                .node_detach(
                    self.paths.repository.clone(),
                    node,
                    Detach::At(held.parent, held.name_hash),
                )
                .await
            {
                Ok(true) => {}
                // Moved out meanwhile, with its local content: gone from here, kept where it went.
                Ok(false) => return Ok(true),
                Err(error) if error.is_node_not_found() => return Ok(true),
                Err(error) => return Err(error),
            }
            self.host.remove_file(&spelled);
            self.record(path, true);
            self.overwritten.push((spelled, true));
            return Ok(true);
        }
        self.paths.children.remove(&node);
        let mut kept = false;
        for child in self.paths.children_of(node).await? {
            let child_node = self.paths.node(child).await?;
            let name = self
                .paths
                .state
                .node_name_clone(self.paths.repository.clone(), child)
                .await?;
            let child_path = path.join(&name);
            kept |= !Box::pin(self.remove_node(child, child_node, &child_path)).await?;
        }
        if kept || held.ext_flags().contains(NodeExtFlags::LocalContent) {
            return Ok(false);
        }
        let spelled = self.paths.spelled(node, path).await;
        if detach_if(
            self.paths.state,
            self.paths.repository,
            node,
            Detach::IfEmpty,
        )
        .await?
        {
            self.record(path, true);
            self.host.remove_tree(&spelled);
            return Ok(true);
        }
        Ok(false)
    }

    /// Sets what stands at `path` in directory `parent` to `wanted`, node `side.node` of
    /// `side.state`. A file of either side holding local content stays as it is, unless the
    /// hold overwrites local changes, which gives a local file the target's content. A tracked
    /// file takes the target's content in place, and a node of another kind is replaced by a
    /// copy. Where nothing stands, the path is added where the change adds it or restores it
    /// (`absent`): a path the revision holds and the State does not is a local deletion, which
    /// only a change of the file restores. A directory the change adds over a directory takes
    /// whatever of the target's subtree it lacks.
    async fn place(
        &mut self,
        parent: NodeID,
        path: &RelativePath,
        wanted: &Node,
        side: &state::NodeMapping,
        absent: Absent,
    ) -> Result<(), StateError> {
        let name = path.name();
        let added = absent != Absent::Leave;
        let exact = self.unsettled.contains_key(path.as_str());
        let Some((existing, held)) = self.paths.child(parent, name).await? else {
            if added {
                if let Some(node) = self.copy(parent, path, wanted, side).await? {
                    self.paths.note_attached(path, node, wanted.is_directory());
                }
                if absent == Absent::Restore {
                    self.reconciled.push(path.clone());
                }
            }
            return Ok(());
        };
        // Measured before the match, whose guards cannot wait.
        let overwrite =
            held.is_file() && wanted.is_file() && is_local(&held) && self.overwrites(path).await?;
        match (held.is_file(), wanted.is_file()) {
            (true, true) if !is_local(&held) => {
                self.record(path, false);
                self.phase(SwapPhase::Modify).await?;
                // The content and the answer temporaries of this statement, so the future holds
                // them only here.
                let refused = match self
                    .paths
                    .state
                    .node_rewrite(
                        self.paths.repository,
                        existing,
                        &Content {
                            mode: wanted.mode,
                            size: wanted.size,
                            address: wanted.address,
                        },
                        Rewrite::IfClean,
                    )
                    .await
                {
                    // Made local meanwhile: local content stays.
                    Ok(_) => return Ok(()),
                    Err(error) => error,
                };
                if !self.paths.node(existing).await?.is_discarded() {
                    return Err(refused);
                }
                drop(refused);
                // Removed meanwhile, as a local deletion: absent, as where nothing stood.
                if added && let Some(node) = self.copy(parent, path, wanted, side).await? {
                    self.paths.note_attached(path, node, wanted.is_directory());
                }
                Ok(())
            }
            (true, true) if overwrite => {
                self.record(path, false);
                self.phase(SwapPhase::Modify).await?;
                let spelled = self.paths.spelled(existing, path).await;
                let overwritten = self
                    .host
                    .overwrite_local(&spelled, existing, (parent, held.name_hash), wanted)
                    .await
                    .map_err(|error| {
                        StateError::internal(format!(
                            "overwriting '{path}' for a forced sync: {error}"
                        ))
                    })?;
                if overwritten {
                    self.overwritten.push((spelled, false));
                }
                Ok(())
            }
            (false, false) if added || exact => {
                Box::pin(fill_in(self, existing, side, path, exact)).await
            }
            (true, true) | (false, false) => Ok(()),
            _ => {
                if Box::pin(self.remove_node(existing, held, path)).await? {
                    self.paths.note_detached(path);
                    if let Some(node) = self.copy(parent, path, wanted, side).await? {
                        self.paths.note_attached(path, node, wanted.is_directory());
                    }
                }
                Ok(())
            }
        }
    }
}

impl Resolver<'_> {
    async fn children_of(&mut self, node: NodeID) -> Result<Vec<NodeID>, StateError> {
        let children = self
            .state
            .node_children(self.repository.clone(), node)
            .await?;
        self.reads += children.len() as u64;
        Ok(children)
    }

    /// Records that `path`, if a directory, is `node`.
    fn note_attached(&mut self, path: &RelativePath, node: NodeID, directory: bool) {
        if directory {
            self.by_path.insert(path.as_str().to_string(), Some(node));
        }
    }

    /// Records that no directory stands at `path` any more, nor below it.
    fn note_detached(&mut self, path: &RelativePath) {
        let below = format!("{path}/");
        let stale: Vec<String> = self
            .by_path
            .range(below.clone()..)
            .take_while(|(key, _)| key.starts_with(&below))
            .map(|(key, _)| key.clone())
            .collect();
        for key in stale {
            self.by_path.remove(&key);
        }
        self.by_path.insert(path.as_str().to_string(), None);
    }

    /// The path of `node`, at `path`, as the State spells it, which the host goes by; `path`
    /// where it cannot be read.
    async fn spelled(&self, node: NodeID, path: &RelativePath) -> RelativePath {
        self.state
            .node_path(self.repository.clone(), node)
            .await
            .ok()
            .and_then(|spelled| RelativePath::new_from_initial_path(&spelled).ok())
            .filter(|spelled| spelled.as_lowercase_str() == path.as_lowercase_str())
            .unwrap_or_else(|| path.clone())
    }
}

/// Adds to directory `dir`, at `path`, whatever of `side`'s directory it lacks, descending into
/// the directories both hold. Where `exact`, it also makes what both hold match `side`: a child
/// `side` lacks goes unless it holds local content, a file takes `side`'s content unless it
/// holds local content, and a child of the other kind is replaced; each path so changed is
/// noted in `reconciled`. A child a served removal takes while its content is replaced is absent,
/// as where nothing stood.
async fn fill_in<H: HoldHost>(
    dirs: &mut Directories<'_, H>,
    dir: NodeID,
    side: &state::NodeMapping,
    path: &RelativePath,
    exact: bool,
) -> Result<(), StateError> {
    let mut held: Vec<(u64, NodeID)> = dirs
        .paths
        .state
        .node_children_with_name_hash(dirs.paths.repository.clone(), dir)
        .await?
        .into_iter()
        .map(|(node, name_hash)| (name_hash, node))
        .collect();
    dirs.paths.reads += held.len() as u64;
    held.sort_unstable_by_key(|&(name_hash, _)| name_hash);
    let (from, from_repository) = (&side.state, &side.repository);
    let mut wanted_hashes: Vec<u64> = Vec::new();
    for child in from
        .node_children(from_repository.clone(), side.node)
        .await?
    {
        dirs.paths.reads += 1;
        let wanted = from.node(from_repository.clone(), child).await?;
        if exact {
            wanted_hashes.push(wanted.name_hash);
        }
        let child_side = state::NodeMapping {
            repository: from_repository.clone(),
            state: from.clone(),
            path: RelativePath::new(),
            node: child,
        };
        let Ok(at) = held.binary_search_by_key(&wanted.name_hash, |&(name_hash, _)| name_hash)
        else {
            // Each name a temporary of its statement, so no later await holds it.
            let child_path =
                path.join(&from.node_name_clone(from_repository.clone(), child).await?);
            dirs.copy(dir, &child_path, &wanted, &child_side).await?;
            if exact {
                dirs.reconciled.push(child_path);
            }
            continue;
        };
        let existing = held[at].1;
        // The record read in a block, so no later await holds it.
        let (existing_directory, keeps_content) = {
            let existing_node = dirs.paths.node(existing).await?;
            (
                existing_node.is_directory(),
                is_local(&existing_node)
                    || (existing_node.address.hash == wanted.address.hash
                        && existing_node.mode == wanted.mode),
            )
        };
        match (existing_directory, wanted.is_directory()) {
            (true, true) => {
                let child_path =
                    path.join(&from.node_name_clone(from_repository.clone(), child).await?);
                Box::pin(fill_in(dirs, existing, &child_side, &child_path, exact)).await?;
            }
            _ if !exact => {}
            (false, false) => {
                if keeps_content {
                    continue;
                }
                let child_path =
                    path.join(&from.node_name_clone(from_repository.clone(), child).await?);
                dirs.record(&child_path, false);
                dirs.phase(SwapPhase::Refill).await?;
                // Under the block lock the rewrite takes: a copy-up since the check stays local.
                // The content and the answer temporaries of this statement, so the future holds
                // them only here.
                let refused = match dirs
                    .paths
                    .state
                    .node_rewrite(
                        dirs.paths.repository,
                        existing,
                        &Content {
                            mode: wanted.mode,
                            size: wanted.size,
                            address: wanted.address,
                        },
                        Rewrite::IfClean,
                    )
                    .await
                {
                    Ok(true) => {
                        dirs.reconciled.push(child_path);
                        continue;
                    }
                    // Made local meanwhile: local content stays.
                    Ok(false) => continue,
                    Err(error) => error,
                };
                // Removed meanwhile, as a local deletion: absent, as where nothing stood.
                if !dirs.paths.node(existing).await?.is_discarded() {
                    return Err(refused);
                }
                drop(refused);
                dirs.copy(dir, &child_path, &wanted, &child_side).await?;
                dirs.reconciled.push(child_path);
            }
            _ => {
                let child_path =
                    path.join(&from.node_name_clone(from_repository.clone(), child).await?);
                // Read again, as the record read above is not held this far.
                let existing_node = dirs.paths.node(existing).await?;
                if Box::pin(dirs.remove_node(existing, existing_node, &child_path)).await? {
                    dirs.copy(dir, &child_path, &wanted, &child_side).await?;
                }
                dirs.reconciled.push(child_path);
            }
        }
    }
    if !exact {
        return Ok(());
    }
    wanted_hashes.sort_unstable();
    for &(name_hash, node) in &held {
        if wanted_hashes.binary_search(&name_hash).is_ok() {
            continue;
        }
        let record = dirs.paths.node(node).await?;
        if record.is_discarded() || record.parent != dir {
            continue;
        }
        let name = dirs
            .paths
            .state
            .node_name_clone(dirs.paths.repository.clone(), node)
            .await?;
        let child_path = path.join(&name);
        Box::pin(dirs.remove_node(node, record, &child_path)).await?;
        dirs.reconciled.push(child_path);
    }
    Ok(())
}

/// What a copy found at its name.
enum Copied {
    /// The copy stands under this id, whole.
    Attached(NodeID),
    /// A live child of the directory held the name: this one. Nothing was copied.
    Exists(NodeID),
    /// The directory was removed, as a user's removal removes it. Nothing was copied.
    ParentGone,
}

/// Attaches a copy of `node`, named `name` and node `side.node` of `side.state`, below
/// `parent`, with every node below it, built out of every chain and published whole
/// ([`SwapNodes::node_attach_tree`]): no request reaches the copy, or adds to it, before it is
/// complete (H14, H22). The nodes below go into directories the build made, so they are added
/// without a name lookup.
///
/// [`SwapNodes::node_attach_tree`]: crate::state::SwapNodes::node_attach_tree
async fn copy_subtree<H: HoldHost>(
    dirs: &mut Directories<'_, H>,
    parent: NodeID,
    name: &str,
    node: &Node,
    side: &state::NodeMapping,
) -> Result<Copied, StateError> {
    dirs.phase(SwapPhase::Copy).await?;
    let (state, repository) = (dirs.paths.state, dirs.paths.repository);
    let mut build = match state
        .swap()
        .node_attach_tree(repository.clone(), parent, &copy_of(node), name)
        .await
    {
        Ok(TreeAttach::Building(build)) => build,
        Ok(TreeAttach::Exists(existing)) => return Ok(Copied::Exists(existing)),
        Err(error) if error.is_node_not_found() => return Ok(Copied::ParentGone),
        Err(error) => return Err(error),
    };
    if node.is_directory()
        && let Err(error) = fill(dirs, &mut build, side).await
    {
        Box::pin(build.abandon()).await;
        return Err(error);
    }
    let attached = match build.publish().await {
        Ok(AttachOutcome::Attached(attached)) => attached,
        Ok(AttachOutcome::Exists(existing)) => return Ok(Copied::Exists(existing)),
        Err(error) if error.is_node_not_found() => return Ok(Copied::ParentGone),
        Err(error) => return Err(error),
    };
    dirs.phase(SwapPhase::Attached).await?;
    Ok(Copied::Attached(attached))
}

/// Adds below `build`'s root, a copy of node `side.node` of `side.state`, a copy of every node
/// below that one.
async fn fill<H: HoldHost>(
    dirs: &mut Directories<'_, H>,
    build: &mut TreeBuild<'_>,
    side: &state::NodeMapping,
) -> Result<(), StateError> {
    let mut pending = vec![(build.root(), side.node)];
    let (from, from_repository) = (&side.state, &side.repository);
    while let Some((into, source)) = pending.pop() {
        for child in from.node_children(from_repository.clone(), source).await? {
            dirs.phase(SwapPhase::Fill).await?;
            dirs.paths.reads += 1;
            let child_name = from.node_name_clone(from_repository.clone(), child).await?;
            // The record read in a block, so no later await holds it.
            let (copy, directory) = {
                let child_node = from.node(from_repository.clone(), child).await?;
                (copy_of(&child_node), child_node.is_directory())
            };
            let added = build.add(into, &copy, &child_name).await?;
            if directory {
                pending.push((added, child));
            }
        }
    }
    Ok(())
}

/// The error of an attach that answered nothing, which only an attach that may not load can.
fn unloaded_attach() -> StateError {
    StateError::internal("an attach that may load found nothing to load")
}

/// A node holding what `node` holds: its kind, mode, size and address.
fn copy_of(node: &Node) -> Node {
    Node {
        flags: node.flags & NodeFlags::File.bits(),
        mode: node.mode,
        size: node.size,
        address: node.address,
        name_hash: node.name_hash,
        ..Node::default()
    }
}

/// Detaches `node` when it is as `when` asks, decided under the locks the detach takes, and
/// answers whether it is gone; one already detached is gone. [`Detach::IfClean`] removes a node
/// nothing below holds local content in; [`Detach::IfEmpty`] one with no child left.
async fn detach_if(
    state: &State,
    repository: &Arc<RepositoryContext>,
    node: NodeID,
    when: Detach<'_>,
) -> Result<bool, StateError> {
    match state
        .swap()
        .node_detach(repository.clone(), node, when)
        .await
    {
        Err(error) if error.is_node_not_found() => Ok(true),
        detached => detached,
    }
}
