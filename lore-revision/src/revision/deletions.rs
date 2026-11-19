// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Where the paths one side of a 3-way diff deleted were last held on that side's history line.
//!
//! Asking whether a conflicting deletion was already merged across needs the revision that
//! deleted the path, which only the line's history can tell. One walk of the line answers every
//! deleted path at once. It compares each revision with the one after it on the line, and only
//! looks below the directories that changed between them.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use futures::StreamExt;
use futures::TryStreamExt;
use lore_base::lore_spawn;
use lore_base::types::Address;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::TryAcquireError;
use tokio::task::JoinSet;

use crate::change::NodeChange;
use crate::find;
use crate::hash;
use crate::lore::*;
use crate::lore_debug;
use crate::lore_trace;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::ROOT_NODE;
use crate::repository::RepositoryContext;
use crate::repository::is_reserved_node_name;
use crate::state::State;
use crate::state::StateError;
use crate::state::Stop;

/// Path lookups kept in flight at once: of the line's first revision, and of one level of a
/// comparison. Each comparison in flight keeps its own, so the walks of a diff keep up to this
/// many times their permits in flight.
const LOOKUPS: usize = 64;

/// Lookups of one revision kept in flight at once, for the paths a comparison could not rule out.
const HELD_LOOKUPS: usize = 16;

/// Where a path was last held on a history line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LastHeld {
    /// The revision after the newest one holding the path, which deleted it. The line's first
    /// revision where that one holds the path itself.
    pub removed_in: Hash,
    /// Revision number of [`Self::removed_in`].
    pub removed_in_number: u64,
    /// Revision number of the newest revision of the line holding the path.
    pub held_number: u64,
    /// The node the path names in that revision, in the line's repository.
    pub node: NodeID,
}

/// For each path the deleting sides of a 3-way diff's conflicts removed, where that side's
/// history line last held it.
#[derive(Debug)]
pub struct Deletions {
    /// The repository whose lines were walked.
    repository: RepositoryId,
    /// For each line walked, by the revision it starts at, the paths asked about and where each
    /// was last held.
    lines: BTreeMap<Hash, BTreeMap<String, Option<LastHeld>>>,
}

impl Deletions {
    /// Walks the line in `repository` of every side that deletes a path of `conflicts`, each once
    /// for all its paths, down to the first revision numbered `bound` or lower.
    ///
    /// A deletion by a side in another repository, below a link, is left out: see
    /// [`Self::last_held`]. Lines are walked at the same time, sharing `permits` revision
    /// comparisons in flight, and the first line that fails stops the others.
    pub async fn walk<'a>(
        conflicts: impl IntoIterator<Item = &'a (NodeChange, NodeChange)>,
        repository: RepositoryId,
        bound: u64,
        permits: usize,
    ) -> Result<Self, StateError> {
        let mut starts: BTreeMap<Hash, LineStart> = BTreeMap::new();
        for change in conflicts
            .into_iter()
            .flat_map(|(source, target)| [source, target])
        {
            let context = &change.to.mapping.repository;
            if change.to.mapping.node.is_valid_node_id() || context.id != repository {
                continue;
            }
            let start = &change.to.mapping.state;
            starts
                .entry(start.revision())
                .or_insert_with(|| LineStart {
                    repository: context.clone(),
                    start: start.clone(),
                    paths: Vec::new(),
                })
                .paths
                .push(change.path().as_str().to_string());
        }

        let shared = LineWalks::new(permits);
        let walks = starts.into_iter().map(|(key, line)| {
            let shared = &shared;
            async move {
                let LineStart {
                    repository,
                    start,
                    paths,
                } = line;
                let found = walk_line(repository, start, paths, bound, shared).await?;
                Ok::<_, StateError>((key, found))
            }
        });
        // Every line ends its comparisons in flight, so that a failed one cancels none of
        // another's.
        let mut lines = BTreeMap::new();
        for walked in futures::future::join_all(walks).await {
            let (key, found) = walked?;
            lines.insert(key, found);
        }
        Ok(Self { repository, lines })
    }

    /// Where the line from `start` in `repository` last held `path`, as [`Self::walk`] found.
    /// `None` where no revision of it down to the bound holds the path.
    ///
    /// A path in another repository, below a link, is answered `None`, as the walk leaves that
    /// repository's lines out. Walking them needs the path spelled from the link's own root, and
    /// a bound in that repository's numbering.
    pub fn last_held(
        &self,
        repository: &RepositoryContext,
        start: &State,
        path: &str,
    ) -> Result<Option<LastHeld>, StateError> {
        if repository.id != self.repository {
            return Ok(None);
        }
        self.lines
            .get(&start.revision())
            .and_then(|line| line.get(path))
            .copied()
            .ok_or_else(|| {
                StateError::internal(format!(
                    "The history line from revision {} was not walked for {path}",
                    start.revision()
                ))
            })
    }
}

/// A line to walk, and the deleted paths to walk it for.
struct LineStart {
    repository: Arc<RepositoryContext>,
    start: Arc<State>,
    paths: Vec<String>,
}

/// What the walks of several lines share: the revision comparisons they may keep in flight
/// between them, the permits of their pagers to request pages ahead, and whether one failed, which
/// stops the others issuing more.
#[lore_macro::test_pub]
struct LineWalks {
    permits: Arc<Semaphore>,
    /// The most comparisons of a line issued and not yet taken in line order.
    window: usize,
    /// The permits of the walks' pagers to request pages ahead, as [`find::lines_ahead`] gives
    /// them.
    lines_ahead: Arc<Semaphore>,
    stopped: AtomicBool,
}

impl LineWalks {
    #[lore_macro::test_pub]
    fn new(permits: usize) -> Self {
        let permits = permits.max(1);
        Self {
            permits: Arc::new(Semaphore::new(permits)),
            window: permits,
            lines_ahead: find::lines_ahead(),
            stopped: AtomicBool::new(false),
        }
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }

    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// A permit to compare two revisions: one free now, else, where `waiting` is false, the first
    /// to come free. `None` where none is free and the caller has comparisons of its own to wait
    /// on, which free one.
    async fn permit(&self, waiting: bool) -> Result<Option<OwnedSemaphorePermit>, StateError> {
        match self.permits.clone().try_acquire_owned() {
            Ok(permit) => Ok(Some(permit)),
            Err(TryAcquireError::NoPermits) if waiting => Ok(None),
            Err(TryAcquireError::NoPermits) => self
                .permits
                .clone()
                .acquire_owned()
                .await
                .map(Some)
                .map_err(permit_failed),
            Err(err) => Err(permit_failed(err)),
        }
    }
}

#[track_caller]
fn permit_failed(err: impl std::error::Error + Send + Sync + 'static) -> StateError {
    StateError::internal_with_context(
        err,
        "Failed to take a permit to compare revisions of a history line",
    )
}

/// A path the walk looks for, and where each of its components ends.
#[derive(Clone, Debug)]
struct Pending {
    path: String,
    ends: Vec<usize>,
}

impl Pending {
    fn new(path: String) -> Self {
        let ends = path
            .match_indices('/')
            .map(|(index, _)| index)
            .chain([path.len()])
            .collect();
        Self { path, ends }
    }

    /// The number of components.
    fn depth(&self) -> usize {
        self.ends.len()
    }

    /// The component at `depth`.
    fn component(&self, depth: usize) -> &str {
        let start = if depth == 0 {
            0
        } else {
            self.ends[depth - 1] + 1
        };
        &self.path[start..self.ends[depth]]
    }

    fn components(&self) -> impl Iterator<Item = &str> {
        (0..self.depth()).map(|depth| self.component(depth))
    }
}

/// What comparing a revision with the one after it on the line found.
struct Comparison {
    /// The newer revision of the two.
    newer: Hash,
    newer_number: u64,
    /// Revision number of the older one.
    older_number: u64,
    /// The pending paths the older revision holds, by index, with the node each names.
    held: Vec<(u32, NodeID)>,
}

/// Results that arrive in any order, taken in the order of their indices from zero.
#[lore_macro::test_pub]
struct InOrder<T> {
    /// The index of the next result to take.
    next: usize,
    waiting: BTreeMap<usize, T>,
}

impl<T> InOrder<T> {
    #[lore_macro::test_pub]
    fn new() -> Self {
        Self {
            next: 0,
            waiting: BTreeMap::new(),
        }
    }

    /// Takes the result at `index`, and gives `each` those now next in order, each once.
    #[lore_macro::test_pub]
    fn take(&mut self, index: usize, result: T, mut each: impl FnMut(T)) {
        if index == self.next {
            each(result);
            self.next += 1;
        } else {
            self.waiting.insert(index, result);
        }
        while let Some(result) = self.waiting.remove(&self.next) {
            each(result);
            self.next += 1;
        }
    }

    /// How many results were answered.
    #[lore_macro::test_pub]
    fn taken(&self) -> usize {
        self.next
    }
}

/// Walks the line from `start` back along self parents for where each of `paths` was last held,
/// down to the first revision numbered `bound` or lower, or to the line's end. `start` is checked
/// too.
///
/// A revision is compared with the one after it, so one is only searched where the two differ
/// above a pending path. A path below a link is not held in `repository`. Revisions the local
/// store lacks are read from the remote by a [`find::LinePager`], which takes its permit to request
/// pages ahead from `shared`. Each comparison in flight holds one of the comparison permits
/// `shared` holds, a line issues at most its window past the comparisons taken, and results are
/// taken in line order, so each path is found in the newest revision holding it. A revision that
/// cannot be read fails the walk once the comparisons in flight end, and stops the other walks
/// sharing `shared` issuing comparisons.
#[lore_macro::test_pub]
async fn walk_line(
    repository: Arc<RepositoryContext>,
    start: Arc<State>,
    paths: Vec<String>,
    bound: u64,
    shared: &LineWalks,
) -> Result<BTreeMap<String, Option<LastHeld>>, StateError> {
    lore_debug!(
        "Walk history line from revision {} -> {} for {} deleted paths, down to revision number {bound}",
        start.revision(),
        start.revision_number(),
        paths.len()
    );

    let mut pending: Vec<Pending> = paths.into_iter().map(Pending::new).collect();
    pending.sort_unstable_by(|a, b| a.components().cmp(b.components()));
    pending.dedup_by(|a, b| a.path == b.path);
    let pending = Arc::new(pending);
    let mut held: Vec<Option<LastHeld>> = futures::stream::iter(0..pending.len())
        .map(|index| {
            let (repository, start, pending) = (repository.clone(), start.clone(), pending.clone());
            async move {
                let node = held_in(&repository, &start, &pending[index].path).await?;
                Ok::<_, StateError>(node.map(|node| LastHeld {
                    removed_in: start.revision(),
                    removed_in_number: start.revision_number(),
                    held_number: start.revision_number(),
                    node,
                }))
            }
        })
        .buffered(LOOKUPS)
        .try_collect()
        .await?;
    let mut live: Arc<[u32]> = (0u32..)
        .zip(&held)
        .filter(|(_, held)| held.is_none())
        .map(|(index, _)| index)
        .collect();

    let mut comparisons: JoinSet<Result<(usize, Comparison), StateError>> = JoinSet::new();
    let mut in_order = InOrder::new();
    let mut issued = 0usize;
    let mut newer = start;
    let mut line_ended = newer.revision_number() <= bound;
    let mut pager = find::LinePager::new(repository.clone(), shared.lines_ahead.clone());
    let mut failure = None;
    loop {
        while !line_ended
            && !live.is_empty()
            && !shared.stopped()
            && issued - in_order.taken() < shared.window
        {
            let permit = match shared.permit(!comparisons.is_empty()).await {
                Ok(Some(permit)) => permit,
                Ok(None) => break,
                Err(err) => {
                    failure = Some(err);
                    line_ended = true;
                    break;
                }
            };
            // Another walk may have failed while this one waited for the permit.
            if shared.stopped() {
                break;
            }
            let parent = newer.parent_self();
            if parent.is_zero() {
                line_ended = true;
                break;
            }
            pager.page_in(parent, newer.revision_number(), bound).await;
            let older = match State::deserialize(repository.clone(), parent).await {
                Ok(older) => older,
                Err(err) => {
                    failure = Some(err);
                    line_ended = true;
                    break;
                }
            };
            line_ended = older.revision_number() <= bound;
            lore_spawn!(
                comparisons,
                compare(
                    issued,
                    repository.clone(),
                    newer,
                    older.clone(),
                    pending.clone(),
                    live.clone(),
                    permit,
                )
            );
            issued += 1;
            newer = older;
        }
        if failure.is_some() {
            shared.stop();
        }

        let Some(joined) = comparisons.join_next().await else {
            break;
        };
        let (index, comparison) = match joined {
            Ok(Ok(done)) => done,
            Ok(Err(err)) => {
                failure.get_or_insert(err);
                line_ended = true;
                shared.stop();
                continue;
            }
            Err(err) => {
                failure.get_or_insert(StateError::internal_with_context(
                    err,
                    "Failed to compare two revisions of a history line",
                ));
                line_ended = true;
                shared.stop();
                continue;
            }
        };
        let mut taken_paths = false;
        in_order.take(index, comparison, |comparison| {
            for (index, node) in comparison.held {
                let last_held = &mut held[index as usize];
                if last_held.is_some() {
                    continue;
                }
                lore_trace!(
                    "{} last held in revision number {}, deleted in {} -> {}",
                    pending[index as usize].path,
                    comparison.older_number,
                    comparison.newer,
                    comparison.newer_number
                );
                *last_held = Some(LastHeld {
                    removed_in: comparison.newer,
                    removed_in_number: comparison.newer_number,
                    held_number: comparison.older_number,
                    node,
                });
                taken_paths = true;
            }
        });
        if taken_paths {
            live = live
                .iter()
                .copied()
                .filter(|&index| held[index as usize].is_none())
                .collect();
        }
    }
    pager.finish().await;

    if let Some(err) = failure {
        return Err(err);
    }

    lore_debug!(
        "Walked {issued} revisions of the history line, {} of {} deleted paths found held",
        held.iter().filter(|held| held.is_some()).count(),
        held.len()
    );
    // Every comparison has ended, so the walk holds the only reference.
    Ok(Arc::unwrap_or_clone(pending)
        .into_iter()
        .zip(held)
        .map(|(pending, held)| (pending.path, held))
        .collect())
}

/// The node `path` names in `state`, where it names one in the state's own tree: a path below a
/// link is not held there, and the link is not read. A path whose lookup fails for any reason but
/// the path's absence fails.
fn held_in<'a>(
    repository: &'a Arc<RepositoryContext>,
    state: &'a State,
    path: &'a str,
) -> impl Future<Output = Result<Option<NodeID>, StateError>> + Send + 'a {
    futures::FutureExt::map(
        state.find_to_link::<true>(repository, ROOT_NODE, path, false),
        |stop| stop.map(|stop| matches!(stop.at, Stop::End).then_some(stop.node)),
    )
}

/// The paths of `pending` at the indices `live` names that `older` holds, where `newer`, the
/// revision after it on the line, holds none of them. Answers with `index`, the comparison's place
/// on the line, and holds `permit` until it ends.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn compare(
    index: usize,
    repository: Arc<RepositoryContext>,
    newer: Arc<State>,
    older: Arc<State>,
    pending: Arc<Vec<Pending>>,
    live: Arc<[u32]>,
    permit: OwnedSemaphorePermit,
) -> impl Future<Output = Result<(usize, Comparison), StateError>> + Send {
    async move {
        let _permit = permit;
        let Candidates { mut held, unsure } =
            candidates(&repository, &newer, &older, &pending, &live).await;
        let resolved: Vec<Option<(u32, NodeID)>> = futures::stream::iter(unsure)
            .map(|candidate| {
                let (repository, older, pending) =
                    (repository.clone(), older.clone(), pending.clone());
                async move {
                    let node =
                        held_in(&repository, &older, &pending[candidate as usize].path).await?;
                    Ok::<_, StateError>(node.map(|node| (candidate, node)))
                }
            })
            .buffered(HELD_LOOKUPS)
            .try_collect()
            .await?;
        held.extend(resolved.into_iter().flatten());
        Ok((
            index,
            Comparison {
                newer: newer.revision(),
                newer_number: newer.revision_number(),
                older_number: older.revision_number(),
                held,
            },
        ))
    }
}

/// A child lookup of the comparison: the component at `depth` of the pending paths at the
/// positions `range` of the live indices, below `older_parent` in the older revision and
/// `newer_parent` in the newer one, where that holds the directory.
struct Lookup {
    older_parent: NodeID,
    newer_parent: Option<NodeID>,
    depth: usize,
    range: Range<usize>,
}

/// What a lookup found below its parent in one revision.
enum Child {
    /// Nothing of the name, so no path below it is held.
    Missing,
    /// A file or a link, which holds no path below it in this repository's tree.
    Leaf(NodeID),
    /// A directory, with the address of the tree below it, or none where it is staged.
    Directory(NodeID, Address),
}

/// The paths a comparison found the older revision may hold.
struct Candidates {
    /// Those it reached a node at, by index, with the node.
    held: Vec<(u32, NodeID)>,
    /// Those below a failed lookup, by index, each to resolve on its own.
    unsure: Vec<u32>,
}

/// The paths of `pending` at the indices `live` names that `older` may hold, where `newer` holds
/// none of them.
///
/// Both revisions are walked down the paths together, a level at a time, from the root. A
/// directory with the same address in both holds the same tree, so where `newer` holds none of the
/// paths below it, `older` holds none either and the walk leaves it. A directory without an
/// address, as the root is, is compared with none, and so is a staged directory, whose address can
/// predate the changes staged below it ([`Child::Directory`]). A path is held where `older` holds
/// a node at it, not held below a link, and unsure where a lookup on the way fails.
async fn candidates(
    repository: &Arc<RepositoryContext>,
    newer: &State,
    older: &State,
    pending: &[Pending],
    live: &[u32],
) -> Candidates {
    let path = |position: usize| &pending[live[position] as usize];
    let mut found = Candidates {
        held: Vec::new(),
        unsure: Vec::new(),
    };
    let mut level = Vec::new();
    groups(pending, live, 0, 0..live.len(), |range| {
        level.push(Lookup {
            older_parent: ROOT_NODE,
            newer_parent: Some(ROOT_NODE),
            depth: 0,
            range,
        });
    });
    while !level.is_empty() {
        let mut next = Vec::new();
        {
            // Each lookup's future is made as the stream reaches it, so that no more than
            // `LOOKUPS` exist at once. The closure takes an index rather than a borrowed lookup,
            // so the stream holds no closure over a borrow of its argument, which a spawned future
            // cannot be proven to hold for every lifetime.
            let level_ref = &level;
            let mut children = std::pin::pin!(
                futures::stream::iter(0..level.len())
                    .map(|position| {
                        let lookup = &level_ref[position];
                        look_up(repository, newer, older, path(lookup.range.start), lookup)
                    })
                    .buffered(LOOKUPS)
            );
            for lookup in &level {
                let Some((older_child, newer_child)) = children.next().await else {
                    break;
                };
                let below = lookup.depth + 1;
                let (older_node, older_address) = match older_child {
                    Ok(Child::Missing) => continue,
                    Err(_) => {
                        found
                            .unsure
                            .extend(lookup.range.clone().map(|position| live[position]));
                        continue;
                    }
                    Ok(Child::Leaf(node)) => (node, None),
                    Ok(Child::Directory(node, address)) => (node, Some(address)),
                };
                let newer_node = match (newer_child, older_address) {
                    (Some(Child::Directory(node, newer_address)), Some(older_address)) => {
                        if !older_address.is_zero() && newer_address == older_address {
                            continue;
                        }
                        Some(node)
                    }
                    _ => None,
                };
                let mut deeper = lookup.range.start;
                while deeper < lookup.range.end && path(deeper).depth() == below {
                    found.held.push((live[deeper], older_node));
                    deeper += 1;
                }
                if older_address.is_some() {
                    groups(pending, live, below, deeper..lookup.range.end, |range| {
                        next.push(Lookup {
                            older_parent: older_node,
                            newer_parent: newer_node,
                            depth: below,
                            range,
                        });
                    });
                }
            }
        }
        level = next;
    }
    found
}

/// Splits the positions `range` of `live`, whose paths of `pending` are sorted by components, into
/// the runs sharing the component at `depth`.
fn groups(
    pending: &[Pending],
    live: &[u32],
    depth: usize,
    range: Range<usize>,
    mut each: impl FnMut(Range<usize>),
) {
    let component = |position: usize| pending[live[position] as usize].component(depth);
    let mut start = range.start;
    while start < range.end {
        let name = component(start);
        let mut end = start + 1;
        while end < range.end && component(end) == name {
            end += 1;
        }
        each(start..end);
        start = end;
    }
}

/// `lookup` in both revisions, for the component of `path` at its depth: always in the older one,
/// and in the newer one where that holds the parent directory and the older one holds a directory
/// of the name, which the newer one's may match.
async fn look_up(
    repository: &Arc<RepositoryContext>,
    newer: &State,
    older: &State,
    path: &Pending,
    lookup: &Lookup,
) -> (Result<Child, StateError>, Option<Child>) {
    let name = path.component(lookup.depth);
    let older_child = child(repository, older, lookup.older_parent, name).await;
    let newer_child = match (&older_child, lookup.newer_parent) {
        (Ok(Child::Directory(..)), Some(parent)) => {
            child(repository, newer, parent, name).await.ok()
        }
        _ => None,
    };
    (older_child, newer_child)
}

/// The child `name` of `parent` in `state`, as a path lookup reaches it: a node not marked deleted
/// preferred, and a name no node may carry missing.
async fn child(
    repository: &Arc<RepositoryContext>,
    state: &State,
    parent: NodeID,
    name: &str,
) -> Result<Child, StateError> {
    if is_reserved_node_name(name) {
        return Ok(Child::Missing);
    }
    let Some((node_id, _staged_delete)) = state
        .find_live_subnode_if_present(repository.clone(), parent, hash::hash_string(name))
        .await?
    else {
        return Ok(Child::Missing);
    };
    let node = state.node(repository.clone(), node_id).await?;
    Ok(if node.is_directory() {
        let address = if node.is_staged() {
            Address::default()
        } else {
            node.address
        };
        Child::Directory(node_id, address)
    } else {
        Child::Leaf(node_id)
    })
}
