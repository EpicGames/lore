// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use bitflags::bitflags;
use futures::future::Either;
use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::task::JoinError;
use tokio::task::JoinSet;

use crate::MAX_CONCURRENT_TREE_TASKS;
use crate::change;
use crate::change::NodeChange;
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
use crate::state::compute_change_flags;
use crate::state::emit_change;
use crate::state::named_node_sort;
use crate::state::stream::emit;
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
    #[lore_macro::test_pub]
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
///
/// Each side asks about `path` as the node it holds there, a link as a directory, and as the other
/// side's node where it holds none, so a type change asks its delete side and its add side each as
/// what it is.
///
/// Returns [`diff_subtree_walk`]'s future itself, seeded when called, or a ready one
/// where both sides drop `path`: a future of its own would hold the sides again
/// beside the walk they are moved into.
pub fn diff_subtree(
    from: NodeChangeState,
    to: NodeChangeState,
    path: RelativePath,
    graft: Option<Arc<GraftOracle>>,
    changes: &ChangeSender,
    filter_mode: FilterMode,
) -> impl Future<Output = Result<DiffWalkStats, StateError>> {
    // A repository's root carries no staged or dirty flag, so a walk rooted at one keeps it. A
    // root only the from side holds is one the to side was built on, and one staged is tracked;
    // where the root is only marked dirty, the to side's tree is asked whether it tracks it, on a
    // cold path of its own, and only where the ignore rules would leave it out.
    let changed = to.flags.contains(NodeFlags::Staged) || !to.mapping.node.is_valid_node_id();
    if !changed
        && filter_mode.contains(FilterMode::KeepTracked)
        && to.flags.contains(NodeFlags::Dirty)
    {
        let to_filter = &to.mapping.repository.filter;
        let (_, to_is_directory) = root_kinds(&from, &to);
        if to_filter
            .child_excludes_tree(
                to_filter.parent_exclusion_states(&path),
                &path,
                to_is_directory,
                filter_mode.at(false),
            )
            .1
        {
            let changes = changes.clone();
            return Either::Right(Box::pin(async move {
                let tracked = to
                    .mapping
                    .state
                    .holds_tracked_change(&to.mapping.repository, to.mapping.node, to.flags)
                    .await?;
                seeded_subtree(from, to, path, graft, &changes, filter_mode, tracked).await
            })
                as std::pin::Pin<
                    Box<dyn Future<Output = Result<DiffWalkStats, StateError>> + Send>,
                >);
        }
    }
    Either::Left(seeded_subtree(
        from,
        to,
        path,
        graft,
        changes,
        filter_mode,
        changed,
    ))
}

/// [`diff_subtree`] once whether the root holds a tracked change, `root_changed`, is settled.
fn seeded_subtree(
    from: NodeChangeState,
    to: NodeChangeState,
    path: RelativePath,
    graft: Option<Arc<GraftOracle>>,
    changes: &ChangeSender,
    filter_mode: FilterMode,
    root_changed: bool,
) -> impl Future<Output = Result<DiffWalkStats, StateError>> {
    let from_filter = &from.mapping.repository.filter;
    let to_filter = &to.mapping.repository.filter;
    let flags = DiffFlags::between(from_filter, to_filter);
    let kinds = root_kinds(&from, &to);
    let (from_states, to_states, excluded) = seed_sides(
        from_filter,
        to_filter,
        &path,
        kinds,
        flags,
        filter_mode.at(root_changed),
    );
    if excluded {
        lore_debug!("Excluded by filter: {}", path.as_str());
        return Either::Left(std::future::ready(Ok(DiffWalkStats::default())));
    }

    let depth = query_depth(path.as_lowercase_str());
    Either::Right(diff_subtree_walk(
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
        changes.clone(),
        filter_mode,
        None,
    ))
}

/// The verdicts a walk over `path` starts from, one per side, and whether the walk
/// drops `path`.
///
/// One filter held by both sides is asked once where both sides ask about `path` as the same
/// kind: the same rules put the same question about the same path answer the same way, and every
/// caller that is not moving the view holds one filter. `kinds` says whether the from side and the
/// to side ask about `path` as a directory.
///
/// `path` is dropped where both sides exclude it, and only there is the exclusion
/// announced. The to side is asked last, and in the announcing form only once the
/// from side has excluded, so the drop and the announcement are one condition.
#[lore_macro::test_pub]
fn seed_sides(
    from_filter: &Arc<Filter>,
    to_filter: &Arc<Filter>,
    path: &RelativePath,
    kinds: (bool, bool),
    flags: DiffFlags,
    mode: FilterMode,
) -> (FilterStates, FilterStates, bool) {
    let (from_is_directory, to_is_directory) = kinds;
    let separate = flags.contains(DiffFlags::TwoViews) || from_is_directory != to_is_directory;
    let from_seed =
        separate.then(|| seed_states(from_filter, path, from_is_directory, false, mode));
    let announce = from_seed.is_none_or(|(_, excluded)| excluded);
    let to_seed = seed_states(to_filter, path, to_is_directory, announce, mode);
    let (from_states, from_excluded) = from_seed.unwrap_or(to_seed);
    (from_states, to_seed.0, from_excluded && to_seed.1)
}

/// Whether the from side and the to side of a walk's root ask about its path as a directory: each
/// as the node it holds, a link as a directory, and as the other side's node where it holds none.
fn root_kinds(from: &NodeChangeState, to: &NodeChangeState) -> (bool, bool) {
    let from_directory = !from.flags.contains(NodeFlags::File);
    let to_directory = !to.flags.contains(NodeFlags::File);
    match (
        from.mapping.node.is_valid_or_root_node_id(),
        to.mapping.node.is_valid_or_root_node_id(),
    ) {
        (true, false) => (from_directory, from_directory),
        (false, true) => (to_directory, to_directory),
        _ => (from_directory, to_directory),
    }
}

/// The states a walk rooted at `path` threads into its children, and whether
/// `filter` excludes `path` with everything below it. `is_directory` is whether
/// `path` names a directory.
///
/// `path`'s own ancestors are folded here, since a walk starting at it has none
/// behind it.
///
/// `announce` emits the filter-exclude event where the verdict excludes.
#[lore_macro::test_pub]
fn seed_states(
    filter: &Filter,
    path: &RelativePath,
    is_directory: bool,
    announce: bool,
    mode: FilterMode,
) -> (FilterStates, bool) {
    let parent = filter.parent_exclusion_states(path);
    if announce {
        filter.child_emit_excludes(parent, path, is_directory, mode)
    } else {
        filter.child_excludes_tree(parent, path, is_directory, mode)
    }
}

/// A paired directory a walk has still to descend into.
///
/// Held by the task that will walk it rather than walked from the frame that found it, which is
/// what keeps a walk's stack the depth of one directory however deep the tree runs.
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
type SubtreeTasks = JoinSet<Result<DiffWalkStats, StateError>>;

/// The subtree walks one task has in flight and the ones it has still to walk itself.
#[lore_macro::test_pub]
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

#[lore_macro::test_pub]
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
            lore_spawn!(
                tasks,
                crate::subtask!(diff_subtree_walk(
                    subtree,
                    flags,
                    changes.clone(),
                    filter_mode,
                    Some(permit)
                ))
            );
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
///
/// `permit` is the budget a spawned walk holds until it has drained its tasks; the walk its
/// caller awaits holds none. Not an `async fn`: the queue is made when called, so the future
/// holds `first` once, in the queue.
#[lore_macro::test_pub]
fn diff_subtree_walk(
    first: PendingSubtree,
    flags: DiffFlags,
    changes: ChangeSender,
    filter_mode: FilterMode,
    permit: Option<OwnedSemaphorePermit>,
) -> impl Future<Output = Result<DiffWalkStats, StateError>> {
    let mut work = SubtreeWork {
        tasks: SubtreeTasks::new(),
        pending: vec![first],
    };
    async move {
        let stats = DiffWalkStats::default();

        // Run the walk in a helper so any `?` early-out still hits the
        // drain below — otherwise the JoinSet drops with subtree-diff
        // tasks still running, leaking the Arc<RepositoryContext> clones.
        let work_result =
            walk_pending_subtrees(&mut work, flags, &changes, filter_mode, &stats).await;
        let drain_result = lore_drain_tasks!(work.tasks, StateError::internal("Task failure"));
        drop(permit);
        work_result?;
        drain_result?;
        Ok(stats)
    }
}

/// Walks every queued subtree, and every subtree they queue, then joins the tasks they spawned.
///
/// Each directory's future is awaited in place, so the directories one task walks reuse the task's
/// own allocation rather than taking a box apiece. The queue is popped by a `let`-`else` rather
/// than a `while let`, whose scrutinee would stay reserved beside the directory's future.
#[lore_macro::test_pub]
async fn walk_pending_subtrees(
    work: &mut SubtreeWork,
    flags: DiffFlags,
    changes: &ChangeSender,
    filter_mode: FilterMode,
    stats: &DiffWalkStats,
) -> Result<(), StateError> {
    loop {
        let Some(subtree) = work.pending.pop() else {
            break;
        };
        diff_subtree_node(subtree, flags, changes, filter_mode, work, stats).await?;
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
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
#[derive(Clone, Copy)]
struct DiffStates {
    from: FilterStates,
    to: FilterStates,
}

/// Where the walk stands on both sides: the paths, and the verdicts they were
/// reached with.
#[lore_macro::test_pub]
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
///
/// Not an `async fn`: `subtree` is taken apart when called, so the future holds its parts
/// once rather than the subtree beside them.
#[lore_macro::test_pub]
fn diff_subtree_node(
    subtree: PendingSubtree,
    flags: DiffFlags,
    changes: &ChangeSender,
    filter_mode: FilterMode,
    work: &mut SubtreeWork,
    stats: &DiffWalkStats,
) -> impl Future<Output = Result<(), StateError>> {
    let PendingSubtree {
        from,
        to,
        mut cursor,
        graft,
    } = subtree;
    async move {
        // If path is a file then treat this as a call for the parent with only one child.
        let (from_nodes, to_nodes, popped) =
            find_sorted_children(&mut cursor.paths, &from, &to).await?;
        if popped {
            cursor.reseed(&from, &to);
        }

        stats.entered();
        diff_subtree_node_walk(
            &from,
            &to,
            &cursor.paths,
            cursor.states,
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
}

/// Walk paired/solo from/to children, dispatching paired-node diffs through
/// [`dispatch_subtree_diff`] and emitting solo changes as they are found.
///
/// Each (case-normalized) name is in one of three buckets:
/// 1. In `to_nodes` but not `from_nodes` → Added or Moved.
/// 2. In `from_nodes` but not `to_nodes` → Deleted or was Moved.
/// 3. In both → Modified, case-only rename, or unchanged.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
fn diff_subtree_node_walk(
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
) -> impl Future<Output = Result<(), StateError>> {
    async move {
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
            while to_index < to_nodes.children.len()
                && to_nodes.children[to_index].name < from_named_node.name
            {
                add_change_for_solo_to_node(context, from, to_index).await?;
                to_index += 1;
            }

            let paired = to_nodes
                .children
                .get(to_index)
                .filter(|to_named_node| to_named_node.name == from_named_node.name);
            stats.queried();
            let Some((from_node_search, from_node_states)) = get_filtered_node_and_path(
                from_nodes,
                from_named_node.node,
                &paths.from,
                states.from,
                filter_mode,
                // The from side is the tree the to side was built on, so its node is tracked,
                // and what is staged or marked on its counterpart is the change below it.
                async |flags: NodeFlags| match paired {
                    _ if flags.intersects(NodeFlags::Staged | NodeFlags::Dirty) => Ok(true),
                    Some(to_named_node) => {
                        to_nodes
                            .state
                            .holds_tracked_change(
                                &to_nodes.repository,
                                to_named_node.node,
                                to_named_node.flags(),
                            )
                            .await
                    }
                    None => Ok(false),
                },
            )
            .await?
            else {
                continue;
            };

            if let Some(to_named_node) = paired {
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
            } else {
                add_change_for_solo_from_node(
                    context,
                    from_named_node,
                    to,
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
        async |flags: NodeFlags| {
            to_nodes
                .state
                .holds_tracked_change(&to_nodes.repository, to_named_node.node, flags)
                .await
        },
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
#[lore_macro::test_pub]
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

/// The add or the delete of a paired directory alone, where the two views place it on disk
/// differently.
///
/// A directory a view excludes stands on disk only for content below it. The walk below reports
/// that content, and its writes and deletes create and empty the directory. Where either revision
/// holds the directory with no children, one side has nothing below it to answer for it, so it is
/// reported here: added where the target view takes it in, deleted where the target view leaves it
/// out. A delete of a directory still holding something keeps it. The change carries no hierarchy.
fn presence_change(
    from_node: &Node,
    to_node: &Node,
    from_states: FilterStates,
    to_states: FilterStates,
    mode: FilterMode,
) -> Option<change::FileAction> {
    if from_node.child().is_some() && to_node.child().is_some() {
        return None;
    }
    match (from_states.excluded(mode), to_states.excluded(mode)) {
        (true, false) => Some(change::FileAction::Add),
        (false, true) => Some(change::FileAction::Delete),
        _ => None,
    }
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
    // Node ids only compare within one revision, so across a pin move the mounts are compared by
    // path below.
    let mount_equal = !hash_equal || from_node.mounts_same_source(&to_node);
    let from_mode = from_node.mode;
    let to_mode = to_node.mode;
    let mode_equal = from_mode == to_mode;
    let is_modify = !hash_equal || !mode_equal || !mount_equal;
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

        // The to side passes the ignore rules where its tree tracks a change at or below the
        // node, which is asked only where the rules would leave the node out.
        let to_filter_mode = to_nodes
            .repository
            .filter
            .tracked_mode(states.to, &subpath, !to_node.is_file(), filter_mode, || {
                to_nodes.state.holds_tracked_change(
                    &to_nodes.repository,
                    to_named_node.node,
                    NodeFlags::from_bits_retain(to_node.flags),
                )
            })
            .await?;
        let (to_subtree_states, to_excludes_subtree) = to_subtree_verdict(
            &to_nodes.repository.filter,
            states.to,
            &subpath,
            from_type == LoreNodeType::File,
            to_type == LoreNodeType::File,
            flags,
            to_filter_mode,
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

            if to_type == LoreNodeType::Directory
                && let Some(action) = presence_change(
                    from_node,
                    &to_node,
                    from_node_states,
                    to_subtree_states,
                    filter_mode,
                )
            {
                emit(changes, || NodeChange {
                    action,
                    flags: compute_change_flags(&to_node),
                    from: from.clone(),
                    to: to.clone(),
                })
                .await?;
            }

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
                || !mount_equal
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

                        // The two subtrees of a remount, or of a link replaced by one to another
                        // repository, don't correspond, so the link node is reported instead.
                        (!is_staged || has_staged_children)
                            && !(from_node.is_link()
                                && crate::link::link_remounted(
                                    &linked_repository,
                                    from_node,
                                    &to_node,
                                    &linked_state,
                                )
                                .await?)
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
                .child_excludes_tree(states.to, &subpath, to_node.is_directory(), to_filter_mode);
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
            from_nodes.children.push(StateNamedNode::new(
                from.mapping.node,
                0,
                node.name_hash,
                NodeFlags::from_bits_retain(node.flags),
            ));
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
            to_nodes.children.push(StateNamedNode::new(
                to.mapping.node,
                0,
                node.name_hash,
                NodeFlags::from_bits_retain(node.flags),
            ));
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
            lore_spawn!(crate::subtask!(async move {
                let mut nodes = state
                    .collect_children_unsorted(
                        repository, node, false, /* No deleted nodes */
                        true,  /* Include links */
                    )
                    .await?;
                named_node_sort(&mut nodes.children);
                Ok::<_, StateError>(nodes)
            }))
        };
        let to_nodes = {
            let repository = to.mapping.repository.clone();
            let state = to.mapping.state.clone();
            let node = to.mapping.node;
            lore_spawn!(crate::subtask!(async move {
                let mut nodes = state
                    .collect_children_unsorted(
                        repository, node, true, /* Include deleted nodes */
                        true, /* Include links */
                    )
                    .await?;
                named_node_sort(&mut nodes.children);
                Ok::<_, StateError>(nodes)
            }))
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
///
/// The node is asked about in the mode [`FilterMode::at`] answers, past the ignore rules where
/// `tracked` answers, from the node's flags, that its tree tracks a change at or below it. It is
/// asked only where the rules would leave the node out.
pub async fn get_filtered_node_and_path<F>(
    nodes: &StateChildrenNodes,
    node_id: NodeID,
    path: &RelativePath,
    parent_states: FilterStates,
    filter_mode: FilterMode,
    tracked: impl FnOnce(NodeFlags) -> F,
) -> Result<Option<(NodeSearchResult, FilterStates)>, StateError>
where
    F: Future<Output = Result<bool, StateError>>,
{
    let Some(result) = get_node_and_path(nodes, node_id, path).await? else {
        return Ok(None);
    };
    let filter = &nodes.repository.filter;
    let is_directory = result.node.is_directory();
    let flags = NodeFlags::from_bits_retain(result.node.flags);
    let mode = filter
        .tracked_mode(
            parent_states,
            &result.path,
            is_directory,
            filter_mode,
            || tracked(flags),
        )
        .await?;
    let (states, excluded) =
        filter.child_emit_excludes(parent_states, &result.path, is_directory, mode);
    if excluded {
        lore_trace!("Path excluded by filter: {}", path);
        return Ok(None);
    }
    Ok(Some((result, states)))
}
