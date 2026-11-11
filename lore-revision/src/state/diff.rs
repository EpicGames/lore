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
#[cfg(target_os = "linux")]
use crate::change::FileAction;
use crate::change::NodeChange;
use crate::change::NodeChangeState;
use crate::filter::Filter;
use crate::filter::FilterMode;
use crate::filter::FilterStates;
use crate::filter::query_depth;
#[cfg(target_os = "linux")]
use crate::fs::filesystem_provider::FileInfo;
#[cfg(target_os = "linux")]
use crate::fs::filesystem_provider::FilesystemDiffIntent;
#[cfg(target_os = "linux")]
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::interface::LoreNodeType;
use crate::lore::Address;
use crate::lore::RepositoryId;
use crate::lore_debug;
use crate::lore_drain_tasks;
use crate::lore_trace;
use crate::lore_warn;
#[cfg(target_os = "linux")]
use crate::node::INVALID_NODE;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeExtFlags;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeIDExt;
#[cfg(target_os = "linux")]
use crate::node::ROOT_NODE;
use crate::repository::RepositoryContext;
#[cfg(target_os = "linux")]
use crate::state::AttachOutcome;
use crate::state::ChangeSender;
#[cfg(target_os = "linux")]
use crate::state::ChangeStream;
#[cfg(target_os = "linux")]
use crate::state::Clear;
#[cfg(target_os = "linux")]
use crate::state::Ext;
#[cfg(target_os = "linux")]
use crate::state::FilesystemDiffStats;
#[cfg(target_os = "linux")]
use crate::state::Mark;
#[cfg(target_os = "linux")]
use crate::state::NodeComparison;
use crate::state::NodeMapping;
#[cfg(target_os = "linux")]
use crate::state::SettledAction;
use crate::state::State;
use crate::state::StateChildrenNodes;
use crate::state::StateError;
use crate::state::StateNamedNode;
#[cfg(target_os = "linux")]
use crate::state::WalkDiscards;
use crate::state::add_change;
use crate::state::add_change_where;
#[cfg(target_os = "linux")]
use crate::state::apply_pending_discards;
use crate::state::compute_change_flags;
use crate::state::emit_change;
#[cfg(target_os = "linux")]
use crate::state::file_matches_node;
#[cfg(target_os = "linux")]
use crate::state::mark_settled;
use crate::state::named_node_sort;
#[cfg(target_os = "linux")]
use crate::state::node_change_state;
#[cfg(target_os = "linux")]
use crate::state::os_diff::ensure_scan_dir_chain;
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
        /// The walk settles the from side against the to side, so a node the from side alone
        /// marks dirty is reported, a directory it alone marks dirty is descended, and a
        /// directory it marks added or staged for delete is reported.
        const SettlesFrom = 0b10;
        /// The to side is a filesystem State that changes under the walk, and the from side the
        /// tree lore holds against it. A to child is reported only while it is live under the
        /// parent and the name it was listed with, and the from side is listed with its staged
        /// deletes, which a name the to side holds again pairs with.
        const FilesystemTo = 0b100;
        /// The walk leaves link pins to the link registry: a link it would descend, one the
        /// caller can read that mounts the same subtree of the same repository on both
        /// sides, is neither descended nor reported.
        const LeavesLinkPins = 0b1000;
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
    /// Where the to side changed under the walk since it listed a directory: a to child moved
    /// since the listing, or a delete confirmed by a find since the directory's chain changed.
    /// For tests.
    #[doc(hidden)]
    pub moved_under_walk: AtomicU64,
    /// Of those, the finds made for a delete since the directory's chain changed. For tests.
    #[doc(hidden)]
    pub finds_after_change: AtomicU64,
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

    /// Records one change to the to side met since the walk listed a directory.
    fn moved(&self) {
        self.moved_under_walk.fetch_add(1, Ordering::Relaxed);
    }

    /// Records one find for a delete made since its directory's chain changed.
    fn found_after_change(&self) {
        self.moved();
        self.finds_after_change.fetch_add(1, Ordering::Relaxed);
    }

    /// Folds in what a subtree reported.
    fn append(&self, subtree: DiffWalkStats) {
        self.directories_entered
            .fetch_add(subtree.directories_entered.into_inner(), Ordering::Relaxed);
        self.filter_queries
            .fetch_add(subtree.filter_queries.into_inner(), Ordering::Relaxed);
        self.moved_under_walk
            .fetch_add(subtree.moved_under_walk.into_inner(), Ordering::Relaxed);
        self.finds_after_change
            .fetch_add(subtree.finds_after_change.into_inner(), Ordering::Relaxed);
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
    diff_subtree_flagged(
        from,
        to,
        path,
        graft,
        DiffFlags::empty(),
        changes,
        filter_mode,
    )
}

/// [`diff_subtree`] with `extra` joined to the flags the sides derive.
#[lore_macro::test_pub]
pub(crate) fn diff_subtree_flagged(
    from: NodeChangeState,
    to: NodeChangeState,
    path: RelativePath,
    graft: Option<Arc<GraftOracle>>,
    extra: DiffFlags,
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
                let tracked = if extra.contains(DiffFlags::FilesystemTo) {
                    from_tracks(&from, to_is_directory).await?
                } else {
                    to.mapping
                        .state
                        .holds_tracked_change(&to.mapping.repository, to.mapping.node, to.flags)
                        .await?
                };
                let filter_mode = filter_mode.at_staged(false);
                seeded_subtree(from, to, path, graft, extra, &changes, filter_mode, tracked).await
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
        extra,
        changes,
        filter_mode,
        changed,
    ))
}

/// Whether lore's tree, the from side of a provider diff, tracks the node it holds at the root of
/// the walk as the kind the working tree holds there, a directory where `is_directory`; false
/// where it holds none there.
async fn from_tracks(from: &NodeChangeState, is_directory: bool) -> Result<bool, StateError> {
    if !from.mapping.node.is_valid_or_root_node_id() {
        return Ok(false);
    }
    from.mapping
        .state
        .tracks_as(
            &from.mapping.repository,
            from.mapping.node,
            from.flags,
            is_directory,
        )
        .await
}

/// [`diff_subtree_flagged`] once whether the root holds a tracked change, `root_changed`, is
/// settled.
#[allow(clippy::too_many_arguments)]
fn seeded_subtree(
    from: NodeChangeState,
    to: NodeChangeState,
    path: RelativePath,
    graft: Option<Arc<GraftOracle>>,
    extra: DiffFlags,
    changes: &ChangeSender,
    filter_mode: FilterMode,
    root_changed: bool,
) -> impl Future<Output = Result<DiffWalkStats, StateError>> {
    let from_filter = &from.mapping.repository.filter;
    let to_filter = &to.mapping.repository.filter;
    let flags = DiffFlags::between(from_filter, to_filter) | extra;
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
        let to_listed = listed_changes(&to, flags).await?;
        // If path is a file then treat this as a call for the parent with only one child.
        let (from_nodes, to_nodes, popped) =
            find_sorted_children(&mut cursor.paths, &from, &to, flags).await?;
        let to_parent = (!popped).then_some((to.mapping.node, to_listed));
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
            to_parent,
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
    to_parent: Option<(NodeID, Option<u32>)>,
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
            flags,
            to_parent,
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
                filter_mode.at_staged(paired.is_some_and(|to_named_node| {
                    to_named_node.flags().contains(NodeFlags::Staged)
                })),
                // The from side is the tree the to side was built on, so its node is tracked,
                // and what is staged or marked on its counterpart is the change below it. Where
                // the to side is a working tree's State, the from side is lore's tree, which
                // answers whether it tracks the node; the working tree's State answers nothing
                // of what lore tracks.
                async |node_flags: NodeFlags| match paired {
                    _ if flags.contains(DiffFlags::FilesystemTo) => {
                        from_nodes
                            .state
                            .tracks(&from_nodes.repository, from_named_node.node, node_flags)
                            .await
                    }
                    _ if node_flags.intersects(NodeFlags::Staged | NodeFlags::Dirty) => Ok(true),
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
    flags: DiffFlags,
    /// The directory the to children stand in, with its chain change count read before the walk
    /// listed it where the to side changes under the walk; `None` where the walk stands at a
    /// file, which the caller found by name.
    to_parent: Option<(NodeID, Option<u32>)>,
}

/// The chain change count of the to directory, read before the walk lists it, where the to side
/// changes under the walk.
async fn listed_changes(to: &NodeChangeState, flags: DiffFlags) -> Result<Option<u32>, StateError> {
    let node = to.mapping.node;
    if !flags.contains(DiffFlags::FilesystemTo) || !node.is_valid_or_root_node_id() {
        return Ok(None);
    }
    let block = to
        .mapping
        .state
        .block(to.mapping.repository.clone(), NodeBlock::index(node))
        .await?;
    Ok(Some(block.chain_changes(Node::index(node))))
}

/// Whether the to directory holds a child of `name_hash` after all, where the to side changes
/// under the walk and the directory's chain changed since the walk listed it, so a listing that
/// lacked the name may have missed a node moved during it. A find answers, and an error other than
/// the node not being found counts as held, so a delete is never reported on a find that failed.
/// A to directory the caller's find did not find holds nothing, and a symlink there is nothing
/// held, as the listing skips one ([`versioned_on_a_working_tree`]).
async fn held_after_listing(context: DiffContext<'_>, name_hash: u64) -> Result<bool, StateError> {
    if !context.flags.contains(DiffFlags::FilesystemTo) {
        return Ok(false);
    }
    let Some((parent, listed)) = context.to_parent else {
        return Ok(false);
    };
    if !parent.is_valid_or_root_node_id() {
        return Ok(false);
    }
    let to_nodes = context.to_nodes;
    if let Some(listed) = listed {
        let block = to_nodes
            .state
            .block(to_nodes.repository.clone(), NodeBlock::index(parent))
            .await?;
        if block.chain_changes(Node::index(parent)) == listed {
            return Ok(false);
        }
        context.stats.found_after_change();
    }
    let node = match to_nodes
        .state
        .find_subnode(to_nodes.repository.clone(), parent, name_hash)
        .await
    {
        Ok(node) if node.is_valid_node_id() => node,
        Ok(_) | Err(crate::state::StateErrors::NodeNotFound(_)) => return Ok(false),
        Err(_) => return Ok(true),
    };
    Ok(
        match to_nodes.state.node(to_nodes.repository.clone(), node).await {
            Ok(held) => versioned_on_a_working_tree(&held),
            Err(error) => !error.is_node_not_found(),
        },
    )
}

/// Whether a filesystem State's node is one a working tree versions: a symlink is not, as an
/// ordinary clone's walk skips one (`crate::fs::os::file_list_item`), so a diff against such a
/// State takes a symlink as nothing there.
#[lore_macro::test_pub]
pub(crate) fn versioned_on_a_working_tree(node: &Node) -> bool {
    !node.ext_flags().contains(NodeExtFlags::Symlink)
}

/// Whether a to child moved since the walk listed it, where the to side changes under the walk:
/// it is discarded, under another directory, or holds another name than it was listed with.
/// Such a child is left for the next diff, as a children walk may omit a node moved during it.
fn moved_since_listed(
    context: DiffContext<'_>,
    listed: &StateNamedNode,
    node: &Node,
    name: &str,
) -> bool {
    let moved = context.flags.contains(DiffFlags::FilesystemTo)
        && (node.is_discarded()
            || context
                .to_parent
                .is_some_and(|(parent, _)| node.parent != parent)
            || crate::hash::hash_string(name) != listed.name);
    if moved {
        context.stats.moved();
    }
    moved
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
    if context.flags.contains(DiffFlags::FilesystemTo)
        && (from_node.is_staged_delete()
            // Boxed: a cold path only a filesystem diff takes, which unboxed would sit in every
            // walk's future.
            || Box::pin(held_after_listing(context, from_named_node.name)).await?)
    {
        return Ok(());
    }
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
        ..
    } = context;
    let to_named_node = &to_nodes.children[to_index];
    stats.queried();
    // A node only a working tree's State holds is one lore's tree lacks, so untracked whatever
    // that State says of it.
    let working_tree = context.flags.contains(DiffFlags::FilesystemTo);
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
        filter_mode.at_staged(to_named_node.flags().contains(NodeFlags::Staged)),
        async |flags: NodeFlags| {
            if working_tree {
                return Ok(false);
            }
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
    if moved_since_listed(context, to_named_node, &to_node, subpath.name()) {
        return Ok(());
    }

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

    add_change_where(
        from,
        to,
        file_action,
        change::Flags::None,
        changes,
        filter_mode,
        to_node_states,
        context.flags.contains(DiffFlags::FilesystemTo),
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
        ..
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
    let is_dirty =
        to_node.is_dirty() || (flags.contains(DiffFlags::SettlesFrom) && from_node.is_dirty());
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
        if moved_since_listed(context, to_named_node, &to_node, &to_name) {
            return Ok(());
        }
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
        // node, which is asked only where the rules would leave the node out. A working tree's
        // State answers nothing of what lore tracks, and its node is paired with one of lore's
        // tree, which passed the rules on the from side: tracked as the kind that node is, so an
        // entry of another kind is new content the rules answer for (ADR-00027).
        let working_tree = flags.contains(DiffFlags::FilesystemTo);
        let to_filter_mode = to_nodes
            .repository
            .filter
            .tracked_mode(
                states.to,
                &subpath,
                !to_node.is_file(),
                filter_mode.at_staged(to_node.is_staged()),
                || async {
                    if working_tree {
                        return Ok(same_type);
                    }
                    to_nodes
                        .state
                        .holds_tracked_change(
                            &to_nodes.repository,
                            to_named_node.node,
                            NodeFlags::from_bits_retain(to_node.flags),
                        )
                        .await
                },
            )
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
            let settles_from = flags.contains(DiffFlags::SettlesFrom)
                && (from_node.is_dirty_add() || from_node.is_staged_delete());
            let node_reported = !mode_equal || is_rename || conflicted_directory || settles_from;
            if node_reported {
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
                        if !node_reported {
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
                    } else if flags.contains(DiffFlags::LeavesLinkPins) {
                        lore_debug!("Diff node {subpath} pin left to the link registry");
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
                add_change_where(
                    from,
                    to,
                    change::FileAction::Add,
                    change::Flags::None,
                    changes,
                    filter_mode,
                    to_node_states,
                    flags.contains(DiffFlags::FilesystemTo),
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
    flags: DiffFlags,
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
            && (!flags.contains(DiffFlags::FilesystemTo) || versioned_on_a_working_tree(&node))
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
            let with_deleted = flags.contains(DiffFlags::FilesystemTo);
            lore_spawn!(crate::subtask!(async move {
                let mut nodes = state
                    .collect_children_unsorted(repository, node, with_deleted, true)
                    .await?;
                named_node_sort(&mut nodes.children);
                if with_deleted {
                    live_before_deleted(&mut nodes).await?;
                }
                Ok::<_, StateError>(nodes)
            }))
        };
        let to_nodes = {
            let repository = to.mapping.repository.clone();
            let state = to.mapping.state.clone();
            let node = to.mapping.node;
            let versioned_only = flags.contains(DiffFlags::FilesystemTo);
            lore_spawn!(crate::subtask!(async move {
                // Include deleted nodes and links.
                let mut nodes = if versioned_only {
                    state
                        .collect_children_unsorted_where(
                            repository,
                            node,
                            true,
                            true,
                            versioned_on_a_working_tree,
                        )
                        .await?
                } else {
                    state
                        .collect_children_unsorted(repository, node, true, true)
                        .await?
                };
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

/// Orders each run of children sharing a name so the live ones precede those staged for
/// delete, so a name the to side holds pairs with a live node where the from side holds both.
async fn live_before_deleted(nodes: &mut StateChildrenNodes) -> Result<(), StateError> {
    let mut start = 0;
    while start < nodes.children.len() {
        let name = nodes.children[start].name;
        let end = start
            + nodes.children[start..]
                .iter()
                .take_while(|child| child.name == name)
                .count();
        if end - start > 1 {
            let mut run = Vec::with_capacity(end - start);
            for child in &nodes.children[start..end] {
                let node = nodes
                    .state
                    .node(nodes.repository.clone(), child.node)
                    .await?;
                run.push((node.is_staged_delete(), *child));
            }
            run.sort_by_key(|(deleted, _)| *deleted);
            for (slot, (_, child)) in nodes.children[start..end].iter_mut().zip(run) {
                *slot = child;
            }
        }
        start = end;
    }
    Ok(())
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

/// Emits the changes between `from` and the filesystem State `to`, both standing at `path`, as
/// a provider serving its tree from a State answers a filesystem diff, and settles them in `from`
/// as `intent` asks, as a marking walk over a working tree does: a change is marked, a node the
/// tree holds unchanged loses its mark, and an add the tree no longer holds is discarded once the
/// walk has ended, or held on `discards` where given, for a caller running several walks over one
/// tree to discard once all of them have drained.
///
/// `current` is what the working copy last held, which tells an add not yet committed from a
/// tracked file. A local file whose content has no address yet is measured through `operation`,
/// as a walk over a working tree measures a file. A staging intent marks as a marking one does,
/// and lore's stage walk stages what it marked.
///
/// `to` changes under the walk. A child it moved since the walk listed it is left for the next
/// diff, and a delete in a directory whose chain changed since the walk listed it is reported only
/// once a find by name on `to` confirms nothing holds the name, so a rename during the diff never
/// reports a name twice or a name held throughout as deleted.
///
/// The walk runs behind a stream of its own and the changes are settled one at a time as they
/// arrive, so one task makes every mark, and the walk has listed a directory of `from` before
/// any mark adds to it.
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn diff_states_into(
    operation: Arc<InstanceOperationImpl>,
    from: NodeMapping,
    current: NodeMapping,
    to: NodeMapping,
    path: RelativePath,
    filter_mode: FilterMode,
    intent: FilesystemDiffIntent,
    discards: Option<Arc<WalkDiscards>>,
    changes: &ChangeSender,
) -> Result<FilesystemDiffStats, StateError> {
    let from_side = node_change_state(&from.repository, &from.state, from.node, path.clone()).await;
    let to_side = node_change_state(&to.repository, &to.state, to.node, path.clone()).await;
    let filesystem = to.clone();
    let intent = if intent.marks_dirty() {
        FilesystemDiffIntent::MarkDirty
    } else {
        FilesystemDiffIntent::Report
    };
    let extra = if intent.marks_dirty() {
        DiffFlags::FilesystemTo | DiffFlags::SettlesFrom
    } else {
        DiffFlags::FilesystemTo
    };
    // A path lore's tree tracks passes the ignore rules, as an ordinary clone's walk asks the
    // tree about an entry before the rules whatever mode its caller names (ADR-00027).
    let filter_mode = filter_mode | FilterMode::KeepTracked;
    let mut walk = ChangeStream::<DiffWalkStats>::spawn(async move |found| {
        if is_removed_directory(&from_side, &to_side, &path) {
            return removed_directory_changes(from_side, to_side, path, &found, filter_mode).await;
        }
        diff_subtree_flagged(from_side, to_side, path, None, extra, &found, filter_mode).await
    });
    let mut settling = Settling {
        operation,
        from,
        current,
        to: filesystem,
        intent,
        stats: FilesystemDiffStats::default(),
        dropped: Vec::new(),
        unmarked: Vec::new(),
        discards: Vec::new(),
        queue: discards,
        settled: Vec::new(),
    };
    while let Some(change) = walk.next().await {
        let settled = match settling.settle(change).await {
            Ok(settled) => settled,
            Err(err) => {
                walk.abandon().await;
                return Err(err);
            }
        };
        if let Some(change) = settled {
            emit(changes, || change).await?;
        }
    }
    let walked = walk.finish().await?;
    let stats = settling.finish().await?;
    stats
        .moved_under_walk
        .fetch_add(walked.moved_under_walk.into_inner(), Ordering::Relaxed);
    stats
        .finds_after_change
        .fetch_add(walked.finds_after_change.into_inner(), Ordering::Relaxed);
    Ok(stats)
}

/// Whether the root of a provider diff at `path` is a directory `from` holds where the filesystem
/// State `to` holds nothing. The walk compares the children of its root, so it would report only
/// what the directory held.
#[cfg(target_os = "linux")]
fn is_removed_directory(from: &NodeChangeState, to: &NodeChangeState, path: &RelativePath) -> bool {
    !path.is_empty()
        && from.mapping.node.is_valid_node_id()
        && !to.mapping.node.is_valid_or_root_node_id()
        && !from.flags.intersects(NodeFlags::File | NodeFlags::Link)
}

/// The changes of a provider diff whose root is a directory the filesystem State no longer holds
/// ([`is_removed_directory`]): the directory deleted, then everything below it, as a walk over a
/// working tree reports a missing path, through the hierarchy a delete of a whole directory walks.
/// The root is put to the filter first, past the ignore rules only where lore's tree tracks it,
/// so an add never committed that the rules now cover is left out, as the walk leaves it.
#[cfg(target_os = "linux")]
async fn removed_directory_changes(
    from: NodeChangeState,
    to: NodeChangeState,
    path: RelativePath,
    changes: &ChangeSender,
    filter_mode: FilterMode,
) -> Result<DiffWalkStats, StateError> {
    let tracked = from_tracks(&from, true).await?;
    let filter = &from.mapping.repository.filter;
    let (states, excluded) = seed_states(filter, &path, true, true, filter_mode.at(tracked));
    if excluded {
        return Ok(DiffWalkStats::default());
    }
    let to = to.invalid(path);
    // Everything reported is a node lore's tree holds, which is past the ignore rules.
    add_change(
        from,
        to,
        FileAction::Delete,
        change::Flags::None,
        changes,
        filter_mode & FilterMode::View,
        states,
    )
    .await?;
    Ok(DiffWalkStats::default())
}

/// What a provider diff has settled so far, and what it settles once the walk has ended.
#[cfg(target_os = "linux")]
struct Settling {
    operation: Arc<InstanceOperationImpl>,
    from: NodeMapping,
    current: NodeMapping,
    /// The filesystem State, which a delete is confirmed against.
    to: NodeMapping,
    intent: FilesystemDiffIntent,
    stats: FilesystemDiffStats,
    /// Directories whose descendants' changes are dropped: discarded whole, staged for delete
    /// already, or held by the filesystem after all.
    dropped: Vec<RelativePath>,
    /// Directories a type change replaced, reported below without marks, as a marking walk
    /// leaves a type change to staging.
    unmarked: Vec<RelativePath>,
    /// Adds the tree no longer holds, discarded, or held on `queue`, once the walk has ended.
    discards: Vec<NodeID>,
    /// The caller's queue `discards` is held on, where given.
    queue: Option<Arc<WalkDiscards>>,
    /// Marked nodes the tree holds unchanged, cleared once every mark is made.
    settled: Vec<NodeID>,
}

/// Whether `path` lies below one of `directories`.
#[cfg(target_os = "linux")]
fn below_any(directories: &[RelativePath], path: &RelativePath) -> bool {
    directories
        .iter()
        .any(|directory| path.below(directory).is_some_and(|rest| !rest.is_empty()))
}

#[cfg(target_os = "linux")]
impl Settling {
    /// Settles one change the walk found, answering with the change to report, if any.
    async fn settle(&mut self, change: NodeChange) -> Result<Option<NodeChange>, StateError> {
        let path = change.path().clone();
        if below_any(&self.dropped, &path) {
            return Ok(None);
        }
        let marks = self.intent.marks_dirty() && !below_any(&self.unmarked, &path);
        let from_valid = change.from.mapping.node.is_valid_node_id();
        let to_valid = change.to.mapping.node.is_valid_node_id();
        match change.action {
            FileAction::Delete | FileAction::Add if from_valid && to_valid => {
                if marks {
                    self.mark_displaced(change.from.mapping.node).await?;
                }
                self.unmarked.push(path);
                Ok(Some(change))
            }
            FileAction::Delete => self.settle_delete(change, path, marks).await,
            FileAction::Add => self.settle_add(change, path, marks).await,
            FileAction::Keep | FileAction::Move if change.to.flags.contains(NodeFlags::File) => {
                self.settle_file(change, path, marks).await
            }
            FileAction::Keep | FileAction::Move => self.settle_directory(change, path, marks).await,
            _ => Ok(Some(change)),
        }
    }

    /// Marks deleted the node a type change displaced, so a stage of a directory above, which
    /// stages only what is marked, reaches its path and replaces it there as a stage of the path
    /// itself does. One staged for delete already, or added since the revision, is reached as it
    /// is.
    async fn mark_displaced(&self, node_id: NodeID) -> Result<(), StateError> {
        let node = self.node(node_id).await?;
        if node.is_staged_delete() || node.is_dirty_add() {
            return Ok(());
        }
        self.mark(node_id, SettledAction::Delete).await
    }

    /// A directory both hold: a delete staged for it is taken back, one `from` holds as an add
    /// not yet committed stays an add, and one reported only for the mark `from` carries is
    /// dropped.
    async fn settle_directory(
        &mut self,
        mut change: NodeChange,
        path: RelativePath,
        marks: bool,
    ) -> Result<Option<NodeChange>, StateError> {
        let node_id = change.from.mapping.node;
        if marks && self.node(node_id).await?.is_staged_delete() {
            self.from
                .state
                .node_clear(&self.from.repository, node_id, Clear::Staged)
                .await?;
        }
        if marks && self.uncommitted(&path).await {
            if !self.node(node_id).await?.is_dirty_add() {
                self.mark(node_id, SettledAction::Add).await?;
            }
            change.action = FileAction::Add;
            change.from = change.from.invalid(path.clone());
            change.to = self.node_side(node_id, path).await?;
            change.flags = compute_change_flags(&self.node(node_id).await?);
            return Ok(Some(change));
        }
        let measured = change.flags.contains(change::Flags::Modify) || change.flags.is_conflict();
        if change.action == FileAction::Keep && !measured {
            return Ok(None);
        }
        Ok(Some(change))
    }

    /// A node the tree no longer holds, which the walk confirmed: an add not yet committed is
    /// discarded, a node staged for delete already is left as it is, and anything else is marked
    /// deleted.
    async fn settle_delete(
        &mut self,
        mut change: NodeChange,
        path: RelativePath,
        marks: bool,
    ) -> Result<Option<NodeChange>, StateError> {
        let node_id = change.from.mapping.node;
        let node = self.node(node_id).await?;
        if node.is_staged_delete() {
            if node.is_directory() {
                self.dropped.push(path);
            }
            return Ok(None);
        }
        if marks && self.uncommitted(&path).await {
            self.discards.push(node_id);
            if node.is_directory() {
                self.dropped.push(path);
            }
            return Ok(None);
        }
        if node.is_file() {
            self.stats.file_delete.fetch_add(1, Ordering::Relaxed);
        }
        if marks {
            self.mark(node_id, SettledAction::Delete).await?;
            change.flags = self.flags_of(node_id).await? | (change.flags & change::Flags::Modify);
        }
        Ok(Some(change))
    }

    /// A node only the tree holds, added to `from` where the intent marks. The walk's listing of
    /// the parent lacked the name, and nothing else writes `from`, so the node is added without a
    /// name check; below the directories a diff rooted inside one `from` lacks adds on its way,
    /// which the walk did not list, it is attached, and a name `from` holds already is marked on
    /// the node holding it.
    async fn settle_add(
        &mut self,
        mut change: NodeChange,
        path: RelativePath,
        marks: bool,
    ) -> Result<Option<NodeChange>, StateError> {
        let is_file = change.to.flags.contains(NodeFlags::File);
        if is_file {
            self.stats.file_add.fetch_add(1, Ordering::Relaxed);
        }
        if !marks {
            let observed = self
                .observed(&path, change.to.mapping.node, is_file)
                .await?;
            change.flags = change::Flags::None;
            change.to = self.filesystem_side(path, observed, is_file);
            return Ok(Some(change));
        }
        let parent_path = path.parent_path();
        let found = self.from.node_at(&parent_path).await;
        let listed = found.is_valid_or_root();
        let parent = if listed {
            found.node
        } else {
            ensure_scan_dir_chain(
                self.from.repository.clone(),
                self.from.state.clone(),
                ROOT_NODE,
                parent_path.as_str(),
            )
            .await?
        };
        let mut flags = NodeFlags::DirtyAdd;
        if is_file {
            flags |= NodeFlags::File;
        }
        let node = Node {
            flags: flags.bits(),
            name_hash: crate::hash::hash_string(path.name()),
            ..Default::default()
        };
        let node_id = if listed {
            self.from
                .state
                .node_add(self.from.repository.clone(), parent, node, path.name())
                .await?
        } else {
            match self
                .from
                .state
                .node_attach(self.from.repository.clone(), parent, node, path.name())
                .await?
            {
                AttachOutcome::Attached(node_id) => node_id,
                AttachOutcome::Exists(held) => {
                    self.undelete_or_mark(held).await?;
                    held
                }
            }
        };
        self.from
            .state
            .node_mark(
                &self.from.repository,
                parent,
                Mark::dirty(NodeFlags::Dirty, Ext::Keep, false),
            )
            .await?;
        change.to = self.node_side(node_id, path).await?;
        change.flags = compute_change_flags(&self.node(node_id).await?);
        Ok(Some(change))
    }

    /// Marks a node `from` held where the diff found only the tree holding the name: a delete
    /// staged for it is taken back, and it is marked modified unless it is an add.
    async fn undelete_or_mark(&self, node_id: NodeID) -> Result<(), StateError> {
        let node = self.node(node_id).await?;
        if node.is_staged_delete() {
            self.from
                .state
                .node_clear(&self.from.repository, node_id, Clear::Staged)
                .await?;
        }
        if !node.is_dirty_add() {
            self.mark(node_id, SettledAction::Modify).await?;
        }
        Ok(())
    }

    /// A file both hold: reported where its content or mode differs, and a mark the tree holds
    /// no change for is cleared. A file `from` holds as an add not yet committed stays an add.
    async fn settle_file(
        &mut self,
        mut change: NodeChange,
        path: RelativePath,
        marks: bool,
    ) -> Result<Option<NodeChange>, StateError> {
        let node_id = change.from.mapping.node;
        let from_node = self.node(node_id).await?;
        if marks && from_node.is_staged_delete() {
            self.from
                .state
                .node_clear(&self.from.repository, node_id, Clear::Staged)
                .await?;
        }
        if marks && self.uncommitted(&path).await {
            if !from_node.is_dirty_add() {
                self.mark(node_id, SettledAction::Add).await?;
            }
            self.stats.file_add.fetch_add(1, Ordering::Relaxed);
            change.action = FileAction::Add;
            change.from = change.from.invalid(path.clone());
            change.to = self.node_side(node_id, path).await?;
            change.flags = compute_change_flags(&self.node(node_id).await?);
            return Ok(Some(change));
        }
        let observed = self.observed(&path, change.to.mapping.node, true).await?;
        if !observed.is_file() {
            if self.held(&path).await {
                return Ok(None);
            }
            change.action = FileAction::Delete;
            change.to = change.to.invalid(path.clone());
            return self.settle_delete(change, path, marks).await;
        }
        let renamed = change.action == FileAction::Move;
        if !renamed && !self.differs(&change, &from_node, &path, &observed).await? {
            self.stats.file_retain.fetch_add(1, Ordering::Relaxed);
            if marks && from_node.is_dirty() {
                self.settled.push(node_id);
            }
            return Ok(None);
        }
        self.stats.file_replace.fetch_add(1, Ordering::Relaxed);
        if marks {
            let action = if renamed {
                SettledAction::Move
            } else {
                SettledAction::Modify
            };
            self.mark(node_id, action).await?;
        }
        change.flags = self.flags_of(node_id).await? | change::Flags::Modify;
        change.to = self.filesystem_side(path, observed, true);
        Ok(Some(change))
    }

    /// Whether the tree's file differs from `from_node`: by address where the tree knows its
    /// content's, else by size, then content, measured through the operation. A from node with
    /// no stored address holds no content, so it equals only an empty file.
    async fn differs(
        &self,
        change: &NodeChange,
        from_node: &Node,
        path: &RelativePath,
        observed: &FileInfo,
    ) -> Result<bool, StateError> {
        if observed.mode_differs_from(from_node.mode) {
            return Ok(true);
        }
        let to_address = change.to.address;
        if !to_address.hash.is_zero() {
            return Ok(to_address.hash != from_node.address.hash);
        }
        if observed.size() != from_node.size {
            return Ok(true);
        }
        if from_node.address.hash.is_zero() {
            return Ok(observed.size() != 0);
        }
        self.stats.file_hash.fetch_add(1, Ordering::Relaxed);
        let comparison = file_matches_node(
            self.from.repository.clone(),
            from_node,
            observed.size(),
            path,
            &self.operation,
            &lore_storage::ContentHashes::default(),
        )
        .await?;
        Ok(!matches!(comparison, NodeComparison::Matches))
    }

    /// Whether a find by name on the filesystem State finds a node at `path`. An error other than
    /// the node not being found counts as held, so a delete is never reported on a failed find.
    async fn held(&self, path: &RelativePath) -> bool {
        let Some(below) = path.below(&self.to.path) else {
            return false;
        };
        match self
            .to
            .state
            .find_relative_node_link(self.to.repository.clone(), self.to.node, below)
            .await
        {
            Ok(link) => link.is_valid(),
            Err(crate::state::StateErrors::NodeNotFound(_)) => false,
            Err(_) => true,
        }
    }

    /// Whether the working copy's last revision holds nothing at `path`, which makes a node
    /// `from` holds there an add not yet committed.
    async fn uncommitted(&self, path: &RelativePath) -> bool {
        !Arc::ptr_eq(&self.from.state, &self.current.state)
            && !self.current.node_at(path).await.is_valid()
    }

    fn mark(
        &self,
        node_id: NodeID,
        action: SettledAction,
    ) -> impl Future<Output = Result<(), StateError>> + Send + '_ {
        mark_settled(
            &self.from.state,
            &self.from.repository,
            node_id,
            action,
            self.intent,
        )
    }

    async fn node(&self, node_id: NodeID) -> Result<Node, StateError> {
        self.from
            .state
            .node(self.from.repository.clone(), node_id)
            .await
    }

    async fn flags_of(&self, node_id: NodeID) -> Result<change::Flags, StateError> {
        Ok(compute_change_flags(&self.node(node_id).await?))
    }

    /// What the tree holds at `path`, where `node` of the filesystem State stands: a file is
    /// measured from the record the walk read of it, never by resolving the path.
    async fn observed(
        &self,
        path: &RelativePath,
        node: NodeID,
        is_file: bool,
    ) -> Result<FileInfo, StateError> {
        if !is_file {
            return Ok(FileInfo::Directory);
        }
        let record = self.to.state.node(self.to.repository.clone(), node).await?;
        self.operation
            .measure(path, &record)
            .await
            .forward_any::<StateError>("reading a file the provider diff reports")
    }

    /// The side of a change standing for the tree at `path`, as a walk over a working tree
    /// reports it.
    fn filesystem_side(
        &self,
        path: RelativePath,
        observed: FileInfo,
        is_file: bool,
    ) -> NodeChangeState {
        NodeChangeState {
            mapping: NodeMapping {
                repository: self.from.repository.clone(),
                state: self.from.state.clone(),
                path,
                node: INVALID_NODE,
            },
            observed: Some(observed),
            flags: if is_file {
                NodeFlags::File
            } else {
                NodeFlags::NoFlags
            },
            address: Address::default(),
            mode: 0,
        }
    }

    /// The side of a change standing for `node_id` of `from`, at `path`.
    async fn node_side(
        &self,
        node_id: NodeID,
        path: RelativePath,
    ) -> Result<NodeChangeState, StateError> {
        let node = self.node(node_id).await?;
        Ok(NodeChangeState {
            mapping: NodeMapping {
                repository: self.from.repository.clone(),
                state: self.from.state.clone(),
                path,
                node: node_id,
            },
            observed: None,
            flags: NodeFlags::from_bits_retain(node.flags),
            address: node.address,
            mode: node.mode,
        })
    }

    /// Clears the marks of the nodes found unchanged, then discards the adds the tree no longer
    /// holds, or holds them on `queue`, and answers with what the diff counted, the discards among
    /// it.
    async fn finish(self) -> Result<FilesystemDiffStats, StateError> {
        let repository = self.from.repository.clone();
        for node_id in self.settled {
            self.from
                .state
                .node_clear(&repository, node_id, Clear::Dirty)
                .await?;
        }
        self.stats
            .discarded
            .fetch_add(self.discards.len() as u64, Ordering::Relaxed);
        match self.queue {
            Some(queue) => queue.queue(self.from.state, repository, self.discards),
            None => apply_pending_discards(self.from.state, repository, self.discards).await?,
        }
        Ok(self.stats)
    }
}
