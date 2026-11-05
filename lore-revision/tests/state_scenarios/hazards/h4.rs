// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! H4: an edit landing while a save writes a block it has registered losing that registration: a
//! save clearing every block it writes leaves an edit that found its block dirty already, and
//! registered nothing, in no save, and the next save answers the old signature; file-metadata
//! blocks the same. A save reading the tree's unused head at its start and writing it back at its
//! end writes over any grab's change meanwhile. Each scenario runs an edit beside a save, every
//! interleaving of their steps, and checks that a save after both holds what the State holds.
//! What each catches:
//! - the attach, metadata and add scenarios, an edit's lost registration; the add, a head written
//!   back over the save's, or the tree's node-block list updated from a copy read before its
//!   write, and its file-metadata variant that list's file-metadata counterpart so updated;
//! - the discard scenario, a saved block not kept registered while it awaits threading;
//! - the threading one, a threading outside the unused permit, a head not read again under the
//!   permit, or an empty chain's head left unset, and its counter a missing point before the
//!   permit (the empty chain is reached through the grab's `Point::Retry`, which no counter
//!   witnesses);
//! - the full-head ones, a head threaded behind itself, and the clean-head one, a head left out of
//!   the save where a writer, not the save's threading, dirtied it;
//! - the dropped ones, a missing `Redirty` mark, a droppable `Point::Threading`, or a clean head
//!   the save threaded left unregistered;
//! - the build scenario, a save beside a build: a save writing, and dropping from memory, a block
//!   a build in flight holds slots in.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore_base::types::Hash;
use lore_revision::node;
use lore_revision::node::BLOCK_NODE_COUNT;
use lore_revision::node::Node;
use lore_revision::node::NodeFileMetadata;
use lore_revision::node::NodeFileMetadataBlock;
use lore_revision::node::NodeID;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_storage::hash::hash_string;

use crate::harness::catalogue::modelled::Modelling;
use crate::harness::catalogue::modelled::modelled;
use crate::harness::catalogue::ops;
use crate::harness::fixtures::Tree;
use crate::harness::invariants;
use crate::harness::invariants::Rules;
use crate::harness::model::Modelled;
use crate::harness::scheduler::Exploration;
use crate::harness::scheduler::Op;
use crate::harness::scheduler::Scenario;

struct Context {
    tree: Tree,
    /// The signature the scenario's save answered.
    saved: OnceLock<Hash>,
    /// Whether the scenario's save started.
    started: AtomicBool,
    /// Whether the scenario's edit ran while its save was under way.
    during: AtomicBool,
    /// The node the setup chose for the scenario's edit, and its path; the root where none.
    node: (NodeID, String),
}

impl Context {
    fn new(tree: Tree) -> Self {
        Self {
            tree,
            saved: OnceLock::new(),
            started: AtomicBool::new(false),
            during: AtomicBool::new(false),
            node: (node::ROOT_NODE, String::new()),
        }
    }

    /// Records whether the scenario's save is under way: started, not done.
    fn note_during(&self) {
        let saving = self.started.load(Ordering::Relaxed) && self.saved.get().is_none();
        self.during.store(saving, Ordering::Relaxed);
    }
}

impl Modelled for Context {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        self.tree.states()
    }
}

/// The scenario's save, its signature kept.
fn save() -> Op<Context> {
    Box::new(|c: Arc<Context>| {
        Box::pin(async move {
            c.started.store(true, Ordering::Relaxed);
            let signature = c.tree.save().await.expect("the State saves");
            c.saved.set(signature).expect("one save");
        })
    })
}

/// The paths of `state`'s live nodes.
async fn paths(c: &Context, state: &Arc<State>) -> BTreeSet<String> {
    crate::holding(state, &c.tree.repository, Rules::default())
        .await
        .paths
        .into_values()
        .collect()
}

/// Whether the save the scenario ran holds `name` below `dir`. Read by name, from the blocks
/// that lookup reaches: a save beside an edit is not a snapshot of one moment, so its State as a
/// whole need not hold the invariants (`State::serialize`).
async fn holds(c: &Context, dir: NodeID, name: &str) -> bool {
    let first = c.tree.load(*c.saved.get().expect("the save ran")).await;
    holds_in(c, &first, dir, name).await
}

/// The tree `state` loaded from the store holds.
async fn stored(c: &Context, state: &State) -> lore_revision::state::Tree {
    state
        .tree(c.tree.repository.clone())
        .await
        .expect("the tree loads")
}

/// Whether `state` holds `name` below `dir`, found by name.
async fn holds_in(c: &Context, state: &Arc<State>, dir: NodeID, name: &str) -> bool {
    match state
        .find_subnode(c.tree.repository.clone(), dir, hash_string(name))
        .await
    {
        Ok(_) => true,
        Err(error) if error.is_node_not_found() => false,
        Err(error) => panic!("a find fails: {error}"),
    }
}

/// The blocks the unused chain of `state` reaches, in chain order, read through its blocks,
/// loading those not in memory; asserts none is on it twice.
async fn chain(c: &Context, state: &State) -> Vec<usize> {
    let tree = stored(c, state).await;
    let mut chain = Vec::new();
    let mut at = tree.block_unused_first as usize;
    while at < tree.block_count as usize {
        assert!(
            !chain.contains(&at),
            "block {at} is on the unused chain twice, after {chain:?}"
        );
        chain.push(at);
        at = state
            .block(c.tree.repository.clone(), at)
            .await
            .expect("the block loads")
            .read()
            .node_block()
            .block_unused_next as usize;
    }
    chain
}

/// Adds files below `dir` until block `block` is full; the last file added.
async fn fill(tree: &Tree, dir: NodeID, block: usize) -> NodeID {
    let mut index = 0;
    loop {
        let name = format!("more{index}");
        let id = tree
            .state
            .node_add(tree.repository.clone(), dir, ops::file(&name), &name)
            .await
            .expect("a file adds");
        if node::NodeBlock::index(id) == block && Node::index(id) == BLOCK_NODE_COUNT - 1 {
            return id;
        }
        index += 1;
    }
}

/// A file of the setup's filler directory, which lies in block 0, and its path.
async fn filler_file(tree: &Tree) -> (NodeID, String) {
    let id = tree
        .state
        .node_children(tree.repository.clone(), tree.id("fill"))
        .await
        .expect("the filler's children")[0];
    let report = crate::holding(&tree.state, &tree.repository, Rules::default()).await;
    (id, report.paths[&id].clone())
}

async fn discard(tree: &Tree, id: NodeID) {
    lore_revision::state::node_discard_fused(tree.state.clone(), tree.repository.clone(), id)
        .await
        .expect("the discard");
}

/// Explores `ops` on `setup`'s tree, every interleaving run: the unused permit and the save's
/// tasks are not recorded for the scheduler, so sleep sets could not trust a step's footprint.
fn explore(scenario: Scenario<Context>) {
    crate::explored(
        scenario,
        Modelling::default(),
        Exploration {
            sleep_sets: false,
            ..Exploration::default()
        },
    );
}

/// [`explore`] to `preemptions` preemptions, for a scenario whose whole space takes too long to
/// run: every schedule with that many preemptions or fewer runs.
fn explore_bounded(scenario: Scenario<Context>, preemptions: usize) {
    crate::explored(
        scenario,
        Modelling::default(),
        Exploration {
            sleep_sets: false,
            preemptions,
            ..Exploration::default()
        },
    );
}

/// A checked create of `d/y` beside a save of the State, which writes `d`'s block: a save
/// after both holds `d/y`, whether the create landed before the save wrote the block or after.
#[test]
fn an_attach_beside_a_save_goes_into_the_next_save() {
    let raced = Arc::new(AtomicUsize::new(0));
    let seen = raced.clone();
    let scenario = Scenario::<Context> {
        name: "h4: an attach beside a save",
        setup: Box::new(|| Box::pin(async { Context::new(Tree::new(&["d/f"]).await) })),
        ops: vec![
            Box::new(|c: Arc<Context>| {
                Box::pin(async move {
                    let _ = ops::checked_create(&c.tree, c.tree.id("d"), "y").await;
                    c.note_during();
                })
            }),
            save(),
        ],
        cancellable: vec![],
        check: Box::new(move |c: Arc<Context>| {
            let raced = seen.clone();
            Box::pin(async move {
                let live = paths(&c, &c.tree.state).await;
                assert!(live.contains("d/y"), "the create landed");
                let next = c.tree.saved().await;
                assert_eq!(
                    paths(&c, &next).await,
                    live,
                    "the next save holds every node the State does"
                );
                if c.during.load(Ordering::Relaxed) && !holds(&c, c.tree.id("d"), "y").await {
                    raced.fetch_add(1, Ordering::Relaxed);
                }
            })
        }),
    };
    explore(scenario);
    assert!(
        raced.load(Ordering::Relaxed) > 0,
        "no run had the create end, while the save was under way, after it wrote 'd''s block"
    );
}

/// The file metadata of `id`'s slot, read from its block.
async fn metadata_of(tree: &Tree, state: &Arc<State>, id: NodeID) -> Hash {
    let slot = node::node_to_file_metadata(id);
    state
        .block_file_metadata(tree.repository.clone(), NodeFileMetadataBlock::index(slot))
        .await
        .expect("the file metadata block")
        .node(NodeFileMetadata::index(slot))
        .metadata
}

/// A metadata set of `d/f` beside a save of the State, whose file metadata block the setup
/// changed: a save after both holds the value set, whether the set landed before the save wrote
/// the block or after.
#[test]
fn a_file_metadata_write_beside_a_save_goes_into_the_next_save() {
    let (before, after) = (Hash::from([1u8; 32]), Hash::from([2u8; 32]));
    let raced = Arc::new(AtomicUsize::new(0));
    let seen = raced.clone();
    let scenario = Scenario::<Context> {
        name: "h4: a file metadata write beside a save",
        setup: Box::new(move || {
            Box::pin(async move {
                let tree = Tree::new(&["d/f", "d/g"]).await;
                tree.set_file_metadata(tree.id("d/g"), before).await;
                Context::new(tree)
            })
        }),
        ops: vec![
            Box::new(move |c: Arc<Context>| {
                Box::pin(async move {
                    c.note_during();
                    c.tree.set_file_metadata(c.tree.id("d/f"), after).await;
                })
            }),
            save(),
        ],
        cancellable: vec![],
        check: Box::new(move |c: Arc<Context>| {
            let raced = seen.clone();
            Box::pin(async move {
                let f = c.tree.id("d/f");
                assert_eq!(metadata_of(&c.tree, &c.tree.state, f).await, after);
                let next = c.tree.saved().await;
                assert_eq!(
                    metadata_of(&c.tree, &next, f).await,
                    after,
                    "the next save holds the value set"
                );
                let first = c.tree.load(*c.saved.get().expect("the save ran")).await;
                if c.during.load(Ordering::Relaxed)
                    && metadata_of(&c.tree, &first, f).await != after
                {
                    raced.fetch_add(1, Ordering::Relaxed);
                }
            })
        }),
    };
    explore(scenario);
    assert!(
        raced.load(Ordering::Relaxed) > 0,
        "no run had the set land, while the save was under way, after it wrote the block"
    );
}

/// A create in a tree whose only block is full, beside a save: the create takes its slot from a
/// fresh block it puts at the head of the unused chain, and the head stays there, whether the
/// create landed before the save, after it, or while it wrote its blocks; a save after both
/// holds the create.
#[test]
fn an_add_beside_a_save_keeps_the_unused_head_it_set() {
    explore(add_beside_a_save("h4: an add beside a save", false));
}

/// [`an_add_beside_a_save_keeps_the_unused_head_it_set`] with a file metadata block for the save
/// to write too: the tree's file metadata list is updated in place, so a create taking its fresh
/// block while the save wrote that list keeps it.
#[test]
fn an_add_beside_a_save_writing_file_metadata_keeps_the_unused_head_it_set() {
    explore(add_beside_a_save(
        "h4: an add beside a save writing file metadata",
        true,
    ));
}

fn add_beside_a_save(name: &'static str, metadata: bool) -> Scenario<Context> {
    Scenario::<Context> {
        name,
        setup: Box::new(move || {
            Box::pin(async move {
                let tree = Tree::spread(&["d/"], "fill", &[]).await;
                assert_eq!(tree.state.block_count(), 1, "the fill took block 0 whole");
                if metadata {
                    let (id, _) = filler_file(&tree).await;
                    tree.set_file_metadata(id, Hash::from([3u8; 32])).await;
                }
                Context::new(tree)
            })
        }),
        ops: vec![
            Box::new(|c: Arc<Context>| {
                Box::pin(async move {
                    let _ = ops::checked_create(&c.tree, c.tree.id("d"), "y").await;
                })
            }),
            save(),
        ],
        cancellable: vec![],
        check: Box::new(|c: Arc<Context>| {
            Box::pin(async move {
                assert_eq!(
                    c.tree.state.block_count(),
                    2,
                    "the create took a fresh block"
                );
                assert_eq!(
                    c.tree.state.unused_head(),
                    1,
                    "the fresh block heads the unused chain"
                );
                let live = paths(&c, &c.tree.state).await;
                assert!(live.contains("d/y"), "the create landed");
                let next = c.tree.saved().await;
                assert_eq!(
                    paths(&c, &next).await,
                    live,
                    "the next save holds every node the State does"
                );
                assert_eq!(
                    stored(&c, &next).await.block_unused_first,
                    1,
                    "the next save stored the head"
                );
            })
        }),
    }
}

/// A discard freeing a slot in a full block off the unused chain, beside a save writing that
/// block: the block stays reachable by a grab, threaded onto the chain by the save or kept
/// registered for the next one, whether the discard landed before the save threaded, between
/// that and the block's image, or after; the next save threads it.
#[test]
fn a_slot_freed_in_a_full_block_beside_a_save_stays_reachable() {
    let raced = Arc::new(AtomicUsize::new(0));
    let seen = raced.clone();
    let scenario = Scenario::<Context> {
        name: "h4: a discard in a full block beside a save",
        setup: Box::new(|| {
            Box::pin(async {
                let tree = Tree::spread(&["d/"], "fill", &["e/"]).await;
                assert_eq!(tree.state.unused_head(), 1, "block 0 left the chain full");
                let (id, path) = filler_file(&tree).await;
                tree.save().await;
                // Block 0 alone changed and registered, so the save writes it.
                let block = invariants::block_of(&tree.state, &tree.repository, id).await;
                assert!(block.write_for_tests().mark_dirty());
                tree.state.block_modified_for_tests(block, 0);
                Context {
                    node: (id, path),
                    ..Context::new(tree)
                }
            })
        }),
        ops: vec![
            Box::new(|c: Arc<Context>| {
                Box::pin(async move {
                    discard(&c.tree, c.node.0).await;
                })
            }),
            save(),
        ],
        cancellable: vec![],
        check: Box::new(move |c: Arc<Context>| {
            let raced = seen.clone();
            Box::pin(async move {
                let live = paths(&c, &c.tree.state).await;
                assert!(!live.contains(&c.node.1), "the discard landed");
                let first = c.tree.load(*c.saved.get().expect("the save ran")).await;
                let freed = first
                    .node(c.tree.repository.clone(), c.node.0)
                    .await
                    .expect("the freed slot reads")
                    .is_discarded();
                // Block 0 not behind the head, block 1, in the first save.
                let threaded = first
                    .block(c.tree.repository.clone(), 1)
                    .await
                    .expect("block 1 loads")
                    .read()
                    .node_block()
                    .block_unused_next
                    == 0;
                if freed && !threaded {
                    raced.fetch_add(1, Ordering::Relaxed);
                }
                let next = c.tree.saved().await;
                assert!(
                    chain(&c, &c.tree.state).await.contains(&0),
                    "the next save threaded block 0"
                );
                assert_eq!(
                    paths(&c, &next).await,
                    live,
                    "the next save holds every node the State does"
                );
            })
        }),
    };
    explore(scenario);
    assert!(
        raced.load(Ordering::Relaxed) > 0,
        "no run had the discard land between the threading and the block's image"
    );
}

/// A create beside a save threading a block onto the unused chain, whose head is full: the
/// create retires the head and pushes a fresh block under the unused permit, and the save links
/// the block behind the head under it, so neither undoes the other: the chain reaches the fresh
/// block and the threaded one wherever the create landed: with its retire and push between the
/// save's read of the head and its permit (the save reads the head again), with its retire alone
/// there (the save finds the chain empty and makes it; the grab's `Point::Retry` opens that
/// window, which no counter witnesses), or with its grab failing on the full head
/// while the save holds the permit (it waits, and retires onto the threaded block). Every
/// schedule of up to three preemptions runs: each of those takes one, and four take several
/// minutes more.
#[test]
fn a_create_beside_a_save_threading_a_block_keeps_both_on_the_unused_chain() {
    let moved = Arc::new(AtomicUsize::new(0));
    let seen = moved.clone();
    let scenario = Scenario::<Context> {
        name: "h4: a create beside a save threading a block",
        setup: Box::new(|| {
            Box::pin(async {
                let tree = Tree::spread(&["d/"], "fill", &["e/"]).await;
                fill(&tree, tree.id("e"), 1).await;
                assert_eq!(
                    (tree.state.block_count(), tree.state.unused_head()),
                    (2, 1),
                    "both blocks full, block 1 heading the chain"
                );
                let (id, _) = filler_file(&tree).await;
                tree.save().await;
                // Block 0 alone changed, marked for the save to thread.
                discard(&tree, id).await;
                Context::new(tree)
            })
        }),
        ops: vec![
            Box::new(|c: Arc<Context>| {
                Box::pin(async move {
                    c.note_during();
                    let _ = ops::checked_create(&c.tree, c.tree.id("d"), "y").await;
                })
            }),
            save(),
        ],
        cancellable: vec![],
        check: Box::new(move |c: Arc<Context>| {
            let moved = seen.clone();
            Box::pin(async move {
                // A fresh block taken while the save was under way: the create retired the head
                // and pushed one between the save's read of the head and its permit, the one
                // window where the head still moves (see the doc).
                if c.during.load(Ordering::Relaxed) && c.tree.state.block_count() == 3 {
                    moved.fetch_add(1, Ordering::Relaxed);
                }
                let live = paths(&c, &c.tree.state).await;
                assert!(live.contains("d/y"), "the create landed");
                let chain = chain(&c, &c.tree.state).await;
                assert!(
                    chain.contains(&0),
                    "the threaded block is on the chain: {chain:?}"
                );
                if c.tree.state.block_count() == 3 {
                    assert!(
                        chain.contains(&2),
                        "the fresh block is on the chain: {chain:?}"
                    );
                }
                let next = c.tree.saved().await;
                assert_eq!(
                    paths(&c, &next).await,
                    live,
                    "the next save holds every node the State does"
                );
            })
        }),
    };
    explore_bounded(scenario, 3);
    assert!(
        moved.load(Ordering::Relaxed) > 0,
        "no run had the create move the head between the save's head read and its permit"
    );
}

/// A discard freeing a slot in the unused chain's full head beside a save threading a block
/// behind it: the head keeps its place, and the mark the discard set on it, which the save may
/// not see, keeps it registered until a save spends it, so no later save threads it behind
/// itself, a cycle; wherever the discard landed. The save stores the block linked behind the
/// head.
#[test]
fn a_slot_freed_in_the_full_head_beside_a_save_threading_leaves_it_the_head() {
    explore_head_beside_a_save(
        "h4: a discard in the full head beside a save threading",
        true,
    );
}

/// [`a_slot_freed_in_the_full_head_beside_a_save_threading_leaves_it_the_head`] with the head
/// clean when the save takes its blocks: a discard turning it dirty after that, which the link
/// then finds dirty already, leaves it out of the save's list, so the save writes it all the
/// same, and stores the block linked behind it.
#[test]
fn a_slot_freed_in_a_clean_full_head_beside_a_save_threading_goes_into_that_save() {
    explore_head_beside_a_save(
        "h4: a discard in a clean full head beside a save threading",
        false,
    );
}

fn explore_head_beside_a_save(name: &'static str, registered: bool) {
    let raced = Arc::new(AtomicUsize::new(0));
    let seen = raced.clone();
    let scenario = Scenario::<Context> {
        name,
        setup: Box::new(move || {
            Box::pin(async move {
                let tree = Tree::spread(&["d/"], "fill", &["e/"]).await;
                let last = fill(&tree, tree.id("e"), 1).await;
                let (filler, _) = filler_file(&tree).await;
                tree.save().await;
                if registered {
                    // Block 1, the full head, changed and registered ahead of block 0.
                    let head = invariants::block_of(&tree.state, &tree.repository, last).await;
                    assert!(head.write_for_tests().mark_dirty());
                    tree.state.block_modified_for_tests(head, 1);
                }
                // Block 0 frees a slot and is marked for the save to thread.
                discard(&tree, filler).await;
                Context {
                    node: (last, String::new()),
                    ..Context::new(tree)
                }
            })
        }),
        ops: vec![
            Box::new(|c: Arc<Context>| {
                Box::pin(async move {
                    c.note_during();
                    discard(&c.tree, c.node.0).await;
                })
            }),
            save(),
        ],
        cancellable: vec![],
        check: Box::new(move |c: Arc<Context>| {
            let raced = seen.clone();
            Box::pin(async move {
                // The discard marked the head after the save's scan, so the save left the mark.
                let marked = c
                    .tree
                    .state
                    .resident_block(1)
                    .is_some_and(|block| block.read().has_first_unused_node());
                if c.during.load(Ordering::Relaxed) && marked {
                    raced.fetch_add(1, Ordering::Relaxed);
                }
                let first = c.tree.load(*c.saved.get().expect("the save ran")).await;
                assert!(
                    chain(&c, &first).await.contains(&0),
                    "the save stored block 0 behind the head"
                );
                let next = c.tree.saved().await;
                assert_eq!(
                    chain(&c, &next).await,
                    chain(&c, &c.tree.state).await,
                    "the next save stored the unused chain the State holds"
                );
                let live = paths(&c, &c.tree.state).await;
                assert_eq!(
                    paths(&c, &next).await,
                    live,
                    "the next save holds every node the State does"
                );
            })
        }),
    };
    explore(scenario);
    assert!(
        raced.load(Ordering::Relaxed) > 0,
        "no run had the discard mark the head during the save, after its scan"
    );
}

/// A swap's copy of `p/d` beside a save, the copy's root taking block 0's last free slot and its
/// files a fresh block that only the copy's registrations hold: the save may write the fresh block
/// mid-build and drop it from memory, and the copy reads it back, so a save after both holds the
/// copy whole.
#[test]
fn a_build_beside_a_save_goes_into_the_next_save_whole() {
    let raced = Arc::new(AtomicUsize::new(0));
    let seen = raced.clone();
    let scenario = Scenario::<Context> {
        name: "h4: a build beside a save",
        setup: Box::new(|| {
            Box::pin(async {
                let tree = Tree::spread(&["p/"], "fill", &[]).await;
                let (id, _) = filler_file(&tree).await;
                discard(&tree, id).await;
                tree.save().await;
                tree.state.mark_served();
                Context::new(tree)
            })
        }),
        ops: vec![
            Box::new(|c: Arc<Context>| {
                Box::pin(async move {
                    ops::swap_copy_tree(&c.tree, c.tree.id("p"), "d", &["a", "b"]).await;
                })
            }),
            save(),
        ],
        cancellable: vec![],
        check: Box::new(move |c: Arc<Context>| {
            let raced = seen.clone();
            Box::pin(async move {
                let live = paths(&c, &c.tree.state).await;
                for path in ["p/d", "p/d/a", "p/d/b"] {
                    assert!(live.contains(path), "the copy holds {path}");
                }
                // The save wrote the copy's fresh block, holding `a`, before the copy's publish.
                let report =
                    crate::holding(&c.tree.state, &c.tree.repository, Rules::default()).await;
                let (a, _) = report.at("p/d/a").expect("the copy holds a");
                let first = c.tree.load(*c.saved.get().expect("the save ran")).await;
                let wrote_a = node::NodeBlock::index(a) == 1
                    && first
                        .node(c.tree.repository.clone(), a)
                        .await
                        .is_ok_and(|record| record.name_hash == hash_string("a"));
                if wrote_a && !holds_in(&c, &first, c.tree.id("p"), "d").await {
                    raced.fetch_add(1, Ordering::Relaxed);
                }
                let next = c.tree.saved().await;
                assert_eq!(
                    paths(&c, &next).await,
                    live,
                    "the next save holds every node the State does"
                );
            })
        }),
    };
    explore(scenario);
    assert!(
        raced.load(Ordering::Relaxed) > 0,
        "no run had the save write the copy's fresh block before its publish"
    );
}

/// How a scenario dropping a save is modelled: a save writes no node's dirt, so a run dropping it
/// leaves closure as it was, and the checkpoint holds it still wherever it walks the State.
fn keeping_closure() -> Modelling {
    Modelling {
        drops_excuse_closure: false,
        ..Modelling::default()
    }
}

/// A save dropped at each point it parks at, its State holding a create and a block that freed a
/// slot while full: whatever it wrote, it leaves every change registered under a dirty State
/// and every block with room reachable by a grab, so a save after it holds what the State does.
/// A save dropped after clearing the State's flag marks the State dirty again, and none is
/// dropped between threading blocks onto the unused chain and linking its head, where no await
/// stands.
#[test]
fn a_save_dropped_part_way_leaves_its_changes_to_the_next() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let seen = dropped.clone();
    let scenario = Scenario::<Context> {
        name: "h4: a save dropped part way",
        setup: Box::new(|| {
            Box::pin(async {
                let tree = Tree::spread(&["d/"], "fill", &["e/"]).await;
                let (id, _) = filler_file(&tree).await;
                discard(&tree, id).await;
                let created = ops::checked_create(&tree, tree.id("d"), "y").await;
                assert!(
                    ops::attached(created).is_some(),
                    "'d/y' attaches: {created:?}"
                );
                Context::new(tree)
            })
        }),
        ops: vec![save()],
        cancellable: vec![0],
        check: Box::new(move |c: Arc<Context>| {
            let dropped = seen.clone();
            Box::pin(async move {
                // Dropped past the State flag's clear: the tree address and the format are set
                // by this save alone, as the State was never saved, the format with the clear.
                if c.saved.get().is_none()
                    && !c.tree.state.tree_hash().is_zero()
                    && c.tree.state.format() != 0
                {
                    dropped.fetch_add(1, Ordering::Relaxed);
                }
                let live = paths(&c, &c.tree.state).await;
                let next = c.tree.saved().await;
                assert!(
                    chain(&c, &c.tree.state).await.contains(&0),
                    "the next save threaded block 0"
                );
                assert_eq!(
                    paths(&c, &next).await,
                    live,
                    "the next save holds every node the State does"
                );
            })
        }),
    };
    let name = scenario.name;
    let covered = modelled(scenario, keeping_closure()).explore(Exploration {
        sleep_sets: false,
        cancellation: true,
        ..Exploration::default()
    });
    eprintln!("'{name}': {covered:?}");
    assert!(covered.complete, "explored {} runs", covered.runs);
    assert!(
        dropped.load(Ordering::Relaxed) > 0,
        "no run dropped the save after it cleared the State's flag"
    );
}

/// [`a_save_dropped_part_way_leaves_its_changes_to_the_next`] with the chain's head clean when the
/// save links it to the block it threads: a save dropped after that link leaves the head
/// registered, so the next save writes it.
#[test]
fn a_save_dropped_after_linking_a_clean_head_leaves_it_registered() {
    let linked = Arc::new(AtomicUsize::new(0));
    let seen = linked.clone();
    let scenario = Scenario::<Context> {
        name: "h4: a save dropped after linking a clean head",
        setup: Box::new(|| {
            Box::pin(async {
                let tree = Tree::spread(&["d/"], "fill", &["e/"]).await;
                let created = ops::checked_create(&tree, tree.id("e"), "y").await;
                assert!(
                    ops::attached(created).is_some(),
                    "'e/y' attaches: {created:?}"
                );
                let (id, _) = filler_file(&tree).await;
                tree.save().await;
                // Block 0 alone changed, marked for the save to thread behind block 1, the head.
                discard(&tree, id).await;
                Context::new(tree)
            })
        }),
        ops: vec![save()],
        cancellable: vec![0],
        check: Box::new(move |c: Arc<Context>| {
            let linked = seen.clone();
            Box::pin(async move {
                let head_links = c
                    .tree
                    .state
                    .resident_block(1)
                    .is_some_and(|block| block.read().node_block().block_unused_next == 0);
                if c.saved.get().is_none() && head_links {
                    linked.fetch_add(1, Ordering::Relaxed);
                }
                let live = paths(&c, &c.tree.state).await;
                let next = c.tree.saved().await;
                assert_eq!(
                    chain(&c, &next).await,
                    chain(&c, &c.tree.state).await,
                    "the next save stored the unused chain the State holds"
                );
                assert_eq!(
                    paths(&c, &next).await,
                    live,
                    "the next save holds every node the State does"
                );
            })
        }),
    };
    let name = scenario.name;
    let covered = modelled(scenario, keeping_closure()).explore(Exploration {
        sleep_sets: false,
        cancellation: true,
        ..Exploration::default()
    });
    eprintln!("'{name}': {covered:?}");
    assert!(covered.complete, "explored {} runs", covered.runs);
    assert!(
        linked.load(Ordering::Relaxed) > 0,
        "no run dropped the save after it linked the head"
    );
}
