// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use bitflags::bitflags;
use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use tokio::sync::Semaphore;
use tokio::task::JoinError;
use tokio::task::JoinSet;

use crate::MAX_CONCURRENT_TREE_TASKS;
use crate::change;
use crate::change::NodeChangeState;
use crate::filter::Filter;
use crate::filter::FilterMode;
use crate::filter::FilterStates;
use crate::filter::query_depth;
use crate::interface::LoreNodeType;
use crate::lore::Address;
use crate::lore::RepositoryId;
use crate::lore_debug;
use crate::lore_drain_tasks;
use crate::lore_trace;
use crate::lore_warn;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::repository::RepositoryContext;
use crate::state::ChangeSender;
use crate::state::NodeMapping;
use crate::state::State;
use crate::state::StateChildrenNodes;
use crate::state::StateError;
use crate::state::StateNamedNode;
use crate::state::add_change;
use crate::state::emit_change;
use crate::state::named_node_sort;
use crate::util::path::RelativePath;

/// Decides whether a source subtree can be adopted whole during a merge.
///
/// Two things must hold. The view must exclude the whole subtree, so nothing in
/// it reaches the working tree. And the target's version of the directory must
/// be byte-identical to the base's, which means the branch changed nothing
/// inside it at any depth.
pub struct GraftOracle {
    repository: Arc<RepositoryContext>,
    state_target: Arc<State>,
    view: Arc<Filter>,
}

impl GraftOracle {
    pub fn new(
        repository: Arc<RepositoryContext>,
        state_target: Arc<State>,
        view: Arc<Filter>,
    ) -> Self {
        Self {
            repository,
            state_target,
            view,
        }
    }

    /// True when `path` in the target tree still matches `base_address` and
    /// holds no uncommitted work.
    async fn adoptable(&self, path: &RelativePath, base_address: Address) -> bool {
        // An out-of-view subtree never reaches the working tree, so merging it
        // is a tree operation only. An in-view subtree keeps the per-file
        // merge, because there the detail is needed on disk.
        //
        // Every descendant must be excluded, not just the directory node.
        if !self.view.excludes_subtree(path, FilterMode::View) {
            return false;
        }
        let Ok(link) = self
            .state_target
            .find_node_link(self.repository.clone(), path.as_str())
            .await
        else {
            return false;
        };
        if !link.is_valid_or_root() {
            return false;
        }
        // `find_node_link` resolves through links. A path in another
        // repository is merged through that link instead.
        if link.repository != self.repository.id {
            return false;
        }
        let Ok(node) = self
            .state_target
            .node(self.repository.clone(), link.node)
            .await
        else {
            return false;
        };
        if node.is_link() || node.is_file() {
            return false;
        }
        // Target's subtree is identical to base's.
        if node.address != base_address {
            return false;
        }
        // Do not adopt over uncommitted work.
        if node.is_staged() || node.is_dirty() {
            return false;
        }
        self.state_target
            .node_has_staged_children(self.repository.clone(), link.node)
            .await
            .is_ok_and(|has_staged| !has_staged)
    }
}

bitflags! {
    /// What holds of a walk's two sides, derived once and carried unchanged to
    /// every step below.
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct DiffFlags: u32 {
        /// The sides filter through different filters, so neither side's verdict
        /// answers for the other side.
        const TwoViews = 0b1;
    }
}

impl DiffFlags {
    /// The flags for a walk whose sides filter through `from` and `to`.
    ///
    /// [`Self::TwoViews`] is pointer inequality, which over-approximates: two
    /// filters held separately report as differing even where their rules agree,
    /// and a walk then does work it could have skipped rather than skipping work
    /// it had to do.
    fn between(from: &Arc<Filter>, to: &Arc<Filter>) -> Self {
        if Arc::ptr_eq(from, to) {
            Self::empty()
        } else {
            Self::TwoViews
        }
    }
}

/// What a walk did to answer, for a caller measuring the walk rather than reading
/// the changes it found.
///
/// One count per task, which every directory that task walks adds to and which folds
/// in what the subtrees it spawned report, so no counter is shared between tasks.
#[derive(Default)]
pub struct DiffWalkStats {
    /// Directories the walk stood in, the one it was rooted at included.
    ///
    /// What a prune saves, and the only place it shows: a pruned subtree and a
    /// walked one whose content matches report the same changes.
    pub directories_entered: AtomicU64,
    /// Verdicts the walk asked a filter for: the one admitting each child it
    /// visits, and the ones it asks about a pair -- what routes the node, what its
    /// children inherit, and what a prune reads before taking a subtree whole.
    ///
    /// The seeding the walk is rooted with is asked once before it begins and is
    /// not among them, nor is what the hierarchy walk an emitted add or delete fans
    /// out into asks of its own. A child whose name the state cannot read is
    /// counted although no verdict was asked for it, the two being one answer to
    /// the caller; the walk logs a warning for that child.
    pub filter_queries: AtomicU64,
}

impl DiffWalkStats {
    /// Records one directory the walk has entered.
    fn entered(&self) {
        self.directories_entered.fetch_add(1, Ordering::Relaxed);
    }

    /// Records one verdict asked of a filter.
    fn queried(&self) {
        self.filter_queries.fetch_add(1, Ordering::Relaxed);
    }

    /// Folds in what a subtree reported.
    fn append(&self, subtree: DiffWalkStats) {
        self.directories_entered
            .fetch_add(subtree.directories_entered.into_inner(), Ordering::Relaxed);
        self.filter_queries
            .fetch_add(subtree.filter_queries.into_inner(), Ordering::Relaxed);
    }
}

/// Emits the changes between the subtrees `from` and `to` name, both spelled from
/// `path`, into `changes`.
///
/// Each side is seeded from its own filter: a [`FilterStates`] names the line its
/// verdict was decided at, and one filter's line numbering says nothing about
/// another's. `path` leaves the walk only where both sides exclude it, which is
/// also the only case the exclusion is announced in.
///
/// [`DiffFlags`] are derived here rather than passed in. This is where the walk
/// begins and where both sides' filters are in hand, so deriving them anywhere
/// else would only be a second answer to the same question.
pub async fn diff_subtree(
    from: NodeChangeState,
    to: NodeChangeState,
    path: RelativePath,
    graft: Option<Arc<GraftOracle>>,
    changes: &ChangeSender,
    filter_mode: FilterMode,
) -> Result<DiffWalkStats, StateError> {
    let from_filter = &from.mapping.repository.filter;
    let to_filter = &to.mapping.repository.filter;
    let flags = DiffFlags::between(from_filter, to_filter);
    let (from_states, to_states, excluded) =
        seed_sides(from_filter, to_filter, &path, flags, filter_mode);
    if excluded {
        lore_debug!("Excluded by filter: {}", path.as_str());
        return Ok(DiffWalkStats::default());
    }

    let depth = query_depth(path.as_lowercase_str());
    diff_subtree_walk(
        PendingSubtree {
            from,
            to,
            cursor: DiffCursor {
                paths: DiffPaths {
                    from: path.clone(),
                    to: path,
                    depth,
                },
                states: DiffStates {
                    from: from_states,
                    to: to_states,
                },
            },
            graft,
        },
        flags,
        changes,
        filter_mode,
    )
    .await
}

/// The verdicts a walk over `path` starts from, one per side, and whether the walk
/// drops `path`.
///
/// One filter held by both sides is asked once: the same rules put the same
/// question about the same path answer the same way, and every caller that is not
/// moving the view holds one filter.
///
/// `path` is dropped where both sides exclude it, and only there is the exclusion
/// announced. The to side is asked last, and in the announcing form only once the
/// from side has excluded, so the drop and the announcement are one condition.
fn seed_sides(
    from_filter: &Arc<Filter>,
    to_filter: &Arc<Filter>,
    path: &RelativePath,
    flags: DiffFlags,
    mode: FilterMode,
) -> (FilterStates, FilterStates, bool) {
    let two_views = flags.contains(DiffFlags::TwoViews);
    let from_seed = two_views.then(|| seed_states(from_filter, path, false, mode));
    let announce = from_seed.is_none_or(|(_, excluded)| excluded);
    let to_seed = seed_states(to_filter, path, announce, mode);
    let (from_states, from_excluded) = from_seed.unwrap_or(to_seed);
    (from_states, to_seed.0, from_excluded && to_seed.1)
}

/// The states a walk rooted at `path` threads into its children, and whether
/// `filter` excludes `path` with everything below it. `path` is taken as a
/// directory, which is what a walk rooted at one stands in.
///
/// `path`'s own ancestors are folded here, since a walk starting at it has none
/// behind it.
///
/// `announce` emits the filter-exclude event where the verdict excludes.
fn seed_states(
    filter: &Filter,
    path: &RelativePath,
    announce: bool,
    mode: FilterMode,
) -> (FilterStates, bool) {
    let parent = filter.parent_exclusion_states(path);
    if announce {
        filter.child_emit_excludes(parent, path, true, mode)
    } else {
        filter.child_excludes_tree(parent, path, true, mode)
    }
}

/// A paired directory a walk has still to descend into.
///
/// Held by the task that will walk it rather than walked from the frame that found it, which is
/// what keeps a walk's stack the depth of one directory however deep the tree runs.
struct PendingSubtree {
    from: NodeChangeState,
    to: NodeChangeState,
    cursor: DiffCursor,
    /// A linked repository merges through its own link, so a mount clears the oracle for
    /// everything below it and each subtree carries the one that answers for it.
    graft: Option<Arc<GraftOracle>>,
}

/// The subtree walks one task has in flight, each reporting what it counted.
///
/// Nothing else comes back: a subtree sends its changes on a clone of the caller's sender
/// rather than collecting them for the directory above to pass on.
type SubtreeTasks = JoinSet<Result<DiffWalkStats, StateError>>;

/// The subtree walks one task has in flight and the ones it has still to walk itself.
struct SubtreeWork {
    tasks: SubtreeTasks,
    /// Taken from the back, so what waits here is the depth-first frontier — a level's worth of
    /// siblings per level standing open — rather than a whole level of the tree.
    pending: Vec<PendingSubtree>,
}

/// Budget for subtree tasks live at once, spent by [`dispatch_subtree_diff`].
///
/// Process-wide rather than per-walk, because every task in a walk owns its own
/// [`SubtreeTasks`]: a budget held by a walk would let concurrent walks each fan out as far as
/// one walk may.
static SUBTREE_TASK_SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();

fn subtree_task_semaphore() -> &'static Arc<Semaphore> {
    SUBTREE_TASK_SEMAPHORE.get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_TREE_TASKS)))
}

/// Hands the paired subtree `cursor` stands at to a task while the budget allows, and to the
/// walking task's own queue once it does not, then folds back whatever tasks have finished.
///
/// Queued rather than walked here, and rather than waiting for a permit. Walking it here would
/// spend a frame per level, so a deep enough tree would exhaust the stack; waiting for a permit
/// would deadlock, since a task holds its own until the subtrees it spawned finish. A queued
/// subtree costs neither: the task that queued it walks it in the same loop it walks everything
/// else, so a walk with no budget at all runs to the bottom of the tree in one task with no
/// stack growth.
fn dispatch_subtree_diff(
    from: NodeChangeState,
    to: NodeChangeState,
    cursor: DiffCursor,
    flags: DiffFlags,
    graft: Option<Arc<GraftOracle>>,
    context: DiffContext<'_>,
    work: &mut SubtreeWork,
) -> Result<(), StateError> {
    let DiffContext {
        changes,
        filter_mode,
        stats,
        ..
    } = context;
    let SubtreeWork { tasks, pending } = work;
    let subtree = PendingSubtree {
        from,
        to,
        cursor,
        graft,
    };
    match subtree_task_semaphore().clone().try_acquire_owned() {
        Ok(permit) => {
            let changes = changes.clone();
            lore_spawn!(tasks, async move {
                let _permit = permit;
                diff_subtree_walk(subtree, flags, &changes, filter_mode).await
            });
        }
        Err(_) => pending.push(subtree),
    }
    while let Some(joined) = tasks.try_join_next() {
        merge_subtree_task(joined, stats)?;
    }
    Ok(())
}

/// Walks `subtree` and everything below it that no task took, and answers with what all of that
/// counted.
///
/// The walk is a loop rather than a recursion: a directory hands what it cannot spawn to
/// [`SubtreeWork::pending`], and this takes them one at a time. So a task's stack holds one
/// directory, its tasks are bounded by the budget, and what grows with the tree is a queue of
/// change states.
async fn diff_subtree_walk(
    first: PendingSubtree,
    flags: DiffFlags,
    changes: &ChangeSender,
    filter_mode: FilterMode,
) -> Result<DiffWalkStats, StateError> {
    let stats = DiffWalkStats::default();
    let mut work = SubtreeWork {
        tasks: SubtreeTasks::new(),
        pending: vec![first],
    };

    // Run the walk in a helper so any `?` early-out still hits the
    // drain below — otherwise the JoinSet drops with subtree-diff
    // tasks still running, leaking the Arc<RepositoryContext> clones.
    let work_result = walk_pending_subtrees(&mut work, flags, changes, filter_mode, &stats).await;
    let drain_result = lore_drain_tasks!(work.tasks, StateError::internal("Task failure"));
    work_result?;
    drain_result?;
    Ok(stats)
}

/// Walks every queued subtree, and every subtree they queue, then joins the tasks they spawned.
async fn walk_pending_subtrees(
    work: &mut SubtreeWork,
    flags: DiffFlags,
    changes: &ChangeSender,
    filter_mode: FilterMode,
    stats: &DiffWalkStats,
) -> Result<(), StateError> {
    while let Some(subtree) = work.pending.pop() {
        // Boxed for two reasons: it keeps one directory's state off the walk's own future,
        // which every caller of `diff` holds and `clippy::large_futures` bounds, and it is
        // what gives that future a size at all, since a directory dispatches walks and so
        // names this one.
        Box::pin(diff_subtree_node(
            subtree,
            flags,
            changes,
            filter_mode,
            work,
            stats,
        ))
        .await?;
    }
    while let Some(joined) = work.tasks.join_next().await {
        merge_subtree_task(joined, stats)?;
    }
    Ok(())
}

/// Folds a joined subtree task's count into the walk that dispatched it.
fn merge_subtree_task(
    joined: Result<Result<DiffWalkStats, StateError>, JoinError>,
    stats: &DiffWalkStats,
) -> Result<(), StateError> {
    stats.append(
        joined
            .internal("Task failure")
            .map_err(StateError::from)
            .flatten()?,
    );
    Ok(())
}

/// The path the walk stands at, one per side.
///
/// The walk pairs children by a case-folded name, so the two spellings diverge only in
/// case and name the same number of components: one `depth` answers for both.
struct DiffPaths {
    from: RelativePath,
    to: RelativePath,
    /// How many components the paths name, which bounds which rules can reach below
    /// them. Zero for the repository root.
    depth: u32,
}

/// The filter verdicts for [`DiffPaths`], one per side.
///
/// A rename gives the two sides different paths, and each side's children are
/// filtered against its own, so the walk carries a verdict for each rather than
/// one for both.
#[derive(Clone, Copy)]
struct DiffStates {
    from: FilterStates,
    to: FilterStates,
}

/// Where the walk stands on both sides: the paths, and the verdicts they were
/// reached with.
struct DiffCursor {
    paths: DiffPaths,
    states: DiffStates,
}

impl DiffCursor {
    /// Re-seeds the verdicts after [`find_sorted_children`] replaced a file path
    /// with its parent, which the ones carried in no longer describe.
    ///
    /// A whole-path fold, because the parent has no walk behind it here. One
    /// walk starts this way at most.
    fn reseed(&mut self, from: &NodeChangeState, to: &NodeChangeState) {
        self.states = DiffStates {
            from: from
                .mapping
                .repository
                .filter
                .exclusion_states(&self.paths.from),
            to: to
                .mapping
                .repository
                .filter
                .exclusion_states(&self.paths.to),
        };
    }
}

/// Emits the changes between the two nodes `subtree` stands at, and queues in `work` what
/// descending below them needs.
async fn diff_subtree_node(
    subtree: PendingSubtree,
    flags: DiffFlags,
    changes: &ChangeSender,
    filter_mode: FilterMode,
    work: &mut SubtreeWork,
    stats: &DiffWalkStats,
) -> Result<(), StateError> {
    let PendingSubtree {
        from,
        to,
        mut cursor,
        graft,
    } = subtree;
    // If path is a file then treat this as a call for the parent with only one child.
    let (from_nodes, to_nodes, popped) =
        find_sorted_children(&mut cursor.paths, &from, &to).await?;
    if popped {
        cursor.reseed(&from, &to);
    }
    let DiffCursor { paths, states } = cursor;

    stats.entered();
    diff_subtree_node_walk(
        &from,
        &to,
        &paths,
        states,
        flags,
        graft,
        changes,
        filter_mode,
        &from_nodes,
        &to_nodes,
        work,
        stats,
    )
    .await
}

/// Walk paired/solo from/to children, dispatching paired-node diffs through
/// [`dispatch_subtree_diff`] and emitting solo changes as they are found.
///
/// Each (case-normalized) name is in one of three buckets:
/// 1. In `to_nodes` but not `from_nodes` → Added or Moved.
/// 2. In `from_nodes` but not `to_nodes` → Deleted or was Moved.
/// 3. In both → Modified, case-only rename, or unchanged.
#[allow(clippy::too_many_arguments)]
async fn diff_subtree_node_walk(
    from: &NodeChangeState,
    to: &NodeChangeState,
    paths: &DiffPaths,
    states: DiffStates,
    flags: DiffFlags,
    graft: Option<Arc<GraftOracle>>,
    changes: &ChangeSender,
    filter_mode: FilterMode,
    from_nodes: &StateChildrenNodes,
    to_nodes: &StateChildrenNodes,
    work: &mut SubtreeWork,
    stats: &DiffWalkStats,
) -> Result<(), StateError> {
    let context = DiffContext {
        changes,
        from_nodes,
        to_nodes,
        paths,
        states,
        filter_mode,
        stats,
    };
    let mut to_index = 0;
    for from_named_node in from_nodes.children.iter() {
        stats.queried();
        let Some((from_node_search, from_node_states)) = get_filtered_node_and_path(
            from_nodes,
            from_named_node.node,
            &paths.from,
            states.from,
            filter_mode,
        )
        .await?
        else {
            continue;
        };

        while to_index < to_nodes.children.len()
            && to_nodes.children[to_index].name < from_named_node.name
        {
            add_change_for_solo_to_node(context, from, to_index).await?;
            to_index += 1;
        }

        if to_index >= to_nodes.children.len()
            || to_nodes.children[to_index].name > from_named_node.name
        {
            add_change_for_solo_from_node(
                context,
                from_named_node,
                to,
                &from_node_search,
                from_node_states,
            )
            .await?;
        } else {
            let to_named_node = &to_nodes.children[to_index];
            to_index += 1;

            add_change_for_paired_nodes(
                work,
                flags,
                graft.clone(),
                context,
                to_named_node,
                from_named_node.node,
                &from_node_search,
                from_node_states,
            )
            .await?;
        }
    }

    for to_index in to_index..to_nodes.children.len() {
        add_change_for_solo_to_node(context, from, to_index).await?;
    }

    Ok(())
}

/// What every child of one directory is reported against: where the walk stands, what it
/// filters through, and where it reports to.
#[derive(Clone, Copy)]
struct DiffContext<'a> {
    changes: &'a ChangeSender,
    from_nodes: &'a StateChildrenNodes,
    to_nodes: &'a StateChildrenNodes,
    paths: &'a DiffPaths,
    states: DiffStates,
    filter_mode: FilterMode,
    stats: &'a DiffWalkStats,
}

async fn add_change_for_solo_from_node(
    context: DiffContext<'_>,
    from_named_node: &StateNamedNode,
    to: &NodeChangeState,
    from_node_search: &NodeSearchResult,
    from_node_states: FilterStates,
) -> Result<(), StateError> {
    let DiffContext {
        changes,
        from_nodes,
        to_nodes,
        filter_mode,
        ..
    } = context;
    let NodeSearchResult {
        node: from_node,
        path: from_path,
    } = from_node_search;
    // Before marking as deleted, check if this node was moved to a different location
    // in the staged state (same node ID, different name/parent)
    if from_named_node.node.is_valid_node_id()
        && let Some(check_node) = to_nodes
            .state
            .try_node(to_nodes.repository.clone(), from_named_node.node)
            .await
        && check_node.is_staged_move()
        && let Ok(from_node) = from_nodes
            .state
            .node(from_nodes.repository.clone(), from_named_node.node)
            .await
        && from_node.address.context == check_node.address.context
    {
        // This node was moved, not deleted - skip adding delete change
        // The move will be reported from the "to" side iteration
        lore_trace!(
            "Node {} moved (not deleted), skipping delete change",
            from_named_node.node
        );
    } else {
        lore_trace!("Node {} deleted", from_named_node.node);

        let from = NodeChangeState {
            mapping: NodeMapping {
                repository: from_nodes.repository.clone(),
                state: from_nodes.state.clone(),
                path: from_path.clone(),
                node: from_named_node.node,
            },
            observed: None,
            flags: NodeFlags::from_bits_retain(from_node.flags),
            address: from_node.address,
            mode: from_node.mode,
        };

        add_change(
            from,
            to.invalid(from_path.clone()),
            change::FileAction::Delete,
            change::Flags::None,
            changes,
            filter_mode,
            from_node_states,
        )
        .await?;
    }
    Ok(())
}

async fn add_change_for_solo_to_node(
    context: DiffContext<'_>,
    from: &NodeChangeState,
    to_index: usize,
) -> Result<(), StateError> {
    let DiffContext {
        changes,
        from_nodes,
        to_nodes,
        paths,
        states,
        filter_mode,
        stats,
    } = context;
    let to_named_node = &to_nodes.children[to_index];
    stats.queried();
    let Some((
        NodeSearchResult {
            node: to_node,
            path: subpath,
        },
        to_node_states,
    )) = get_filtered_node_and_path(
        to_nodes,
        to_named_node.node,
        &paths.to,
        states.to,
        filter_mode,
    )
    .await?
    else {
        return Ok(());
    };

    // Determine the action and from_path for moved nodes
    let (file_action, from_path) = if to_node.is_staged_delete() {
        lore_trace!("Node {} deleted", to_named_node.node);
        (change::FileAction::Delete, None)
    } else if to_node.is_staged_move() {
        // TODO(mjansson): A node moved within a repository the working tree materializes below
        //                 its root needs the path of the mount to spell where it was, which the
        //                 walk does not carry. Such a move is realized as a delete and an add.
        let original_path = if from_nodes.repository.id == from_nodes.repository.root_id() {
            from_nodes
                .state
                .node_path(from_nodes.repository.clone(), to_named_node.node)
                .await
                .ok()
        } else {
            None
        };
        lore_trace!(
            "Node {} moved from {:?} to {}",
            to_named_node.node,
            original_path,
            subpath
        );
        (
            change::FileAction::Move,
            original_path.map(|p| RelativePath::new_from_initial_path(&p).unwrap_or_default()),
        )
    } else {
        lore_trace!("Node {} added", to_named_node.node);
        (change::FileAction::Add, None)
    };

    let to = NodeChangeState {
        mapping: NodeMapping {
            repository: to_nodes.repository.clone(),
            state: to_nodes.state.clone(),
            path: subpath.clone(),
            node: to_named_node.node,
        },
        observed: None,
        flags: NodeFlags::from_bits_retain(to_node.flags),
        address: to_node.address,
        mode: to_node.mode,
    };

    let from = from.invalid(match file_action {
        // Empty where the walk could not spell the source, which is a move it reports without one.
        change::FileAction::Move => from_path.unwrap_or_default(),
        _ => subpath.clone(),
    });

    add_change(
        from,
        to,
        file_action,
        change::Flags::None,
        changes,
        filter_mode,
        to_node_states,
    )
    .await?;
    Ok(())
}

/// The verdict a walk under `path` inherits, given `node`'s own.
///
/// The walk continues under `path` as a directory, and a whole-path query folds
/// every ancestor as one. A directory node was already stepped that way, so
/// `node_states` stands; a link node was not, and is stepped again as the
/// directory its content sits in.
fn subtree_states(
    nodes: &StateChildrenNodes,
    parent: FilterStates,
    path: &RelativePath,
    node: &Node,
    node_states: FilterStates,
    mode: FilterMode,
    stats: &DiffWalkStats,
) -> FilterStates {
    if node.is_directory() {
        return node_states;
    }
    stats.queried();
    nodes
        .repository
        .filter
        .child_excludes_tree(parent, path, true, mode)
        .0
}

/// Whether the to side holds nothing at or under `path`, with the states its children inherit
/// where the walk goes on to use them.
///
/// The verdict reads `false` wherever the two sides cannot disagree, since under one filter the
/// from side's verdict, taken before this node was paired, is the to side's too.
///
/// The filter is asked only where one of the two answers is read: a paired directory threads the
/// states into the recursion below it whatever the flags say, and every other pairing reads the
/// verdict alone, so is asked under [`DiffFlags::TwoViews`] alone. **Only a paired directory may
/// read the states.** Unasked they are the parent's, and a caller reading them there would filter
/// `path`'s children as though `path`'s own rules had never been stepped.
///
/// `is_file` is the to node's, and anything else is asked as a directory, a link included, because
/// a link's content sits in a directory even though the node is not one. So the answer is the
/// subtree's and not the node's, and for a link the two diverge: a directory-only rule naming a mount
/// reaches what is under it without matching the mount itself.
#[allow(clippy::too_many_arguments)]
fn to_subtree_verdict(
    filter: &Filter,
    parent: FilterStates,
    path: &RelativePath,
    was_file: bool,
    is_file: bool,
    flags: DiffFlags,
    mode: FilterMode,
    stats: &DiffWalkStats,
) -> (FilterStates, bool) {
    let two_views = flags.contains(DiffFlags::TwoViews);
    let paired_directory = !was_file && !is_file;
    if !paired_directory && !two_views {
        return (parent, false);
    }
    stats.queried();
    let (states, excluded) = filter.child_excludes_tree(parent, path, !is_file, mode);
    (states, excluded && two_views)
}

/// Whether the two views can hold different content below the directory the walk paired at
/// `path`, so a walk that found its content equal has to descend anyway.
///
/// Equal content says nothing about the views: it says the two revisions agree, and what a
/// working tree holds is what the revision and the view agree on. Only where both views cover
/// the subtree -- every path under it included, whatever else their rules say -- is there
/// nothing inside for them to differ about.
///
/// A view question, and put to the view slot: the ignore slot governs what a revision is written
/// from rather than what a working tree materializes it into.
///
/// The caller asks under [`DiffFlags::TwoViews`] and a mode consulting the view, and nowhere
/// else: one filter cannot diverge from itself, and a walk consulting no view has no view to
/// diverge over.
///
/// One `path` for both sides, as one `depth` serves both in [`DiffPaths`]: the two sides' spellings
/// of a paired directory diverge only in case, and a filter reads the folded spelling.
fn views_diverge_below(
    from_filter: &Filter,
    to_filter: &Filter,
    path: &RelativePath,
    depth: u32,
    states: DiffStates,
    stats: &DiffWalkStats,
) -> bool {
    let covers = |filter: &Filter, states| {
        stats.queried();
        filter.covers_subtree(states, path, depth, FilterMode::View)
    };
    !(covers(from_filter, states.from) && covers(to_filter, states.to))
}

/// Report the change between the two nodes the walk paired by name, and descend where both are
/// directories.
///
/// The walk pairs on a case-folded name, so a pair's own two spellings can differ, and that is the
/// rename it reports as a move. A node below such a pair stands at two paths without being renamed
/// itself, and is reported only for what else changed about it.
#[allow(clippy::too_many_arguments)]
async fn add_change_for_paired_nodes(
    work: &mut SubtreeWork,
    flags: DiffFlags,
    graft: Option<Arc<GraftOracle>>,
    context: DiffContext<'_>,
    to_named_node: &StateNamedNode,
    from_node_id: NodeID,
    from_node_search: &NodeSearchResult,
    from_node_states: FilterStates,
) -> Result<(), StateError> {
    let DiffContext {
        changes,
        from_nodes,
        to_nodes,
        paths,
        states,
        filter_mode,
        stats,
    } = context;
    let NodeSearchResult {
        node: from_node,
        path: from_path,
    } = from_node_search;
    let to_block_index = NodeBlock::index(to_named_node.node);
    let to_node_index = Node::index(to_named_node.node);
    let to_block = to_nodes
        .state
        .block(to_nodes.repository.clone(), to_block_index)
        .await?;
    let to_node = to_block.node(to_node_index);

    let from_size = from_node.size;
    let to_size = to_node.size;
    let from_address = from_node.address;
    let to_address = to_node.address;
    let hash_equal = from_address == to_address;
    let from_mode = from_node.mode;
    let to_mode = to_node.mode;
    let mode_equal = from_mode == to_mode;
    let is_modify = !hash_equal || !mode_equal;
    // What the walk measured of the content, which the action states nothing about: a node can be
    // moved and modified at once, or moved with the content it had.
    let measured = if is_modify {
        change::Flags::Modify
    } else {
        change::Flags::None
    };
    let is_staged = to_node.is_staged();
    let is_staged_delete = to_node.is_staged_delete();
    let is_staged_merge = to_node.is_staged_merge();
    let is_staged_merge_conflict = to_node.is_staged_merge_conflict();
    let is_dirty = to_node.is_dirty();
    let is_dirty_delete = to_node.is_dirty_delete();

    let to = NodeChangeState {
        mapping: NodeMapping {
            repository: to_nodes.repository.clone(),
            state: to_nodes.state.clone(),
            path: from_path.clone(),
            node: to_named_node.node,
        },
        observed: None,
        flags: NodeFlags::from_bits_retain(to_node.flags),
        address: to_node.address,
        mode: to_node.mode,
    };

    let from = NodeChangeState {
        mapping: NodeMapping {
            repository: from_nodes.repository.clone(),
            state: from_nodes.state.clone(),
            path: from_path.clone(),
            node: from_node_id,
        },
        observed: None,
        flags: NodeFlags::from_bits_retain(from_node.flags),
        address: from_node.address,
        mode: from_node.mode,
    };

    if is_staged_delete || is_dirty_delete {
        lore_trace!("Diff node {} deleted", &paths.to);

        add_change(
            from,
            to,
            change::FileAction::Delete,
            change::Flags::None,
            changes,
            filter_mode,
            from_node_states,
        )
        .await?;
    } else {
        let from_type = from_node.node_type();
        let to_type = to_node.node_type();
        // A directory and a link both hold a subtree, but one of each at a path
        // is a type change: the subtree moved between repositories.
        let both_files = from_type == to_type && to_type == LoreNodeType::File;
        let same_type = from_type == to_type;

        let to_name = match to_nodes
            .state
            .node_name_ref(to_nodes.repository.clone(), to_named_node.node)
            .await
        {
            Ok(name) => name,
            Err(err) => {
                lore_warn!(
                    "Skipping node {} with invalid name: {err}",
                    to_named_node.node
                );
                return Ok(());
            }
        };
        let from_name = from_path.name();
        let is_rename = *from_name != *to_name;

        let subpath = paths.to.push_into_buf(&to_name).freeze();
        let mut to = to;
        to.mapping.path = subpath.clone();
        if is_rename {
            lore_trace!("Node is renamed from {from_name} -> {to_name}");
        }
        drop(to_name);

        let (to_subtree_states, to_excludes_subtree) = to_subtree_verdict(
            &to_nodes.repository.filter,
            states.to,
            &subpath,
            from_type == LoreNodeType::File,
            to_type == LoreNodeType::File,
            flags,
            filter_mode,
            stats,
        );
        if to_excludes_subtree {
            lore_trace!("Diff node {subpath} excluded on the to side, delete {from_path}");
            add_change(
                from,
                to,
                change::FileAction::Delete,
                change::Flags::None,
                changes,
                filter_mode,
                from_node_states,
            )
            .await?;
            return Ok(());
        }

        let action = if is_rename {
            change::FileAction::Move
        } else {
            change::FileAction::Keep
        };

        if both_files {
            if is_modify || is_staged || is_staged_merge || is_dirty || is_rename {
                lore_trace!(
                    "Diff node {subpath} file modified {from_address} size {from_size} to {to_address} size {to_size}, mode {from_mode} to {to_mode} - {action:?}"
                );

                emit_change(
                    from.clone(),
                    to.clone(),
                    action,
                    measured,
                    changes,
                    filter_mode,
                )
                .await?;
            }
        } else if same_type {
            let child_states = DiffStates {
                from: subtree_states(
                    from_nodes,
                    states.from,
                    from_path,
                    from_node,
                    from_node_states,
                    filter_mode,
                    stats,
                ),
                to: to_subtree_states,
            };
            let child_depth = paths.depth + 1;

            // A conflict on a directory node is at the path itself, and the
            // walk below it reports nothing that carries it.
            let conflicted_directory =
                is_staged_merge_conflict && to_type == LoreNodeType::Directory;
            if !mode_equal || is_rename || conflicted_directory {
                lore_trace!(
                    "Diff node {subpath} directory mode change from {from_mode} to {to_mode}, {action:?}|modify"
                );
                emit_change(
                    from.clone(),
                    to.clone(),
                    action,
                    measured,
                    changes,
                    filter_mode,
                )
                .await?;
            }
            if !hash_equal
                || is_staged
                || is_staged_merge
                || is_dirty
                || (flags.contains(DiffFlags::TwoViews)
                    && filter_mode.contains(FilterMode::View)
                    && views_diverge_below(
                        &from_nodes.repository.filter,
                        &to_nodes.repository.filter,
                        &subpath,
                        child_depth,
                        child_states,
                        stats,
                    ))
            {
                if to_node.is_link() {
                    let link_repository_id: RepositoryId = to_node.address.context.into();

                    let can_read_link = to.mapping.repository.can_read_link(link_repository_id);

                    // If the link is staged and doesn't have staged children, it's a link update
                    let recurse_link = if !can_read_link {
                        false
                    } else {
                        let linked_repository = to
                            .mapping
                            .repository
                            .to_link_context(link_repository_id)
                            .await;
                        let linked_state =
                            State::deserialize(linked_repository.clone(), to_node.address.hash)
                                .await
                                .forward::<StateError>("Link error")?;

                        let has_staged_children = linked_state
                            .node_has_staged_children(linked_repository.clone(), to_node.child)
                            .await?;

                        if is_staged { has_staged_children } else { true }
                    };

                    if !recurse_link {
                        if can_read_link {
                            lore_debug!("Diff node {subpath} has no linked changes");
                        } else {
                            lore_debug!(
                                "Diff node {subpath} not descended: caller not authorized \
                                 for linked repository {link_repository_id}"
                            );
                        }
                        emit_change(
                            from.clone(),
                            to.clone(),
                            action,
                            measured,
                            changes,
                            filter_mode,
                        )
                        .await?;
                    } else {
                        lore_debug!("Diff node {subpath} has linked changes, recurse diff");
                        dispatch_subtree_diff(
                            from,
                            to,
                            DiffCursor {
                                paths: DiffPaths {
                                    from: from_path.clone(),
                                    to: subpath,
                                    depth: child_depth,
                                },
                                states: child_states,
                            },
                            flags,
                            // A linked repository merges through its own link.
                            None,
                            context,
                            work,
                        )?;
                    }
                } else {
                    // Source changed this directory and the target branch did
                    // not, so nothing inside needs a three-way merge. Emit one
                    // change for the subtree instead of one per file.
                    let adopted = match graft.as_ref() {
                        Some(oracle) if !hash_equal => {
                            oracle.adoptable(&subpath, from_address).await
                        }
                        _ => false,
                    };

                    if adopted {
                        lore_trace!(
                            "Diff node {subpath} unchanged on target, grafting subtree {from_address} -> {to_address}"
                        );
                        emit_change(
                            from.clone(),
                            to.clone(),
                            change::FileAction::Graft,
                            measured,
                            changes,
                            filter_mode,
                        )
                        .await?;
                    } else {
                        lore_trace!(
                            "Diff node {subpath} directory hash change from {from_address} to {to_address}, recurse diff"
                        );
                        dispatch_subtree_diff(
                            from,
                            to,
                            DiffCursor {
                                paths: DiffPaths {
                                    from: from_path.clone(),
                                    to: subpath,
                                    depth: child_depth,
                                },
                                states: child_states,
                            },
                            flags,
                            graft,
                            context,
                            work,
                        )?;
                    }
                }
            }
        } else {
            lore_trace!("Diff node {subpath} changed from {from_type:?} to {to_type:?}");
            stats.queried();
            let (to_node_states, to_node_excluded) = to_nodes
                .repository
                .filter
                .child_excludes_tree(states.to, &subpath, to_node.is_directory(), filter_mode);
            add_change(
                from.clone(),
                to.clone(),
                change::FileAction::Delete,
                change::Flags::None,
                changes,
                filter_mode,
                from_node_states,
            )
            .await?;
            if !to_node_excluded {
                add_change(
                    from,
                    to,
                    change::FileAction::Add,
                    change::Flags::None,
                    changes,
                    filter_mode,
                    to_node_states,
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// The third element reports whether `directory_paths` was popped to the
/// parent, which a caller holding a verdict for the path it passed in has to
/// know about.
async fn find_sorted_children(
    directory_paths: &mut DiffPaths,
    from: &NodeChangeState,
    to: &NodeChangeState,
) -> Result<(StateChildrenNodes, StateChildrenNodes, bool), StateError> {
    // If the given path and subtrees are files and not directories, which
    // can be the case when called from library interfaces like status with
    // an explicit path, we need to handle this here.
    let is_file_path = {
        let is_from_file = from
            .mapping
            .state
            .node(from.mapping.repository.clone(), from.mapping.node)
            .await
            .map(|node| node.is_file())
            .unwrap_or_default();
        let is_to_file = to
            .mapping
            .state
            .node(to.mapping.repository.clone(), to.mapping.node)
            .await
            .map(|node| node.is_file())
            .unwrap_or_default();
        is_from_file || is_to_file
    };

    Ok(if is_file_path {
        // Given path was a file node, treat it as enumerating the parent directory
        // and finding a single node for that file
        let mut from_nodes = StateChildrenNodes {
            repository: from.mapping.repository.clone(),
            state: from.mapping.state.clone(),
            children: vec![],
        };

        if from.mapping.node.is_valid_node_id()
            && let Ok(node) = from
                .mapping
                .state
                .node(from.mapping.repository.clone(), from.mapping.node)
                .await
        {
            from_nodes.children.push(StateNamedNode {
                node: from.mapping.node,
                name: node.name_hash,
            });
        }

        let mut to_nodes = StateChildrenNodes {
            repository: to.mapping.repository.clone(),
            state: to.mapping.state.clone(),
            children: vec![],
        };

        if to.mapping.node.is_valid_node_id()
            && let Ok(node) = to
                .mapping
                .state
                .node(to.mapping.repository.clone(), to.mapping.node)
                .await
        {
            to_nodes.children.push(StateNamedNode {
                node: to.mapping.node,
                name: node.name_hash,
            });
        }
        directory_paths.from.pop();
        directory_paths.to.pop();
        directory_paths.depth = directory_paths.depth.saturating_sub(1);
        (from_nodes, to_nodes, true)
    } else {
        // Given path was a directory, enumerate all nodes
        let from_nodes = {
            let repository = from.mapping.repository.clone();
            let state = from.mapping.state.clone();
            let node = from.mapping.node;
            lore_spawn!(async move {
                let mut nodes = state
                    .collect_children_unsorted(
                        repository, node, false, /* No deleted nodes */
                        true,  /* Include links */
                    )
                    .await?;
                named_node_sort(&mut nodes.children);
                Ok::<_, StateError>(nodes)
            })
        };
        let to_nodes = {
            let repository = to.mapping.repository.clone();
            let state = to.mapping.state.clone();
            let node = to.mapping.node;
            lore_spawn!(async move {
                let mut nodes = state
                    .collect_children_unsorted(
                        repository, node, true, /* Include deleted nodes */
                        true, /* Include links */
                    )
                    .await?;
                named_node_sort(&mut nodes.children);
                Ok::<_, StateError>(nodes)
            })
        };

        let from_nodes = from_nodes.await;
        let to_nodes = to_nodes.await;

        (
            from_nodes
                .internal("Task failure")
                .map_err(StateError::from)??,
            to_nodes
                .internal("Task failure")
                .map_err(StateError::from)??,
            false,
        )
    })
}

pub struct NodeSearchResult {
    pub node: Node,
    pub path: RelativePath,
}

/// The node a walk matched against an entry on disk.
pub struct NodeMatch {
    pub node: Node,
    /// The node's own path, present only where the state spells its name differently from
    /// the entry. Names are matched by a case-folded hash, so every other node is spelled
    /// exactly by the entry and its path is built from that instead.
    pub renamed_path: Option<RelativePath>,
}

impl NodeMatch {
    /// The node's own path, for a change to record or a recursion to walk below.
    ///
    /// `parent` must be the one [`get_node_match`] was given and `name` the entry it was
    /// matched against: a renamed node already carries a path built under that parent, and
    /// answers with it whatever is passed here.
    pub fn path(&self, parent: &RelativePath, name: &str) -> RelativePath {
        match &self.renamed_path {
            Some(path) => path.clone(),
            None => parent.push_into_buf(name).freeze(),
        }
    }

    /// Whether the state spells the node's name differently from the entry on disk.
    pub fn renamed(&self) -> bool {
        self.renamed_path.is_some()
    }
}

/// The node `node_id` holds, matched against the entry named `name` in `parent`, or nothing
/// where the node's name is unusable.
///
/// A walk matches far more nodes than it records, so no path is built for a node the state
/// spells as the entry does; [`NodeMatch::path`] builds one where it is needed.
pub async fn get_node_match(
    nodes: &StateChildrenNodes,
    node_id: NodeID,
    name: &str,
    parent: &RelativePath,
) -> Result<Option<NodeMatch>, StateError> {
    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);
    let block = nodes
        .state
        .block_with_nametable(nodes.repository.clone(), block_index)
        .await?;
    let node = block.node(node_index);
    let node_name = match block.node_name_ref(node_index) {
        Ok(node_name) => node_name,
        Err(err) => {
            lore_warn!("Skipping node {} with invalid name: {err}", node_id);
            return Ok(None);
        }
    };
    let renamed_path = (*node_name != *name).then(|| parent.push_into_buf(&node_name).freeze());

    Ok(Some(NodeMatch { node, renamed_path }))
}

async fn get_node_and_path(
    nodes: &StateChildrenNodes,
    node_id: NodeID,
    path: &RelativePath,
) -> Result<Option<NodeSearchResult>, StateError> {
    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);
    let block = nodes
        .state
        .block_with_nametable(nodes.repository.clone(), block_index)
        .await?;
    let node = block.node(node_index);
    let name = match block.node_name_ref(node_index) {
        Ok(name) => name,
        Err(err) => {
            lore_warn!("Skipping node {} with invalid name: {err}", node_id);
            return Ok(None);
        }
    };
    let path = path.push_into_buf(name).freeze();

    Ok(Some(NodeSearchResult { node, path }))
}

/// [`get_node_and_path`] for a walk standing in `path`: `parent_states` is the
/// filter's verdict for it, and the child's own is returned alongside the node
/// for the walk to carry below.
pub async fn get_filtered_node_and_path(
    nodes: &StateChildrenNodes,
    node_id: NodeID,
    path: &RelativePath,
    parent_states: FilterStates,
    filter_mode: FilterMode,
) -> Result<Option<(NodeSearchResult, FilterStates)>, StateError> {
    Ok(get_node_and_path(nodes, node_id, path)
        .await?
        .and_then(|result| {
            let (states, excluded) = nodes.repository.filter.child_emit_excludes(
                parent_states,
                &result.path,
                result.node.is_directory(),
                filter_mode,
            );
            if excluded {
                lore_trace!("Path excluded by filter: {}", path);
                None
            } else {
                Some((result, states))
            }
        }))
}

/// The decisions a walk takes and does not report: what it makes of its two filters,
/// what it seeds each side with, what it asks the to side about a paired node, and what it
/// does with a subtree it has no budget left to walk in a task.
#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::state::ChangeStream;

    /// A filter that excludes `x` and re-includes under it, so a query against it steps
    /// to a verdict no root state matches and still descends.
    ///
    /// Descending is what keeps it free of an event, and so of the execution context a
    /// send reaches for and a unit test does not stand up.
    fn stepping() -> Arc<Filter> {
        let mut filter = Filter::default();
        filter.view.add_exclusion("/x").expect("view exclusion");
        filter
            .view
            .add_inclusion("/x/keep")
            .expect("view inclusion");
        Arc::new(filter)
    }

    fn stepped_path() -> RelativePath {
        RelativePath::new_from_initial_path("x").expect("valid path")
    }

    #[test]
    fn one_filter_answers_for_both_sides() {
        let filter = stepping();
        let path = stepped_path();

        let (from_states, to_states, excluded) = seed_sides(
            &filter,
            &filter,
            &path,
            DiffFlags::empty(),
            FilterMode::Full,
        );

        assert_eq!(
            (from_states, excluded),
            seed_states(&filter, &path, true, FilterMode::Full),
            "one filter must seed both sides with the one answer it gives"
        );
        assert_eq!(from_states, to_states);
    }

    /// Two filters are asked separately, and the walk goes on unless both exclude.
    #[test]
    fn two_filters_seed_each_side_from_its_own() {
        let from = stepping();
        let to = Arc::new(Filter::default());
        let path = stepped_path();

        let (from_states, to_states, excluded) =
            seed_sides(&from, &to, &path, DiffFlags::TwoViews, FilterMode::Full);

        assert_ne!(
            from_states, to_states,
            "each side must carry the verdict of the filter it walks under"
        );
        assert_eq!(
            to_states,
            FilterStates::ROOT,
            "a filter with no rules steps nowhere"
        );
        assert!(
            !excluded,
            "a path one side still holds must not leave the walk"
        );
    }

    #[test]
    fn one_filter_on_both_sides_is_not_two_views() {
        let filter = Arc::new(Filter::default());
        let shared = filter.clone();
        assert_eq!(DiffFlags::between(&filter, &shared), DiffFlags::empty());
    }

    #[test]
    fn two_filters_are_two_views_even_where_their_rules_agree() {
        let from = Arc::new(Filter::default());
        let to = Arc::new(Filter::default());
        assert_eq!(DiffFlags::between(&from, &to), DiffFlags::TwoViews);
    }

    /// A filter that excludes `path`, so a query against it is visible in what comes
    /// back: states other than the parent's, and a verdict of `true`.
    fn excluding(path: &str) -> Filter {
        let mut filter = Filter::default();
        filter
            .view
            .add_exclusion(&format!("/{path}"))
            .expect("view exclusion");
        filter
    }

    /// The to side is not asked about a paired file under one filter.
    #[test]
    fn a_paired_file_under_one_filter_costs_no_to_side_query() {
        let filter = excluding("x");
        let path = RelativePath::new_from_initial_path("x").expect("valid path");
        let stats = DiffWalkStats::default();
        let verdict = |flags| {
            to_subtree_verdict(
                &filter,
                FilterStates::ROOT,
                &path,
                true,
                true,
                flags,
                FilterMode::Full,
                &stats,
            )
        };

        assert_eq!(verdict(DiffFlags::empty()), (FilterStates::ROOT, false));
        assert_eq!(stats.filter_queries.load(Ordering::Relaxed), 0);
        assert!(
            verdict(DiffFlags::TwoViews).1,
            "the filter must be one that answers true, or the case above proves nothing"
        );
        assert_eq!(stats.filter_queries.load(Ordering::Relaxed), 1);
    }

    /// A paired directory needs the states below it whatever the flags say, so it is
    /// asked either way -- and reports no exclusion under one filter, where the from
    /// side's verdict already stands for both.
    #[test]
    fn a_paired_directory_is_asked_under_one_filter_and_reports_nothing() {
        let filter = excluding("x");
        let path = RelativePath::new_from_initial_path("x").expect("valid path");
        let stats = DiffWalkStats::default();
        let (states, excluded) = to_subtree_verdict(
            &filter,
            FilterStates::ROOT,
            &path,
            false,
            false,
            DiffFlags::empty(),
            FilterMode::Full,
            &stats,
        );

        assert_ne!(states, FilterStates::ROOT, "the states must be stepped");
        assert!(!excluded, "one filter cannot route the two sides apart");
        assert_eq!(stats.filter_queries.load(Ordering::Relaxed), 1);
    }

    /// A directory the to side excludes but re-includes under is still held there, so the
    /// verdict is the subtree's and not the node's: deleting it would take the re-included
    /// content with it.
    #[test]
    fn a_directory_re_including_below_itself_is_not_excluded() {
        let path = RelativePath::new_from_initial_path("x").expect("valid path");
        let excluded = |filter: &Filter| {
            to_subtree_verdict(
                filter,
                FilterStates::ROOT,
                &path,
                false,
                false,
                DiffFlags::TwoViews,
                FilterMode::Full,
                &DiffWalkStats::default(),
            )
            .1
        };
        let mut re_including = excluding("x");
        re_including
            .view
            .add_inclusion("/x/keep")
            .expect("view inclusion");

        assert!(
            excluded(&excluding("x")),
            "the rule alone must exclude the directory"
        );
        assert!(
            !excluded(&re_including),
            "a re-inclusion below must keep the directory"
        );
    }

    /// `is_file` alone decides which of the two questions is put, and a directory-only rule
    /// makes the two answers diverge.
    ///
    /// This is the shape a link arrives in: neither side of the pairing is a file, so the
    /// subtree question is asked of it, and the rule reaches what a mount holds without
    /// matching the mount. The node question, which the type-change branch puts, answers the
    /// other way -- so neither call stands in for the other.
    #[test]
    fn a_directory_only_rule_answers_the_two_questions_differently() {
        let mut filter = Filter::default();
        filter.view.add_exclusion("/x/").expect("view exclusion");
        let path = stepped_path();
        let excluded = |was_file, is_file| {
            to_subtree_verdict(
                &filter,
                FilterStates::ROOT,
                &path,
                was_file,
                is_file,
                DiffFlags::TwoViews,
                FilterMode::Full,
                &DiffWalkStats::default(),
            )
            .1
        };

        assert!(excluded(false, false), "the subtree question must exclude");
        assert!(
            !excluded(true, true),
            "the node question must not match what is no directory"
        );
    }

    /// How deep the fixture tree runs. Every level is a paired directory the walk dispatches, so
    /// a walk with no budget left takes all of them from its own queue.
    ///
    /// Deep enough that walking them from the frames that found them would not fit a thread's
    /// stack: a walk that recursed instead of queueing aborts the run here, at the 103,504 bytes
    /// a level of that cost when it was measured, rather than passing quietly.
    const DEPTH: u32 = 30;
    /// Files in each directory of the fixture tree.
    const FILES: u32 = 2;

    /// The name of the directory at `level` of the chain.
    fn directory_name(level: u32) -> String {
        format!("d{level:02}")
    }

    /// The name of file `index` in a directory of the chain.
    fn file_name(index: u32) -> String {
        format!("f{index}.bin")
    }

    /// A file node addressing `content`, which is all the walk reads of a file: the content
    /// itself is never fetched, so nothing stands behind the address but this.
    fn file_node(content: &[u8]) -> Node {
        Node {
            flags: NodeFlags::File.bits(),
            address: Address::zero_context_hash(crate::hash::hash_slice(content)),
            size: content.len() as u64,
            ..Default::default()
        }
    }

    /// A state holding a chain of [`DEPTH`] directories with [`FILES`] files in each, the
    /// deepest of them holding `content` and the rest holding nothing.
    ///
    /// Staged rather than committed, which is what makes every directory of it worth
    /// descending: the nodes carry no computed address, so the walk pairs them on the staged
    /// flag instead.
    async fn chain_state(repository: &Arc<RepositoryContext>, content: &[u8]) -> Arc<State> {
        let state = Arc::new(State::new());
        let stage = async |path: RelativePath, node| {
            crate::stage::stage_single_node(
                repository.clone(),
                state.clone(),
                path,
                node,
                Arc::default(),
                None,
                FilterMode::empty(),
            )
            .await
            .expect("a staged node");
        };
        let mut directory = RelativePath::new();
        for level in 0..DEPTH {
            directory = directory.push_into_buf(directory_name(level)).freeze();
            stage(directory.clone(), Node::default()).await;
            for file in 0..FILES {
                let deepest = level + 1 == DEPTH && file + 1 == FILES;
                stage(
                    directory.push_into_buf(file_name(file)).freeze(),
                    file_node(if deepest { content } else { b"" }),
                )
                .await;
            }
        }
        state
    }

    /// The path of the one file [`chain_state`] addresses differently for a different
    /// `content`, which is the deepest file of the chain.
    fn deepest_file() -> String {
        let mut path = (0..DEPTH).map(directory_name).collect::<Vec<_>>();
        path.push(file_name(FILES - 1));
        path.join("/")
    }

    /// A repository with no working tree, which is all a state-to-state walk needs.
    async fn walk_repository() -> Arc<RepositoryContext> {
        let (immutable_store, mutable_store, _execution) =
            crate::fs::filesystem_provider::tests::test_store_create()
                .await
                .expect("test stores");
        Arc::new(RepositoryContext::new(
            crate::repository::test_helpers::default_repository_creation_args(
                immutable_store,
                mutable_store,
            ),
        ))
    }

    /// What a walk between the two states emitted, sorted, and how many directories it stood
    /// in.
    ///
    /// Each change is its action letter, whether the walk measured the content as changed, and
    /// its path. The letter alone does not carry the second: a paired file is reported `Keep`,
    /// which reads as `M`, whether its content moved or only its staged flag did.
    async fn walk(
        repository: &Arc<RepositoryContext>,
        from: &Arc<State>,
        to: &Arc<State>,
    ) -> (Vec<(String, bool, String)>, u64) {
        let (repository_from, repository_to) = (repository.clone(), repository.clone());
        let (from, to) = (from.clone(), to.clone());
        let mut walk = ChangeStream::spawn(async move |changes| {
            crate::state::diff(
                repository_from,
                from,
                repository_to,
                to,
                None,
                None,
                &changes,
                FilterMode::Full,
            )
            .await
        });
        let mut emitted = Vec::new();
        while let Some(change) = walk.next().await {
            emitted.push((
                change.action.as_string_short().to_string(),
                change.flags.contains(change::Flags::Modify),
                change.path().as_str().to_string(),
            ));
        }
        let stats = walk.finish().await.expect("a walk of the two states");
        emitted.sort();
        (emitted, stats.directories_entered.load(Ordering::Relaxed))
    }

    /// One semaphore for the process, so that what bounds one walk bounds every walk running
    /// beside it. A budget minted per walk would let each of them fan out as far as one walk
    /// may.
    #[test]
    fn the_fan_out_budget_is_one_semaphore() {
        let first: *const Semaphore = Arc::as_ptr(subtree_task_semaphore());
        let second: *const Semaphore = Arc::as_ptr(subtree_task_semaphore());
        assert_eq!(first, second, "the budget must not be minted per caller");
    }

    /// A walk that cannot spawn takes every subtree from [`SubtreeWork::pending`] instead, and
    /// reports the same changes over the same directories as a walk that spawns them all.
    ///
    /// No permit is free here, so the queue is the only way down: each of the [`DEPTH`]
    /// directories below the root is queued by the directory above it and walked by the one task
    /// the walk has. That every one of them was walked is what the count says, and the chain is
    /// long enough that walking them from the frames that queued them would not fit a thread's
    /// stack.
    ///
    /// The timeout is the point of the test as much as the equality is: a task holds its permit
    /// until the subtrees it spawned finish, so a walk that waited for one would wait on a
    /// descendant that cannot start, and would never end rather than fail.
    #[tokio::test]
    async fn a_walk_with_no_budget_left_walks_the_same_tree() {
        let execution = crate::fs::filesystem_provider::tests::setup_test_execution();
        lore_base::runtime::LORE_CONTEXT
            .scope(execution, async {
                let repository = walk_repository().await;
                let from = chain_state(&repository, b"one").await;
                let to = chain_state(&repository, b"two").await;

                let spawned = walk(&repository, &from, &to).await;

                let _budget = subtree_task_semaphore()
                    .clone()
                    .acquire_many_owned(MAX_CONCURRENT_TREE_TASKS as u32)
                    .await
                    .expect("the whole fan-out budget");
                assert_eq!(
                    subtree_task_semaphore().available_permits(),
                    0,
                    "the walk below must meet a budget that is actually spent"
                );
                let queued =
                    tokio::time::timeout(Duration::from_secs(120), walk(&repository, &from, &to))
                        .await
                        .expect("a walk that never waits for a permit it cannot be given");

                assert_eq!(
                    spawned.1,
                    u64::from(DEPTH) + 1,
                    "the walk must stand in the root and every directory below it"
                );
                assert_eq!(
                    queued.1, spawned.1,
                    "every queued subtree must be walked, and counted by the task that walked it"
                );
                assert_eq!(
                    queued.0, spawned.0,
                    "a subtree taken from the queue must report what a task of its own would have"
                );
                assert!(
                    spawned.0.contains(&("M".to_string(), true, deepest_file())),
                    "the one file the two states address differently must be the one reported \
                     with its content changed"
                );
            })
            .await;
    }
}
