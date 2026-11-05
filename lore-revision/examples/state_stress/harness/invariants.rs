// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The contract's invariants over a quiescent State: the tree's shape, the slots, the dirty state
//! and the names, read through the State's public readers; and that the next sweep reaches every
//! slot neither live nor free (C15), and a sweep left none but in a subtree the provider retains
//! ([`swept`]).

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

use lore_base::types::Hash;
use lore_revision::node::BLOCK_NODE_COUNT;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::NodeIDExt;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_revision::state::StateError;
use lore_storage::hash::hash_string;

/// Which invariants apply beyond those every State keeps.
#[derive(Clone, Copy, Debug, Default)]
pub struct Rules {
    /// Every live dirty node's live non-root ancestors are dirty (C11c). A served State keeps
    /// it at every moment; an unserved one at every quiescent point.
    pub dirty_closure: bool,
    /// No two live siblings share a name hash: the checked tiers' guarantee.
    pub unique_names: bool,
    /// No live node carries extension flags: a State never served.
    pub no_ext_flags: bool,
    /// No build is in flight, so no typed slot is outside every chain: a C API tree, whose
    /// calls build nothing private.
    pub no_builds: bool,
}

impl Rules {
    /// What an exclusive claim's writer leaves at a quiescent point of a C API tree: dirty
    /// closure, unique names, no extension flags and no build in flight.
    pub const CLOSED: Rules = Rules {
        dirty_closure: true,
        unique_names: true,
        no_ext_flags: true,
        no_builds: true,
    };
}

/// What a check found besides violations.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// Live nodes, the root included.
    pub live: usize,
    /// Discarded slots neither live nor on a free list: detached, in flight or pending, the model's
    /// to tell apart (`StateModel::unreleased`).
    pub detached: usize,
    /// Every slot of a block in memory neither live nor free: those `detached` counts, and the nodes
    /// below them.
    pub unreleased: BTreeSet<NodeID>,
    /// Live nodes by id, with their records, for scenario checks.
    pub nodes: BTreeMap<NodeID, Node>,
    /// Each live node's path from the root.
    pub paths: BTreeMap<NodeID, String>,
}

impl Report {
    /// The live node at `path`, if any.
    pub fn at(&self, path: &str) -> Option<(NodeID, Node)> {
        self.paths
            .iter()
            .find(|(_, p)| p.as_str() == path)
            .map(|(id, _)| (*id, self.nodes[id]))
    }
}

/// Checks `state` against the invariants `rules` selects; every violation found, or the report.
pub async fn check(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    rules: Rules,
) -> Result<Report, Vec<String>> {
    let mut violations = Vec::new();
    let mut report = Report::default();
    let root = state
        .node(repository.clone(), ROOT_NODE)
        .await
        .map_err(|error| vec![format!("the root reads: {error}")])?;
    report.nodes.insert(ROOT_NODE, root);
    report.paths.insert(ROOT_NODE, String::new());
    let limit = state.block_count() * BLOCK_NODE_COUNT + 1;
    let mut pending = vec![ROOT_NODE];
    while let Some(dir) = pending.pop() {
        let record = report.nodes[&dir];
        if !record.is_directory() && dir != ROOT_NODE {
            continue;
        }
        let dir_path = report.paths[&dir].clone();
        let mut hashes = BTreeSet::new();
        let mut next = record.child();
        let mut steps = 0;
        while let Some(child) = next {
            steps += 1;
            if steps > limit {
                violations.push(format!("the chain of {dir} ('{dir_path}') does not end"));
                break;
            }
            let node = match state.node(repository.clone(), child).await {
                Ok(node) => node,
                Err(error) => {
                    violations.push(format!("child {child} of {dir} reads: {error}"));
                    break;
                }
            };
            if node.is_discarded() {
                violations.push(format!(
                    "discarded node {child} in the chain of {dir} ('{dir_path}')"
                ));
            }
            if node.parent != dir {
                violations.push(format!(
                    "node {child} in the chain of {dir} ('{dir_path}') names parent {}",
                    node.parent
                ));
            }
            if report.nodes.insert(child, node).is_some() {
                violations.push(format!("node {child} is reached twice, again under {dir}"));
                break;
            }
            let name = match state.node_name_clone(repository.clone(), child).await {
                Ok(name) => name,
                Err(error) => {
                    violations.push(format!("the name of {child} reads: {error}"));
                    String::new()
                }
            };
            if hash_string(&name) != node.name_hash {
                violations.push(format!(
                    "node {child} '{name}' does not match its name hash"
                ));
            }
            if rules.unique_names && !hashes.insert(node.name_hash) {
                violations.push(format!(
                    "two children of {dir} ('{dir_path}') hash as '{name}'"
                ));
            }
            let path = if dir_path.is_empty() {
                name
            } else {
                format!("{dir_path}/{name}")
            };
            if rules.dirty_closure && node.is_dirty() && dir != ROOT_NODE && !record.is_dirty() {
                violations.push(format!(
                    "dirty '{path}' ({child}) under clean '{dir_path}' ({dir})"
                ));
            }
            if rules.no_ext_flags && node.ext_flags != 0 {
                violations.push(format!(
                    "'{path}' ({child}) carries ext flags {:x}",
                    node.ext_flags
                ));
            }
            report.paths.insert(child, path);
            if node.is_directory() && !node.is_link() {
                pending.push(child);
            }
            next = node.sibling();
        }
    }
    report.live = report.nodes.len();
    match free_slots(state, &report) {
        Ok((detached, unreached, free)) => {
            report.detached = detached.len();
            report.unreleased = detached.iter().chain(&unreached).copied().collect();
            for node in unreached {
                if rules.no_builds {
                    violations.push(format!(
                        "node {node}, not Discarded, is in no chain with no build in flight"
                    ));
                } else if let Some(violation) =
                    outside_every_chain(state, repository, (&report, &free), node).await
                {
                    violations.push(violation);
                }
            }
            violations.extend(unswept(state, repository, &report).await);
        }
        Err(mut found) => violations.append(&mut found),
    }
    if violations.is_empty() {
        Ok(report)
    } else {
        Err(violations)
    }
}

/// Walks every resident block's free list: each slot on it is Discarded and below the block's
/// node count, and none is live. No pending clear stands, as a settle drops its own once it
/// ends, finished, refused or cancelled; a block holding changes not in the store is recorded
/// changed, with the State dirty, so a save writes it; and each block is registered at most once,
/// exactly where its registered bit is set (C14). Answers the Discarded slots
/// that are neither free nor live, the slots that are none of the three, and the free ones.
#[allow(clippy::type_complexity)]
fn free_slots(
    state: &Arc<State>,
    report: &Report,
) -> Result<(Vec<NodeID>, Vec<NodeID>, BTreeSet<NodeID>), Vec<String>> {
    let mut violations = Vec::new();
    let mut detached = Vec::new();
    let mut unreached = Vec::new();
    let mut all_free = BTreeSet::new();
    for block_index in 0..state.block_count() {
        let Some(block) = state.resident_block(block_index) else {
            continue;
        };
        let (count, head, unused, discarded, changed) = {
            let reader = block.read();
            let data = reader.node_block();
            let (discarded, typed): (Vec<usize>, Vec<usize>) =
                (0..data.node_count as usize).partition(|&index| data.node[index].is_discarded());
            unreached.extend(
                typed
                    .into_iter()
                    .map(|index| node_id(block_index, index))
                    .filter(|id| !report.nodes.contains_key(id)),
            );
            (
                data.node_count,
                data.node_unused,
                data.node_unused_count,
                discarded,
                reader.is_dirty(),
            )
        };
        if changed && !(state.block_recorded_changed(block_index) && state.is_dirty()) {
            violations.push(format!(
                "block {block_index} holds changes no save would write"
            ));
        }
        for index in (0..count as usize).filter(|&index| block.clear_pending(index)) {
            let id = node_id(block_index, index);
            violations.push(format!("a pending clear outlived its settle on {id}"));
        }
        let mut free = BTreeSet::new();
        let mut at = head;
        for _ in 0..unused {
            if at >= count {
                violations.push(format!(
                    "block {block_index}'s free list reaches slot {at}, past its count {count}"
                ));
                break;
            }
            let node = block.node(at as usize);
            if !node.is_discarded() {
                violations.push(format!(
                    "block {block_index}'s free slot {at} is not discarded"
                ));
            }
            if !free.insert(at) {
                violations.push(format!("block {block_index}'s free list loops at {at}"));
                break;
            }
            at = node.sibling;
        }
        for index in discarded {
            let id = node_id(block_index, index);
            if report.nodes.contains_key(&id) {
                violations.push(format!("live node {id} is discarded"));
            } else if !free.contains(&(index as u32)) {
                detached.push(id);
            }
        }
        all_free.extend(
            free.iter()
                .map(|&index| node_id(block_index, index as usize)),
        );
        drop(block);
    }
    violations.extend(state.registration_faults());
    unused_chain(state, &mut violations);
    if violations.is_empty() {
        Ok((detached, unreached, all_free))
    } else {
        Err(violations)
    }
}

/// What a save leaves for the next save to read: each block registered at most once, exactly where
/// its registered bit is set (C14), and the unused chain of blocks as [`check`] reads it.
pub fn after_save(state: &Arc<State>) -> Vec<String> {
    let mut violations = state.registration_faults();
    unused_chain(state, &mut violations);
    violations
}

/// The tree's unused chain of blocks: none on it twice, and, where every block on it is in
/// memory, every resident block with room reachable by a grab: on the chain, the last block,
/// which a grab takes up when the chain is empty, or one that freed a slot while full and is
/// registered for the next save, which threads it onto the chain. A State taking new nodes from
/// fresh blocks keeps its tree's free slots off the chain by design.
fn unused_chain(state: &Arc<State>, violations: &mut Vec<String>) {
    let count = state.block_count();
    let mut on_chain = BTreeSet::new();
    let mut at = state.unused_head() as usize;
    while at < count {
        if !on_chain.insert(at) {
            violations.push(format!("block {at} is on the unused chain twice"));
            return;
        }
        let Some(block) = state.resident_block(at) else {
            return;
        };
        at = block.read().node_block().block_unused_next as usize;
    }
    if state.attach_slot_resident() {
        return;
    }
    for index in 0..count.saturating_sub(1) {
        let Some(block) = state.resident_block(index) else {
            continue;
        };
        let (room, marked) = {
            let reader = block.read();
            (!reader.is_full(), reader.has_first_unused_node())
        };
        // The registrations are read with the block's guard dropped: the State's lock comes first.
        if room && !on_chain.contains(&index) && !(marked && state.block_recorded_changed(index)) {
            violations.push(format!("block {index} has room no grab reaches"));
        }
    }
}

/// Where `node`, a slot that is not Discarded and that no chain from the root reaches, hangs
/// from: it must hang below a Discarded node that is not free, as the members of a detached
/// subtree and the nodes of a build not yet published do. One whose ancestors reach a live node
/// without meeting a Discarded one is a typed slot outside every chain, which a cancelled attach
/// leaves (H8); one below a free slot outlived an ancestor that was freed before it. The free
/// slots are those of resident blocks alone, so a slot below a free one in a block not in memory
/// goes unreported.
async fn outside_every_chain(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    (report, free): (&Report, &BTreeSet<NodeID>),
    node: NodeID,
) -> Option<String> {
    let limit = state.block_count() * BLOCK_NODE_COUNT + 1;
    let mut at = node;
    for _ in 0..limit {
        let parent = match state.node(repository.clone(), at).await {
            Ok(record) => record.parent,
            Err(error) => return Some(format!("unreached node {at} reads: {error}")),
        };
        if report.nodes.contains_key(&parent) {
            return Some(format!(
                "node {node}, not Discarded, is in no chain: its ancestors reach live {parent}"
            ));
        }
        if free.contains(&parent) {
            return Some(format!(
                "node {node}, not Discarded, hangs below {parent}, a free slot"
            ));
        }
        match state.node(repository.clone(), parent).await {
            Ok(record) if record.is_discarded() => return None,
            Ok(_) => at = parent,
            Err(error) => return Some(format!("unreached node {parent} reads: {error}")),
        }
    }
    Some(format!("the ancestors of unreached node {node} do not end"))
}

/// Every slot of `report.unreleased` the next sweep reaches (C15): it scans each block in memory
/// the State noted whose flag is set, and walks each `Discarded` slot there, holding a node, as a
/// root, with the subtree below it. One it reaches none of, as an earlier sweep dropped or a write
/// that left a slot detached without flagging its block leaves one, stays for good, stored with
/// every save. Where a walk meets a block that does not load, nothing is judged: what lies past it
/// a later sweep reaches.
async fn unswept(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    report: &Report,
) -> Vec<String> {
    let mut slots = Slots::default();
    let mut reached = BTreeSet::new();
    for &root in &report.unreleased {
        let block_index = NodeBlock::index(root);
        let Some(block) = state.resident_block(block_index) else {
            continue;
        };
        let (discarded, flagged) = {
            let reader = block.read();
            (
                reader.node_block().node[Node::index(root)].is_discarded(),
                reader.is_unswept(),
            )
        };
        drop(block);
        if !discarded
            || !flagged
            || !state.block_noted_unswept(block_index)
            || reached.contains(&root)
        {
            continue;
        }
        if sweep_walk(state, repository, root, &mut slots, &mut reached)
            .await
            .is_err()
        {
            return Vec::new();
        }
    }
    let mut violations = Vec::new();
    for &node in report.unreleased.difference(&reached) {
        let block_index = NodeBlock::index(node);
        let flagged = state
            .resident_block(block_index)
            .is_some_and(|block| block.read().is_unswept());
        violations.push(format!(
            "slot {node}, neither live nor free, no sweep reaches: its block {block_index} is {} \
             and {}",
            if flagged { "flagged" } else { "not flagged" },
            if state.block_noted_unswept(block_index) {
                "noted"
            } else {
                "not noted"
            }
        ));
    }
    violations
}

/// What a sweep left, where it answered done, as `report`, a check right after it, finds the
/// State: every slot of a block in memory neither live nor free is a node of the subtree of a
/// `Discarded` root there, walked as the sweep walks it, that holds a node `retained` names, the
/// provider's test of what the kernel references; and each such root's block is flagged and noted
/// for the next sweep (C15). A walk meeting a block that does not load leaves nothing judged.
pub async fn swept(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    report: &Report,
    retained: impl Fn(NodeID) -> bool,
) -> Vec<String> {
    let mut slots = Slots::default();
    let mut kept = BTreeSet::new();
    let mut violations = Vec::new();
    for &root in &report.unreleased {
        let discarded = state
            .resident_block(NodeBlock::index(root))
            .is_some_and(|block| block.node(Node::index(root)).is_discarded());
        if !discarded {
            continue;
        }
        let mut members = BTreeSet::new();
        if sweep_walk(state, repository, root, &mut slots, &mut members)
            .await
            .is_err()
        {
            return Vec::new();
        }
        if !members.iter().any(|&member| retained(member)) {
            continue;
        }
        let block_index = NodeBlock::index(root);
        let flagged = state
            .resident_block(block_index)
            .is_some_and(|block| block.read().is_unswept());
        if !flagged || !state.block_noted_unswept(block_index) {
            violations.push(format!(
                "{root}, whose subtree holds a retained node, lies in block {block_index}, not \
                 flagged and noted for the next sweep"
            ));
        }
        kept.extend(members);
    }
    violations.extend(
        report
            .unreleased
            .difference(&kept)
            .map(|node| format!("slot {node}, neither live nor free, a sweep left behind")),
    );
    violations
}

/// The slots of the blocks a walk read that hold a node, below the block's node count and off its
/// free list, by block index.
#[derive(Default)]
struct Slots {
    blocks: BTreeMap<usize, (usize, BTreeSet<usize>)>,
}

impl Slots {
    /// Whether `node`'s slot holds a node, its block loaded where it is not in memory.
    async fn hold(
        &mut self,
        state: &Arc<State>,
        repository: &Arc<RepositoryContext>,
        node: NodeID,
    ) -> Result<bool, StateError> {
        let block_index = NodeBlock::index(node);
        if let std::collections::btree_map::Entry::Vacant(entry) = self.blocks.entry(block_index) {
            let block = state.block(repository.clone(), block_index).await?;
            let held = {
                let reader = block.read();
                let data = reader.node_block();
                let mut free = BTreeSet::new();
                let mut at = data.node_unused as usize;
                for _ in 0..data.node_unused_count {
                    if at >= BLOCK_NODE_COUNT || !free.insert(at) {
                        break;
                    }
                    at = data.node[at].sibling as usize;
                }
                (data.node_count as usize, free)
            };
            entry.insert(held);
        }
        let (count, free) = &self.blocks[&block_index];
        let index = Node::index(node);
        Ok(index < *count && !free.contains(&index))
    }
}

/// Adds `root` and every node of the subtree below it to `reached`, walked as the sweep walks it:
/// through the chains its directories hold, never the root's own siblings; a chain ending at a slot
/// holding no node, at a node naming another parent, or at a loop. Links are leaves. Fails where a
/// block does not load.
async fn sweep_walk(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    root: NodeID,
    slots: &mut Slots,
    reached: &mut BTreeSet<NodeID>,
) -> Result<(), StateError> {
    reached.insert(root);
    let record = state.node(repository.clone(), root).await?;
    let mut chains = Vec::new();
    if record.is_directory() && record.child.is_valid_node_id() {
        chains.push((root, record.child));
    }
    while let Some((directory, head)) = chains.pop() {
        let mut seen = BTreeSet::new();
        let mut at = head;
        while at.is_valid_node_id() && seen.insert(at) {
            if !slots.hold(state, repository, at).await? {
                break;
            }
            let node = state.node(repository.clone(), at).await?;
            if node.parent != directory {
                break;
            }
            if reached.insert(at) && node.is_directory() && node.child.is_valid_node_id() {
                chains.push((at, node.child));
            }
            at = node.sibling;
        }
    }
    Ok(())
}

fn node_id(block_index: usize, index: usize) -> NodeID {
    NodeID::try_from(block_index * BLOCK_NODE_COUNT + index).expect("a node id")
}

/// What a settle's clear compares of a child (C11b): its name hash, its kind by its file and link
/// flags, its content hash and, for a file, its mode.
type ChildKey = (u64, u16, Hash, u16);

/// The children of each directory a walk reached, by the directory, each as a settle compares it
/// ([`ChildKey`]), sorted, so two directories holding the same multiset compare equal.
fn children_by_parent(walk: &Report) -> BTreeMap<NodeID, Vec<ChildKey>> {
    let mut children: BTreeMap<NodeID, Vec<ChildKey>> = BTreeMap::new();
    for (&id, node) in &walk.nodes {
        if id == ROOT_NODE {
            continue;
        }
        let kind = node.flags & (NodeFlags::File | NodeFlags::Link).bits();
        let mode = if node.is_file() { node.mode } else { 0 };
        children.entry(node.parent).or_default().push((
            node.name_hash,
            kind,
            node.address.hash,
            mode,
        ));
    }
    for keys in children.values_mut() {
        keys.sort_unstable();
    }
    children
}

/// C11c's second half on a served State its invariants' walk reached as `served`: every clean live
/// directory but the root and those along a swap's changed paths (`swapped`) holds the children of
/// its committed directory, the node `committed_of` maps it to in the committed State walked as
/// `committed`, as a settle's clear compares them (C11b): as multisets of [`ChildKey`]. A
/// directory `committed_of` maps to nothing, made since the commit, is passed over: no settle
/// compares it with a committed one.
pub fn committed_children(
    served: &Report,
    (committed, committed_of): (&Report, &BTreeMap<NodeID, NodeID>),
    swapped: &BTreeSet<NodeID>,
) -> Vec<String> {
    let (held, stored) = (children_by_parent(served), children_by_parent(committed));
    let mut violations = Vec::new();
    for (&dir, node) in &served.nodes {
        if dir == ROOT_NODE || !node.is_directory() || node.is_dirty() || swapped.contains(&dir) {
            continue;
        }
        let Some(&against) = committed_of.get(&dir) else {
            continue;
        };
        let path = &served.paths[&dir];
        if !committed
            .nodes
            .get(&against)
            .is_some_and(Node::is_directory)
        {
            violations.push(format!(
                "clean '{path}' ({dir}) stands for committed {against}, no live directory (C11c)"
            ));
            continue;
        }
        let (holds, wanted) = (
            held.get(&dir).map_or(&[][..], Vec::as_slice),
            stored.get(&against).map_or(&[][..], Vec::as_slice),
        );
        if holds != wanted {
            violations.push(format!(
                "clean '{path}' ({dir}) holds {} children, committed {against} {}, as name hash, \
                 kind, content and mode they differ: {holds:x?} against {wanted:x?} (C11c)",
                holds.len(),
                wanted.len()
            ));
        }
    }
    violations
}

/// The block holding `node`, loaded.
pub async fn block_of(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    node: NodeID,
) -> Arc<NodeBlock> {
    state
        .block(repository.clone(), NodeBlock::index(node))
        .await
        .expect("the block loads")
}
