// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Discards of nodes and subtrees, freeing their slots.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crossbeam_queue::SegQueue;
use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use smallvec::SmallVec;
use tokio::task::JoinSet;

use crate::node::BLOCK_NODE_COUNT;
use crate::node::Node;
use crate::node::NodeBlock;
use crate::node::NodeBlockData;
use crate::node::NodeBlockWriter;
use crate::node::NodeFlags;
use crate::node::NodeID;
use crate::node::NodeIDExt;
use crate::node::SiblingCycleGuard;
use crate::repository::RepositoryContext;
use crate::state::State;
use crate::state::StateError;
use crate::state::clear_propagation;
#[cfg(feature = "state-hooks")]
use crate::state::node::hooks::log;
use crate::subtask;

/// Read-only walk of `parent_node_id`'s `child → sibling → …` chain formatted for diagnostic
/// error messages. Called only from the error paths of a discard's detach and of a chain change
/// whose attempts ran out ([`ChainAttempts::exhausted`]), so it never costs on the hot path.
///
/// [`ChainAttempts::exhausted`]: super::locks::ChainAttempts::exhausted
pub(in crate::state) async fn format_parent_child_chain(
    state: &State,
    repository: &Arc<RepositoryContext>,
    parent_node_id: NodeID,
) -> String {
    const MAX_CHAIN: usize = 64;

    let Ok(parent_block) = state
        .block(repository.clone(), NodeBlock::index(parent_node_id))
        .await
    else {
        return "<parent block unreadable>".to_string();
    };
    let initial_child = parent_block.node(Node::index(parent_node_id)).child;

    let mut buffer = String::new();
    if !initial_child.is_valid_node_id() {
        buffer.push_str("<empty>");
        return buffer;
    }

    let mut current_node_id = initial_child;
    let mut steps = 0;
    while current_node_id.is_valid_node_id() {
        if steps == MAX_CHAIN {
            buffer.push_str(" -> …(truncated)");
            return buffer;
        }
        if steps > 0 {
            buffer.push_str(" -> ");
        }
        buffer.push_str(&current_node_id.to_string());
        steps += 1;

        let Ok(sibling_block) = state
            .block(repository.clone(), NodeBlock::index(current_node_id))
            .await
        else {
            buffer.push_str(" -> <unreadable>");
            return buffer;
        };
        current_node_id = sibling_block.node(Node::index(current_node_id)).sibling;
    }
    buffer
}

/// Counts of the files and directories a discard freed, links counted as directories.
#[derive(Debug, Default, Clone, Copy)]
pub struct DiscardCounts {
    pub file_count: u64,
    pub directory_count: u64,
}

impl DiscardCounts {
    #[inline]
    fn count(&mut self, flags: u16) {
        if flags & NodeFlags::File.bits() != 0 {
            self.file_count += 1;
        } else {
            self.directory_count += 1;
        }
    }

    pub(in crate::state) fn add(&mut self, other: Self) {
        self.file_count += other.file_count;
        self.directory_count += other.directory_count;
    }
}

/// What a discard's detach leaves of the node it detaches.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Remains {
    /// Marked `Discarded`, its child and type kept for the frees below it.
    Marked,
    /// Freed, as a node with nothing below it to free is.
    Freed,
}

/// Detaches `node_id`, read as `node` from `block`, from its parent's chain, then marks it
/// `Discarded` or frees it as `then` says, counting a node freed. Under an exclusive claim
/// nothing runs beside it, so the walk to the node's predecessor takes one block lock at a time,
/// and where the record the detach writes lies in the node's block both writes take one lock. A
/// node its parent's chain does not reach is a broken hierarchy. `parent_held` keeps the block of
/// the last parent looked up, for callers detaching several nodes, which mostly share one.
async fn detach_exclusive(
    state: &State,
    repository: &Arc<RepositoryContext>,
    (node_id, node, block): (NodeID, &Node, &Arc<NodeBlock>),
    then: Remains,
    counts: &mut DiscardCounts,
    parent_held: &mut Option<(usize, Arc<NodeBlock>)>,
) -> Result<(), StateError> {
    let parent_block_index = NodeBlock::index(node.parent);
    let parent_index = Node::index(node.parent);
    // Every block here is borrowed: the node's, the caller's cache, or `loaded`, a step's.
    let parent_block: &Arc<NodeBlock> = if parent_block_index == NodeBlock::index(node_id) {
        block
    } else {
        match parent_held {
            Some((index, held)) if *index == parent_block_index => held,
            _ => {
                let loaded = known_block(state, repository, parent_block_index).await?;
                &parent_held.insert((parent_block_index, loaded)).1
            }
        }
    };
    let first = parent_block.node(parent_index).child;
    let mut loaded: Option<Arc<NodeBlock>> = None;
    // The record pointing at the node: the parent, through its `child`, or a predecessor, through
    // its `sibling`.
    let (predecessor_block, predecessor_block_index, predecessor_index, head) = if first == node_id
    {
        (parent_block, parent_block_index, parent_index, true)
    } else {
        let mut previous_id = first;
        let mut cycle = SiblingCycleGuard::new(node.parent);
        // The block of the last step, as siblings mostly share one.
        let mut held = (parent_block_index, parent_block);
        loop {
            if !previous_id.is_valid_node_id() {
                // Boxed: a cold path, which would otherwise size every discard's future.
                let chain =
                    Box::pin(format_parent_child_chain(state, repository, node.parent)).await;
                return Err(StateError::internal(format!(
                    "Discard hierarchy broken: node {node_id} (parent={parent_node_id}, \
                         sibling_in_node={node_sibling}, flags={node_flags:#x}) not in \
                         parent.child chain (observed: {chain})",
                    parent_node_id = node.parent,
                    node_sibling = node.sibling,
                    node_flags = node.flags,
                )));
            }
            cycle.observe(previous_id).map_err(StateError::from)?;
            let previous_block_index = NodeBlock::index(previous_id);
            if held.0 != previous_block_index {
                held = if previous_block_index == NodeBlock::index(node_id) {
                    (previous_block_index, block)
                } else {
                    let step = known_block(state, repository, previous_block_index).await?;
                    (previous_block_index, &*loaded.insert(step))
                };
            }
            let sibling = held.1.node(Node::index(previous_id)).sibling;
            if sibling == node_id {
                break (
                    held.1,
                    previous_block_index,
                    Node::index(previous_id),
                    false,
                );
            }
            previous_id = sibling;
        }
    };
    let block_index = NodeBlock::index(node_id);
    let node_index = Node::index(node_id);
    // Under `state-hooks`, the detach's record, made in the section that holds the node, before
    // the node is marked or freed.
    #[cfg(feature = "state-hooks")]
    let _label = log::label(log::Section::DiscardMarks);
    let finish = |writer: &mut NodeBlockWriter<'_>, counts: &mut DiscardCounts| {
        #[cfg(feature = "state-hooks")]
        log::record(
            &state.hooks,
            log::Effect::Detach {
                node: node_id,
                parent: node.parent,
                how: log::How::Discard,
                expected: log::Expected::Nothing,
                instance: parent_block.hook_instance(),
            },
        );
        match then {
            // Detached, and freed below unless the discard fails first, which leaves it to the
            // next sweep (C15).
            Remains::Marked => {
                writer.node(node_index).flags |= NodeFlags::Discarded.bits();
                writer.mark_unswept();
            }
            Remains::Freed => {
                counts.count(writer.node(node_index).flags);
                writer.discard_node(block_index, node_index);
            }
        }
    };
    let dirtied = {
        let mut writer = predecessor_block.write();
        let predecessor = writer.node(predecessor_index);
        if head {
            predecessor.child = node.sibling;
        } else {
            predecessor.sibling = node.sibling;
        }
        if predecessor_block_index == block_index {
            finish(&mut writer, counts);
        }
        writer.mark_dirty()
    };
    if dirtied {
        modified(state, predecessor_block, predecessor_block_index);
    }
    if predecessor_block_index != block_index {
        let dirtied = {
            let mut writer = block.write();
            finish(&mut writer, counts);
            writer.mark_dirty()
        };
        if dirtied {
            modified(state, block, block_index);
        }
    }
    Ok(())
}

/// The error of a discard's walk meeting `at`, below `directory`, `Discarded` already: built out
/// of line, as the walk's per-node loop is hot.
#[cold]
#[inline(never)]
fn met_discarded(at: NodeID, directory: NodeID) -> StateError {
    StateError::internal(format!(
        "Discard met node {at} below {directory}, freed or taken already"
    ))
}

/// Block `block_index`, where it is in memory without a lookup of the store's, else loaded.
#[inline]
async fn known_block(
    state: &State,
    repository: &Arc<RepositoryContext>,
    block_index: usize,
) -> Result<Arc<NodeBlock>, StateError> {
    match state.resident_block(block_index) {
        Some(block) => Ok(block),
        None => state.block(repository.clone(), block_index).await,
    }
}

/// Registers `block`, which just turned dirty, and marks the State dirty with it.
#[inline]
fn modified(state: &State, block: &Arc<NodeBlock>, block_index: usize) {
    state.block_modified(block.clone(), block_index);
}

/// Frees `node_id`, held in `block`, counting it.
#[inline]
fn free_node(
    state: &State,
    block: &Arc<NodeBlock>,
    block_index: usize,
    node_id: NodeID,
    counts: &mut DiscardCounts,
) {
    let dirtied = {
        #[cfg(feature = "state-hooks")]
        let _label = log::label(log::Section::DiscardFree);
        let mut writer = block.write();
        counts.count(writer.node(Node::index(node_id)).flags);
        writer.discard_node(block_index, Node::index(node_id));
        writer.mark_dirty()
    };
    if dirtied {
        modified(state, block, block_index);
    }
}

/// Discards the subtree at `root` under an exclusive claim, with no parent marked: detaches
/// `root` first, marking it `Discarded` (H6), then frees its subtree, each directory after its
/// own children, a task per directory, and `root` last. An error part way leaves a detached
/// subtree partly freed, never a live node over a freed slot. A root already `Discarded` is
/// skipped. Links are leaves: their child belongs to the linked State.
pub async fn node_discard_fused(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    root: NodeID,
) -> Result<DiscardCounts, StateError> {
    state.assert_unserved("node_discard_fused");
    let block_index = NodeBlock::index(root);
    let block = known_block(&state, &repository, block_index).await?;
    let node = block.node(Node::index(root));
    let mut counts = DiscardCounts::default();
    if node.is_discarded() {
        return Ok(counts);
    }
    if !node.is_directory() {
        detach_exclusive(
            &state,
            &repository,
            (root, &node, &block),
            Remains::Freed,
            &mut counts,
            &mut None,
        )
        .await?;
        return Ok(counts);
    }
    detach_exclusive(
        &state,
        &repository,
        (root, &node, &block),
        Remains::Marked,
        &mut counts,
        &mut None,
    )
    .await?;
    #[cfg(feature = "state-hooks")]
    crate::state::node::hooks::reached(crate::state::node::hooks::Point::Discard).await;
    free_below(state, repository, root, (block, block_index), node.child()).await
}

/// Makes `start`, the first node of `directory`'s chain a failed discard left unfreed, the
/// directory's head, `directory` held in `own`: the files before it in the chain are freed and its
/// directories marked, and a freed slot's `sibling` continues the free list, so without this the
/// next sweep would reach none of the rest from `directory`. The block was flagged for that sweep
/// when `directory` was marked.
fn take_unfreed(
    state: &State,
    own: &Arc<NodeBlock>,
    own_index: usize,
    directory: NodeID,
    start: NodeID,
) {
    let dirtied = {
        #[cfg(feature = "state-hooks")]
        let _label = log::label(log::Section::DiscardMarks);
        let mut writer = own.write();
        writer.node(Node::index(directory)).child = start;
        writer.mark_dirty()
    };
    if dirtied {
        modified(state, own, own_index);
    }
}

/// Frees every node below `directory`, whose chain starts at `first`, each directory after its
/// own children, with a task per directory, then `directory` itself, held in `own`, its block and
/// the block's index. Each run of the chain lying in one block is freed under one write lock,
/// a child's sibling read before it is freed, as freeing repurposes it for the block's free list;
/// the run's directories are marked `Discarded` under that lock and spawned once it is released,
/// as their tasks start on that block. A chain that breaks, meets a `Discarded` node or reaches a
/// block that fails to load ends this directory's walk; that, or a task that fails, makes it
/// answer the first error once every task is joined, which leaves `directory` itself unfreed for
/// the next sweep (C15). A block that fails to load leaves the chain from its node on unfreed, and
/// `directory` takes that node as its head, as the slots freed before it continue the free list now.
fn free_below(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    directory: NodeID,
    own: (Arc<NodeBlock>, usize),
    first: Option<NodeID>,
) -> Pin<Box<dyn Future<Output = Result<DiscardCounts, StateError>> + Send>> {
    Box::pin(async move {
        let mut counts = DiscardCounts::default();
        // Made at the first directory child: a set allocates, and most directories hold none.
        let mut tasks: Option<JoinSet<Result<DiscardCounts, StateError>>> = None;
        let mut next = first;
        let mut cycle = SiblingCycleGuard::new(directory);
        // Boxed, as it lives across both awaits of a future boxed per directory, and is rare.
        let mut broken: Option<Box<StateError>> = None;
        while let Some(start) = next {
            let block_index = NodeBlock::index(start);
            // Not through `known_block`, whose own future would sit in this one.
            let block = match state.resident_block(block_index) {
                Some(block) => block,
                None => match state.block(repository.clone(), block_index).await {
                    Ok(block) => block,
                    Err(error) => {
                        take_unfreed(&state, &own.0, own.1, directory, start);
                        broken = Some(Box::new(error));
                        break;
                    }
                },
            };
            // The run's directories and their chain heads, spawned once its lock is released;
            // made here, as no await falls between, so the future holds none.
            let mut directories: SmallVec<[(NodeID, Option<NodeID>); 8]> = SmallVec::new();
            let dirtied = {
                // Its directories marked, its files freed: the marks are the run's writes.
                #[cfg(feature = "state-hooks")]
                let _label = log::label(log::Section::DiscardMarks);
                let mut writer = block.write();
                let mut at = start;
                let mut changed = false;
                // A directory marked here is in no chain once a free before it repurposes its
                // predecessor's sibling, so a task failing below it leaves it to the next sweep.
                let mut marked = false;
                loop {
                    let child = writer.node(Node::index(at));
                    if let Err(error) = child.walk_step(at, directory, &mut cycle) {
                        broken = Some(Box::new(error));
                        next = None;
                        break;
                    }
                    // A live chain below a detached root holds no `Discarded` node: one met is a
                    // slot freed or a directory taken already, by this discard, where a looping
                    // chain meets it again before the cycle guard trips, or earlier, through a
                    // corrupt chain.
                    if child.is_discarded() {
                        broken = Some(Box::new(met_discarded(at, directory)));
                        next = None;
                        break;
                    }
                    next = child.sibling();
                    if child.is_directory() {
                        child.flags |= NodeFlags::Discarded.bits();
                        marked = true;
                        directories.push((at, child.child()));
                        #[cfg(feature = "state-hooks")]
                        log::record(&state.hooks, log::Effect::Discarding { node: at });
                    } else {
                        counts.count(child.flags);
                        writer.discard_node(block_index, Node::index(at));
                    }
                    changed = true;
                    match next {
                        Some(sibling) if NodeBlock::index(sibling) == block_index => at = sibling,
                        _ => break,
                    }
                }
                if marked {
                    writer.mark_unswept();
                }
                changed && writer.mark_dirty()
            };
            if dirtied {
                modified(&state, &block, block_index);
            }
            for (at, first) in directories {
                let set = tasks.get_or_insert_with(JoinSet::new);
                lore_spawn!(
                    set,
                    subtask!(free_below(
                        state.clone(),
                        repository.clone(),
                        at,
                        (block.clone(), block_index),
                        first,
                    ))
                );
            }
        }
        // Every task joined whatever failed, so none frees after the discard has answered.
        if let Some(mut tasks) = tasks {
            while let Some(joined) = tasks.join_next().await {
                match joined.internal("Discard node task") {
                    Ok(Ok(found)) => counts.add(found),
                    Ok(Err(error)) => {
                        broken.get_or_insert_with(|| Box::new(error));
                    }
                    Err(error) => {
                        broken.get_or_insert_with(|| Box::new(error.into()));
                    }
                }
            }
        }
        if let Some(error) = broken {
            return Err(*error);
        }
        free_node(&state, &own.0, own.1, directory, &mut counts);
        Ok(counts)
    })
}

/// Walks the subtree below `root`, reading only, with a task per directory so blocks not in
/// memory load side by side; `handler` gets each node below `root` with its flags. Links are
/// leaves, and a root that is not a directory has nothing below it. The collecting half of a
/// discard whose freeing half is [`node_discard_free`].
pub async fn node_discard_walk<F>(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    root: NodeID,
    handler: F,
) -> Result<(), StateError>
where
    F: Fn(NodeID, u16) + Clone + Send + 'static,
{
    let node = known_block(&state, &repository, NodeBlock::index(root))
        .await?
        .node(Node::index(root));
    if !node.is_directory() {
        return Ok(());
    }
    walk_below(state, repository, root, node.child(), handler).await
}

fn walk_below<F>(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    directory: NodeID,
    first: Option<NodeID>,
    handler: F,
) -> Pin<Box<dyn Future<Output = Result<(), StateError>> + Send>>
where
    F: Fn(NodeID, u16) + Clone + Send + 'static,
{
    Box::pin(async move {
        // Made at the first directory child: a set allocates, and most directories hold none.
        let mut tasks: Option<JoinSet<Result<(), StateError>>> = None;
        let mut next = first;
        let mut cycle = SiblingCycleGuard::new(directory);
        // The block of the last child read, as siblings mostly share one.
        let mut held: Option<(usize, Arc<NodeBlock>)> = None;
        while let Some(child_id) = next {
            let block_index = NodeBlock::index(child_id);
            let block = match &held {
                Some((index, block)) if *index == block_index => block,
                _ => {
                    // Not through `known_block`, whose own future would sit in this boxed one.
                    let block = match state.resident_block(block_index) {
                        Some(block) => block,
                        None => state.block(repository.clone(), block_index).await?,
                    };
                    &held.insert((block_index, block)).1
                }
            };
            let child = block.node(Node::index(child_id));
            child.walk_step(child_id, directory, &mut cycle)?;
            next = child.sibling();
            handler(child_id, child.flags);
            if child.is_directory() {
                let set = tasks.get_or_insert_with(JoinSet::new);
                lore_spawn!(
                    set,
                    subtask!(walk_below(
                        state.clone(),
                        repository.clone(),
                        child_id,
                        child.child(),
                        handler.clone(),
                    ))
                );
            }
        }
        if let Some(mut tasks) = tasks {
            while let Some(joined) = tasks.join_next().await {
                joined.internal("Discard walk task")??;
            }
        }
        Ok(())
    })
}

/// Frees, under an exclusive claim, `roots` and the nodes below them a walk collected
/// ([`node_discard_walk`]), with no parent marked. Each root is detached first and `on_root` told
/// its flags: a directory marked `Discarded` (H6), the rest freed, having nothing below them. Then
/// every block a collected node or a directory root lies in is loaded, so a load error frees
/// nothing below a root, and those nodes are freed a block at a time, under one write lock each,
/// skipping a slot already free (`discard_node` has no guard against a second free). A root
/// already `Discarded` is skipped. The counts cover every node freed.
pub async fn node_discard_free<F>(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    roots: &[NodeID],
    mut collected: Vec<NodeID>,
    mut on_root: F,
) -> Result<DiscardCounts, StateError>
where
    F: FnMut(NodeID, u16),
{
    state.assert_unserved("node_discard_free");
    let mut counts = DiscardCounts::default();
    // The blocks of the last root and of its parent, as consecutive roots mostly share them.
    let mut held: Option<(usize, Arc<NodeBlock>)> = None;
    let mut parent_held: Option<(usize, Arc<NodeBlock>)> = None;
    for &root in roots {
        let block_index = NodeBlock::index(root);
        let block = match &held {
            Some((index, block)) if *index == block_index => block,
            _ => {
                let block = known_block(state, repository, block_index).await?;
                &held.insert((block_index, block)).1
            }
        };
        let node = block.node(Node::index(root));
        if node.is_discarded() {
            continue;
        }
        let then = if node.is_directory() {
            collected.push(root);
            Remains::Marked
        } else {
            Remains::Freed
        };
        detach_exclusive(
            state,
            repository,
            (root, &node, block),
            then,
            &mut counts,
            &mut parent_held,
        )
        .await?;
        on_root(root, node.flags);
    }
    #[cfg(feature = "state-hooks")]
    crate::state::node::hooks::reached(crate::state::node::hooks::Point::Discard).await;
    counts.add(free_collected(state, repository, &mut collected).await?);
    Ok(counts)
}

/// Frees `collected`, sorting it: loads every block the nodes lie in first, so a load error frees
/// nothing, then frees them a block at a time ([`free_resolved`]). Under an exclusive claim, as
/// nothing may take a slot it frees meanwhile.
async fn free_collected(
    state: &State,
    repository: &Arc<RepositoryContext>,
    collected: &mut [NodeID],
) -> Result<DiscardCounts, StateError> {
    collected.sort_unstable();
    let mut blocks: Vec<(usize, Arc<NodeBlock>)> = Vec::new();
    for &node_id in collected.iter() {
        let block_index = NodeBlock::index(node_id);
        if blocks.last().is_none_or(|(last, _)| *last != block_index) {
            blocks.push((
                block_index,
                known_block(state, repository, block_index).await?,
            ));
        }
    }
    free_resolved(state, &blocks, collected)
}

/// Frees `collected`, sorted, a block at a time under one write lock each, skipping a slot already
/// free, every block a node lies in found in `blocks`, sorted by index; it loads nothing, so it
/// frees all or, where a block is missing, the groups before it.
pub(in crate::state) fn free_resolved(
    state: &State,
    blocks: &[(usize, Arc<NodeBlock>)],
    collected: &[NodeID],
) -> Result<DiscardCounts, StateError> {
    let mut counts = DiscardCounts::default();
    let mut group = collected;
    // Both sorted, so the blocks are met in order; `blocks` may hold more than the groups need.
    let mut cursor = 0;
    while let Some(&first) = group.first() {
        let block_index = NodeBlock::index(first);
        let length = group
            .iter()
            .position(|&node_id| NodeBlock::index(node_id) != block_index)
            .unwrap_or(group.len());
        let (ids, rest) = group.split_at(length);
        while blocks
            .get(cursor)
            .is_some_and(|(index, _)| *index < block_index)
        {
            cursor += 1;
        }
        let Some((_, block)) = blocks
            .get(cursor)
            .filter(|(index, _)| *index == block_index)
        else {
            return Err(StateError::internal(format!(
                "a free's block {block_index} was not resolved"
            )));
        };
        free_group(state, block, block_index, ids, &mut counts);
        group = rest;
    }
    Ok(counts)
}

/// Frees `ids`, all in `block`, under one write lock on it, counting each, and skipping a slot
/// already free: on the block's free list, or past its node count.
fn free_group(
    state: &State,
    block: &Arc<NodeBlock>,
    block_index: usize,
    ids: &[NodeID],
    counts: &mut DiscardCounts,
) {
    let dirtied = {
        #[cfg(feature = "state-hooks")]
        let _label = log::label(log::Section::DiscardFree);
        let mut writer = block.write();
        let mut free = free_list_slots(writer.data());
        for &node_id in ids {
            let index = Node::index(node_id);
            if free[index / 64] & (1 << (index % 64)) != 0
                || index >= writer.node_block().node_count as usize
            {
                continue;
            }
            counts.count(writer.node(index).flags);
            writer.discard_node(block_index, index);
            free[index / 64] |= 1 << (index % 64);
        }
        writer.mark_dirty()
    };
    if dirtied {
        modified(state, block, block_index);
    }
}

/// The words of a bitmap with a bit per slot of a block.
pub(in crate::state) const SLOT_WORDS: usize = BLOCK_NODE_COUNT / 64;

/// The slots on `data`'s free list, a bit each.
pub(in crate::state) fn free_list_slots(data: &NodeBlockData) -> [u64; SLOT_WORDS] {
    let mut free = [0u64; SLOT_WORDS];
    let mut at = data.node_unused as usize;
    for _ in 0..data.node_unused_count {
        if at >= BLOCK_NODE_COUNT {
            break;
        }
        free[at / 64] |= 1 << (at % 64);
        at = data.node[at].sibling as usize;
    }
    free
}

/// Discard the nodes a parallel filesystem walk collected — a reverted `DirtyAdd`, an entry
/// behind a nested-repository boundary, or an entry neither the file system nor any commit
/// holds — each with its subtree ([`node_discard_fused`]), and [clear the
/// propagation](clear_propagation) each leaves stale on its ancestor chain. Must only be called
/// after every walk over the tree has drained — discarding mid-walk mutates `parent.child` /
/// sibling chains under walks that are still reading them and races into a `"Discard hierarchy
/// broken"`, and clears marks other walks are still carrying up.
pub(crate) async fn apply_pending_discards(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    pending_discards: Vec<NodeID>,
) -> Result<(), StateError> {
    discard_pending(state, repository, pending_discards, false).await
}

/// [`apply_pending_discards`], where `emptied_adds` also discards each directory above a discarded
/// node that holds nothing once it leaves and is marked dirty as added and not staged
/// ([`discard_emptied_adds`]).
async fn discard_pending(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    mut pending_discards: Vec<NodeID>,
    emptied_adds: bool,
) -> Result<(), StateError> {
    if pending_discards.is_empty() {
        return Ok(());
    }
    pending_discards.sort_unstable();
    pending_discards.dedup();

    for discard_node_id in pending_discards {
        let Ok(discard_node) = state.node(repository.clone(), discard_node_id).await else {
            continue;
        };
        if discard_node.is_discarded() {
            continue;
        }
        node_discard_fused(state.clone(), repository.clone(), discard_node_id).await?;
        // The discard marks the State where a block turns dirty; a stage reads `is_dirty()` to
        // decide whether to save, so it is marked whatever the blocks were.
        state.mark_dirty();
        let stays = if emptied_adds {
            Box::pin(discard_emptied_adds(
                &state,
                &repository,
                discard_node.parent,
            ))
            .await?
        } else {
            discard_node.parent
        };
        clear_propagation(&state, &repository, stays).await?;
    }
    Ok(())
}

/// Discards `directory` and each directory above it, while each holds no node and is marked dirty
/// as added and not staged, and answers the first that stays. Boxed by its caller, as only a reset
/// takes it.
async fn discard_emptied_adds(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    mut directory: NodeID,
) -> Result<NodeID, StateError> {
    while directory.is_valid_node_id() {
        let node = state.node(repository.clone(), directory).await?;
        if !node.is_directory()
            || !node.is_dirty_add()
            || node.is_staged()
            || node.child().is_some()
        {
            break;
        }
        node_discard_fused(state.clone(), repository.clone(), directory).await?;
        directory = node.parent;
    }
    Ok(directory)
}

/// Nodes walks over a tree found stale, each with the tree it belongs to, held for
/// `apply_pending_discards` until every walk that marks the tree has drained.
///
/// A walk crosses into linked and layer trees, so what it holds can belong to several.
#[derive(Default)]
pub struct WalkDiscards {
    batches: SegQueue<DiscardBatch>,
    /// Whether a directory marked dirty as added and not staged leaves with the last node it held.
    emptied_adds: bool,
}

/// Nodes of `state` a walk found stale.
pub(in crate::state) struct DiscardBatch {
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    nodes: Vec<NodeID>,
}

impl WalkDiscards {
    /// Discards held for a walk after which a directory marked dirty as added and not staged leaves
    /// the tree with the last node it held, as one added for what the walk drops.
    pub(crate) fn emptying_adds() -> Self {
        Self {
            batches: SegQueue::new(),
            emptied_adds: true,
        }
    }

    /// Hold `nodes` of `state` for discarding.
    #[lore_macro::test_pub]
    pub(crate) fn queue(
        &self,
        state: Arc<State>,
        repository: Arc<RepositoryContext>,
        nodes: Vec<NodeID>,
    ) {
        if !nodes.is_empty() {
            self.batches.push(DiscardBatch {
                state,
                repository,
                nodes,
            });
        }
    }

    /// Whether no node is held, which leaves nothing to [apply](Self::apply).
    pub(crate) fn is_empty(&self) -> bool {
        self.batches.is_empty()
    }

    /// Discard every node held. Call only once every walk holding nodes here has drained.
    #[lore_macro::test_pub]
    pub(crate) async fn apply(&self) -> Result<(), StateError> {
        while let Some(batch) = self.batches.pop() {
            discard_pending(
                batch.state,
                batch.repository,
                batch.nodes,
                self.emptied_adds,
            )
            .await?;
        }
        Ok(())
    }
}
