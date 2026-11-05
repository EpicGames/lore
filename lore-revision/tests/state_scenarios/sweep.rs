// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The sweep of detached slots (C15) and its served form. Each scenario leaves slots neither live
//! nor free as production leaves them: adds and attaches dropped between their grab and their
//! publish, a discard failing part way on a store that fails its reads, the C API delete's pending
//! queue, a VFS provider's removals; then sweeps at a quiescent point and checks that every such
//! slot is freed, once, nothing live is, and on a served State nothing the provider retains. A root
//! whose subtree reaches a block that fails to load stays for a later sweep, and every chain mark
//! and absence proof taken before a sweep that freed a slot answers stale after it, as a slot it
//! freed may be taken again, while those taken after stand.
//!
//! The explored ones run under the model, which holds a sweep made through the catalogue's call to
//! the plan it reads as the call begins ([`crate::harness::model::Model`]), and the checkpoint,
//! whose invariants check that the next sweep reaches every slot neither live nor free.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::Absent;
use lore_revision::state::AddedSince;
use lore_revision::state::AttachCheck;
use lore_revision::state::AttachOutcome;
use lore_revision::state::Detach;
use lore_revision::state::DiscardCounts;
use lore_revision::state::State;
use lore_storage::hash::hash_string;

use crate::harness::catalogue::modelled::Modelling;
use crate::harness::catalogue::ops;
use crate::harness::catalogue::ops::Marked;
use crate::harness::faults::FailingStore;
use crate::harness::fixtures::Tree;
use crate::harness::invariants;
use crate::harness::invariants::Report;
use crate::harness::invariants::Rules;
use crate::harness::model::Modelled;
use crate::harness::scheduler::Exploration;
use crate::harness::scheduler::Op;
use crate::harness::scheduler::Scenario;

/// What a sweep freed, files and directories together.
fn total(counts: DiscardCounts) -> usize {
    usize::try_from(counts.file_count + counts.directory_count).expect("a count")
}

/// A sweep of `tree` at a quiescent point, its served form keeping `retained` where `served`, and
/// what it left: checked before and after, every live node of before still live, nothing left
/// neither live nor free but in a subtree holding a retained node, whose root's block is flagged
/// for the next sweep ([`invariants::swept`]). The check before, the sweep's counts, and the check
/// after.
async fn swept_whole(
    tree: &Tree,
    served: bool,
    retained: &[NodeID],
) -> (Report, DiscardCounts, Report) {
    let before = crate::holding(&tree.state, &tree.repository, Rules::default()).await;
    let counts = if served {
        ops::served_sweep(tree, retained).await
    } else {
        ops::sweep(tree).await
    };
    let after = crate::holding(&tree.state, &tree.repository, Rules::default()).await;
    let left = invariants::swept(&tree.state, &tree.repository, &after, |node| {
        retained.contains(&node)
    })
    .await;
    assert!(left.is_empty(), "the sweep left: {left:#?}");
    let live = |report: &Report| report.nodes.keys().copied().collect::<BTreeSet<NodeID>>();
    assert_eq!(live(&before), live(&after), "the sweep freed no live node");
    (before, counts, after)
}

/// Explores `op` on a tree holding `d/f` and the empty directory `e`, served where `served`, the op
/// dropped at every point it parks at, then sweeps as a stage, a commit or a provider's pause does:
/// every slot the run left neither live nor free is freed, each once, and no live node; some run
/// leaves one to free.
fn dropped_then_swept(name: &'static str, served: bool, op: Op<Tree>) {
    let leaked = Arc::new(AtomicUsize::new(0));
    let counted = leaked.clone();
    let scenario = Scenario::<Tree> {
        name,
        setup: Box::new(move || {
            Box::pin(async move {
                let tree = Tree::new(&["d/f", "e/"]).await;
                if served {
                    tree.state.mark_served();
                }
                tree
            })
        }),
        ops: vec![op],
        cancellable: vec![0],
        check: Box::new(move |tree: Arc<Tree>| {
            let leaked = counted.clone();
            Box::pin(async move {
                let (before, counts, after) = swept_whole(&tree, served, &[]).await;
                assert!(after.unreleased.is_empty(), "the sweep freed every slot");
                assert_eq!(
                    total(counts),
                    before.unreleased.len(),
                    "the sweep freed each slot left once"
                );
                if !before.unreleased.is_empty() {
                    leaked.fetch_add(1, Ordering::Relaxed);
                }
            })
        }),
    };
    let covered = crate::explored(
        scenario,
        Modelling::default(),
        Exploration {
            cancellation: true,
            ..Exploration::default()
        },
    );
    assert!(
        leaked.load(Ordering::Relaxed) > 0,
        "no run dropped the op holding a slot, in {} runs",
        covered.runs
    );
}

/// A scan's add of the directory `d/y`, then its parent's mark, as the scan adds one it finds new.
#[test]
fn an_add_dropped_at_every_point_leaves_a_slot_the_sweep_frees() {
    dropped_then_swept(
        "sweep: an add dropped at every point",
        false,
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                ops::scan_add(&tree, tree.id("d"), "y").await;
            })
        }),
    );
}

/// The C API's add of the file `d/y`, under the shared claim, which its commit sweeps after.
#[test]
fn a_c_api_add_dropped_at_every_point_leaves_a_slot_the_sweep_frees() {
    dropped_then_swept(
        "sweep: a C API add dropped at every point",
        false,
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                ops::staged_add(&tree, tree.id("d"), ops::file("y"), "y").await;
            })
        }),
    );
}

/// A VFS provider's create of `d/y` on a served State, whose provider sweeps where it pauses.
#[test]
fn a_served_create_dropped_at_every_point_leaves_a_slot_the_served_sweep_frees() {
    dropped_then_swept(
        "sweep: a served create dropped at every point",
        true,
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                ops::served_create(&tree, tree.id("d"), "y").await;
            })
        }),
    );
}

/// A swap's copy of a directory of two files into `e`: a build, which a drop before its publish
/// leaves whole, its root in flight and its files below it, all freed by the served sweep.
#[test]
fn a_swap_copy_dropped_at_every_point_leaves_a_build_the_served_sweep_frees_whole() {
    dropped_then_swept(
        "sweep: a swap copy dropped at every point",
        true,
        Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                ops::swap_copy_tree(&tree, tree.id("e"), "copy", &["i", "j"]).await;
            })
        }),
    );
}

/// A tree over a store that fails its reads when told to, which a checkpoint mends.
struct Failing {
    tree: Tree,
    store: Arc<FailingStore>,
}

impl Modelled for Failing {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        self.tree.states()
    }

    fn mend(&self) {
        self.store.fail(false);
    }

    fn faulted(&self) -> bool {
        self.store.failed_outside_calls() > 0
    }
}

/// How a discard fails part way.
#[derive(Clone, Copy)]
enum Failure {
    /// In the directory's own chain: `p/d`'s chain runs `x`, then `w` past the fill, whose block
    /// fails to load, then `z`. `x` is freed in place, so its `sibling` links its block's free
    /// list, and the discard makes `w` the directory's head, the chain past the failed block kept.
    Head,
    /// In a task below: `p/d`'s chain runs `s` then `x`; `x` is freed, `s` marked, and `s`'s task
    /// fails at `w`, past the fill, leaving `s`'s chain and `p/d` itself unfreed, `p/d`'s head the
    /// freed `x`.
    Below,
}

/// The tree `failure` names, saved and loaded again through a failing store with only `p/d`'s
/// block in memory, the store reading still: the scenario's op makes it fail, once the model's
/// start read what it needs.
async fn failing_discard(failure: Failure) -> Failing {
    let tree = match failure {
        Failure::Head => {
            let tree = Tree::spread(&["p/keep", "p/d/z", "p/d/x"], "fill", &["p/d/w"]).await;
            let (d, x, w, z) = (
                tree.id("p/d"),
                tree.id("p/d/x"),
                tree.id("p/d/w"),
                tree.id("p/d/z"),
            );
            let low = invariants::block_of(&tree.state, &tree.repository, d).await;
            let high = invariants::block_of(&tree.state, &tree.repository, w).await;
            tree.state
                .node_edit_for_tests(&low, d, |node| node.child = x);
            tree.state
                .node_edit_for_tests(&low, x, |node| node.sibling = w);
            tree.state
                .node_edit_for_tests(&high, w, |node| node.sibling = z);
            tree
        }
        Failure::Below => Tree::spread(&["p/keep", "p/d/x", "p/d/s/z"], "fill", &["p/d/s/w"]).await,
    };
    let (tree, store) = tree.failing().await;
    invariants::block_of(&tree.state, &tree.repository, tree.id("p/d")).await;
    assert!(
        tree.state.resident_block(1).is_none(),
        "the block past the fill is not in memory"
    );
    Failing { tree, store }
}

/// A fused discard of `p/d` failing part way as `failure` says, then, the store reading again, the
/// sweep: it frees `p/d` and everything below it the discard left, the chain past the failed block
/// included, each once, and nothing live.
fn a_failed_discard_then_the_sweep(name: &'static str, failure: Failure) {
    let scenario = Scenario::<Failing> {
        name,
        setup: Box::new(move || Box::pin(failing_discard(failure))),
        ops: vec![Box::new(|c: Arc<Failing>| {
            Box::pin(async move {
                c.store.fail(true);
                let discarded = lore_revision::state::node_discard_fused(
                    c.tree.state.clone(),
                    c.tree.repository.clone(),
                    c.tree.id("p/d"),
                )
                .await;
                assert!(discarded.is_err(), "the block past the fill fails to load");
            })
        })],
        cancellable: Vec::new(),
        check: Box::new(move |c: Arc<Failing>| {
            Box::pin(async move {
                let (before, counts, after) = swept_whole(&c.tree, false, &[]).await;
                let paths = failing_discard_paths(failure);
                let left: BTreeSet<NodeID> = paths.iter().map(|path| c.tree.id(path)).collect();
                // The block past the fill, which the discard failed to load, the sweep loads.
                assert!(
                    before.unreleased.is_subset(&left)
                        && before.unreleased.contains(&left_root(&c)),
                    "the discard left its root and what it did not free: {:?}",
                    before.unreleased
                );
                assert!(
                    c.tree.state.resident_block(1).is_some(),
                    "the sweep loaded the block the discard could not"
                );
                assert_eq!(total(counts), paths.len(), "the sweep freed each once");
                assert!(after.unreleased.is_empty(), "the sweep freed them all");
                assert!(after.at("p/keep").is_some(), "the sibling stays");
            })
        }),
    };
    crate::explored(scenario, Modelling::default(), Exploration::default());
}

/// The root of a failing discard's tree.
fn left_root(c: &Failing) -> NodeID {
    c.tree.id("p/d")
}

/// The paths a discard failing as `failure` says leaves neither live nor free.
fn failing_discard_paths(failure: Failure) -> &'static [&'static str] {
    match failure {
        Failure::Head => &["p/d", "p/d/w", "p/d/z"],
        Failure::Below => &["p/d", "p/d/s", "p/d/s/w", "p/d/s/z"],
    }
}

#[test]
fn a_discard_failing_in_its_own_chain_leaves_the_chain_past_the_block_to_the_sweep() {
    a_failed_discard_then_the_sweep(
        "sweep: a discard failing in its own chain, then the sweep",
        Failure::Head,
    );
}

#[test]
fn a_discard_failing_below_leaves_its_marked_directories_to_the_sweep() {
    a_failed_discard_then_the_sweep(
        "sweep: a discard failing in a task below, then the sweep",
        Failure::Below,
    );
}

/// A detached directory `p/d`, whose chain reaches `w` past the fill, saved and loaded again, so
/// its block flags itself as it loads; the sweep while the block of `w` fails to load keeps `p/d`
/// and flags its block again; once the store reads, the next sweep frees `p/d` and everything below
/// it.
#[test]
fn a_root_whose_subtree_fails_to_load_stays_for_the_next_sweep() {
    let scenario = Scenario::<Failing> {
        name: "sweep: a root whose subtree fails to load, swept again",
        setup: Box::new(|| {
            Box::pin(async {
                let tree = Tree::spread(&["p/keep", "p/d/x"], "fill", &["p/d/w"]).await;
                tree.state
                    .node_detach(tree.repository.clone(), tree.id("p/d"), Detach::Always)
                    .await
                    .expect("'p/d' detaches");
                let (tree, store) = tree.failing().await;
                invariants::block_of(&tree.state, &tree.repository, tree.id("p/d")).await;
                Failing { tree, store }
            })
        }),
        ops: vec![Box::new(|c: Arc<Failing>| {
            Box::pin(async move {
                c.store.fail(true);
                let counts = ops::sweep(&c.tree).await;
                assert_eq!(total(counts), 0, "a root whose walk fails is kept");
            })
        })],
        cancellable: Vec::new(),
        check: Box::new(|c: Arc<Failing>| {
            Box::pin(async move {
                let d = c.tree.id("p/d");
                let block = NodeBlock::index(d);
                let flagged = c
                    .tree
                    .state
                    .resident_block(block)
                    .is_some_and(|held| held.read().is_unswept());
                assert!(
                    flagged && c.tree.state.block_noted_unswept(block),
                    "the kept root's block is flagged and noted for the next sweep"
                );
                let (before, counts, after) = swept_whole(&c.tree, false, &[]).await;
                assert!(before.unreleased.contains(&d), "'p/d' was kept");
                assert_eq!(total(counts), 3, "the next sweep frees 'p/d', 'x' and 'w'");
                assert!(after.unreleased.is_empty(), "nothing is left");
            })
        }),
    };
    crate::explored(scenario, Modelling::default(), Exploration::default());
}

/// A tree whose `s/t` the C API delete queued pending, and what the sweeps of the scenario's op
/// freed, where it ran them.
struct Pending {
    tree: Tree,
    swept: Mutex<Vec<DiscardCounts>>,
}

impl Modelled for Pending {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        self.tree.states()
    }
}

/// The C API delete's pending subtree `s/t`, holding `a` and, past the fill, `b`, saved and its
/// blocks evicted, so the drain loads them; then two sweeps, as two C API commits run them, the op
/// dropped at every point, the drain's block loads among them: the first drains the queue and frees
/// the subtree whole, the second frees nothing; a run dropping the first, at one of those loads,
/// leaves the queue, which the check's sweep drains.
#[test]
fn a_pending_subtree_drained_by_the_sweep_leaves_nothing_for_the_next() {
    let dropped_first = Arc::new(AtomicUsize::new(0));
    let counted = dropped_first.clone();
    let scenario = Scenario::<Pending> {
        name: "sweep: a pending subtree drained, then swept again",
        setup: Box::new(|| {
            Box::pin(async {
                let tree = Tree::spread(&["s/t/a", "q"], "fill", &["s/t/b"]).await;
                let (s, t) = (tree.id("s"), tree.id("s/t"));
                tree.state
                    .node_discard_pending(tree.repository.clone(), t, s, hash_string("t"))
                    .await
                    .expect("the delete")
                    .expect("'s/t' stands where it was found");
                tree.save().await.expect("the State saves");
                tree.state.evict_blocks_for_tests();
                Pending {
                    tree,
                    swept: Mutex::default(),
                }
            })
        }),
        ops: vec![Box::new(|c: Arc<Pending>| {
            Box::pin(async move {
                for _ in 0..2 {
                    let counts = ops::sweep(&c.tree).await;
                    c.swept.lock().expect("the counts").push(counts);
                }
            })
        })],
        cancellable: vec![0],
        check: Box::new(move |c: Arc<Pending>| {
            let dropped_first = counted.clone();
            Box::pin(async move {
                let swept = c.swept.lock().expect("the counts").clone();
                if swept.is_empty() {
                    dropped_first.fetch_add(1, Ordering::Relaxed);
                }
                if let [first, second] = swept[..] {
                    assert_eq!(
                        (first.file_count, first.directory_count),
                        (2, 1),
                        "the first sweep drains 's/t', 'a' and 'b'"
                    );
                    assert_eq!(total(second), 0, "the second frees nothing");
                }
                let (_, counts, after) = swept_whole(&c.tree, false, &[]).await;
                if swept.len() == 2 {
                    assert_eq!(total(counts), 0, "nothing is left to sweep");
                }
                assert!(after.unreleased.is_empty(), "nothing is pending");
                assert!(after.at("q").is_some(), "the sibling stays");
            })
        }),
    };
    let covered = crate::explored(
        scenario,
        Modelling::default(),
        Exploration {
            cancellation: true,
            ..Exploration::default()
        },
    );
    assert!(
        dropped_first.load(Ordering::Relaxed) > 0,
        "no run dropped the first sweep before it returned, in {} runs",
        covered.runs
    );
}

/// A served tree as a VFS provider serves one: new nodes from fresh blocks, `a`, `b` and `c`
/// dirty, as local changes leave them.
async fn served_tree() -> Tree {
    let tree = Tree::new(&["a/x", "b/y", "c/d/e", "c/h/i", "c/k", "f/"]).await;
    tree.state
        .take_new_nodes_from_fresh_blocks(tree.repository.clone())
        .await
        .expect("new nodes come from fresh blocks");
    tree.state.mark_served();
    for dir in ["a", "b", "c"] {
        assert!(ops::served_mark_dir(&tree, tree.id(dir)).await);
    }
    tree
}

/// A served tree whose provider removed: `a/n`, a file it created; `c/h`, a clean directory a
/// swap detached with `i` below it; `c/d`, a directory with `e` below it; and `f`, an empty one.
/// What the scenario retains, by path.
struct Retaining {
    tree: Tree,
    /// The nodes the provider removed, by the path each had.
    removed: Vec<(&'static str, NodeID)>,
    /// The scenario's sweep returned.
    swept: AtomicBool,
}

impl Modelled for Retaining {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        self.tree.states()
    }
}

impl Retaining {
    fn id(&self, path: &str) -> NodeID {
        self.removed
            .iter()
            .find(|(removed, _)| *removed == path)
            .map_or_else(|| self.tree.id(path), |(_, id)| *id)
    }
}

/// [`Retaining`]'s tree.
async fn removed() -> Retaining {
    let tree = served_tree().await;
    let n = ops::served_create(&tree, tree.id("a"), "n")
        .await
        .expect("'a/n' is created");
    assert!(ops::served_detach(&tree, n).await, "'a/n' is removed");
    assert!(
        ops::swap_detach_if_clean(&tree, tree.id("c/h")).await,
        "the swap detaches 'c/h'"
    );
    assert!(
        ops::served_detach(&tree, tree.id("c/d")).await,
        "'c/d' is removed"
    );
    assert!(
        ops::served_rmdir(&tree, tree.id("f")).await.removed,
        "'f' is removed"
    );
    Retaining {
        tree,
        removed: vec![("a/n", n)],
        swept: AtomicBool::new(false),
    }
}

/// The paths of each removed subtree, its root first.
const REMOVED: [&[&str]; 4] = [&["a/n"], &["c/h", "c/h/i"], &["c/d", "c/d/e"], &["f"]];

/// The provider's sweep where it paused, retaining `a/n`, a removed subtree's root, and `c/h/i`, a
/// node deep in another: those subtrees stay whole and their roots' blocks flagged for the next
/// sweep, the other two are freed, nothing live; a sweep retaining nothing then frees the rest.
#[test]
fn a_served_reclaim_keeps_what_the_provider_retains_and_frees_the_rest() {
    let scenario = Scenario::<Retaining> {
        name: "sweep: a served reclaim retaining two subtrees",
        setup: Box::new(|| Box::pin(removed())),
        ops: vec![Box::new(|c: Arc<Retaining>| {
            Box::pin(async move {
                let retained = [c.id("a/n"), c.id("c/h/i")];
                ops::served_sweep(&c.tree, &retained).await;
                c.swept.store(true, Ordering::Relaxed);
            })
        })],
        cancellable: vec![0],
        check: Box::new(|c: Arc<Retaining>| {
            Box::pin(async move {
                let tree = &c.tree;
                let report = crate::holding(&tree.state, &tree.repository, Rules::default()).await;
                let retained = [c.id("a/n"), c.id("c/h/i")];
                let left = invariants::swept(&tree.state, &tree.repository, &report, |node| {
                    retained.contains(&node)
                })
                .await;
                // A sweep takes nothing before its last await, so a run dropping it left
                // everything; one it returned in left the retained subtrees alone.
                let dropped = !c.swept.load(Ordering::Relaxed);
                assert!(dropped || left.is_empty(), "the reclaim left: {left:#?}");
                for paths in REMOVED {
                    let kept = paths.contains(&"a/n") || paths.contains(&"c/h");
                    for path in paths {
                        assert_eq!(
                            report.unreleased.contains(&c.id(path)),
                            kept || dropped,
                            "'{path}' kept as its subtree is retained"
                        );
                    }
                }
                let (_, _, after) = swept_whole(tree, true, &[]).await;
                assert!(after.unreleased.is_empty(), "released, they are freed");
            })
        }),
    };
    crate::explored(
        scenario,
        Modelling::default(),
        Exploration {
            cancellation: true,
            ..Exploration::default()
        },
    );
}

/// What the stale reads of a served tree hold across a reclaim: the mark of `a`'s chain whose head
/// was the created `a/h`, and an absence of `x` from `a` taken while `a/x` held it, which no
/// listing would take, standing in for one a reused slot makes wrong.
struct Stale {
    tree: Tree,
    h: NodeID,
    marked: Mutex<Option<Marked>>,
    absent: Absent,
}

impl Modelled for Stale {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        self.tree.states()
    }
}

/// An absence of `name` from `dir`, at its log's position now, whatever `dir` holds.
fn absence_now(tree: &Tree, dir: NodeID, name: &str) -> Absent {
    let block = tree
        .state
        .resident_block(NodeBlock::index(dir))
        .expect("the directory's block is in memory");
    let (mark, _) = block.watch_chain(Node::index(dir), None);
    Absent::new(&tree.state, mark.seq, block, dir, hash_string(name))
}

/// A file named `name` added and holding local content, as a served create makes one.
fn local_file(name: &str) -> Node {
    Node {
        flags: (NodeFlags::File | NodeFlags::DirtyAdd).bits(),
        ext_flags: NodeExtFlags::LocalContent.bits(),
        name_hash: hash_string(name),
        ..Node::default()
    }
}

/// A mark of `a`'s chain taken while `a/h` headed it, and an absence of `x`, then `a/h` removed
/// and the provider's sweep freeing it: after the sweep, a create in `a` takes `h`'s slot again and
/// heads `a` under `h`'s id, which a search since the mark, stopping at the head it saw, would pass
/// over; the search answers stale. An attach of `x` by the absence finds `a/x` as a walk does,
/// rather than trusting it.
#[test]
fn marks_and_absences_taken_before_a_reclaim_answer_stale_after_it() {
    let scenario = Scenario::<Stale> {
        name: "sweep: marks and absences across a reclaim",
        setup: Box::new(|| {
            Box::pin(async {
                let tree = served_tree().await;
                let a = tree.id("a");
                let h = ops::served_create(&tree, a, "h")
                    .await
                    .expect("'a/h' is created");
                let (_, marked) = ops::listed_marked(&tree, a)
                    .await
                    .expect("'a' lists with a mark");
                let absent = absence_now(&tree, a, "x");
                assert!(ops::served_detach(&tree, h).await, "'a/h' is removed");
                Stale {
                    tree,
                    h,
                    marked: Mutex::new(Some(marked)),
                    absent,
                }
            })
        }),
        ops: vec![Box::new(|c: Arc<Stale>| {
            Box::pin(async move {
                let counts = ops::served_sweep(&c.tree, &[]).await;
                assert_eq!(total(counts), 1, "the sweep frees 'a/h'");
            })
        })],
        cancellable: Vec::new(),
        check: Box::new(|c: Arc<Stale>| {
            Box::pin(async move {
                let tree = &c.tree;
                let a = tree.id("a");
                let n = ops::served_create(tree, a, "n")
                    .await
                    .expect("'a/n' is created");
                assert_eq!(n, c.h, "the create takes the slot the sweep freed");
                let marked = c.marked.lock().expect("the mark").take().expect("the mark");
                assert_eq!(
                    ops::added_since(tree, &marked, "n").await,
                    Some(AddedSince::Stale),
                    "the mark taken before the reclaim answers stale"
                );
                let attached = tree
                    .state
                    .served()
                    .node_attach(
                        tree.repository.clone(),
                        a,
                        local_file("x"),
                        "x",
                        AttachCheck::Absent(&c.absent),
                    )
                    .await
                    .expect("the attach");
                assert_eq!(
                    attached,
                    Some(AttachOutcome::Exists(tree.id("a/x"))),
                    "the absence taken before the reclaim is not trusted"
                );
            })
        }),
    };
    crate::explored(scenario, Modelling::default(), Exploration::default());
}

/// A mark and an absence taken after a sweep that freed a slot stand: the search since the mark
/// finds a create made after it, and an attach by the absence trusts it, as one taken where the
/// name is held, which no listing takes, shows by attaching a second holder.
#[tokio::test]
async fn marks_and_absences_taken_after_a_reclaim_stand() {
    crate::in_context(async {
        let tree = served_tree().await;
        let a = tree.id("a");
        let h = ops::served_create(&tree, a, "h")
            .await
            .expect("'a/h' is created");
        assert!(ops::served_detach(&tree, h).await, "'a/h' is removed");
        let counts = ops::served_sweep(&tree, &[]).await;
        assert_eq!(total(counts), 1, "the sweep frees 'a/h'");

        let (_, marked) = ops::listed_marked(&tree, a)
            .await
            .expect("'a' lists with a mark");
        let absent = absence_now(&tree, a, "x");
        let m = ops::served_create(&tree, a, "m")
            .await
            .expect("'a/m' is created");
        assert_eq!(
            ops::added_since(&tree, &marked, "m").await,
            Some(AddedSince::Found(m)),
            "the mark taken after the reclaim finds the create"
        );
        let attached = tree
            .state
            .served()
            .node_attach(
                tree.repository.clone(),
                a,
                local_file("x"),
                "x",
                AttachCheck::Absent(&absent),
            )
            .await
            .expect("the attach");
        assert!(
            matches!(attached, Some(AttachOutcome::Attached(_))),
            "the absence taken after the reclaim is trusted: {attached:?}"
        );
    })
    .await;
}

/// A directory a swap built, whose chain a listing logs, logging a create in it and the create's
/// removal, then removed and freed with the file by the provider's sweep; then a swap's copy whose
/// root takes its slot again: the build starts from a directory whose log holds no patch, as a
/// slot freed empties its log.
#[test]
fn a_swap_build_after_a_served_reclaim_reuses_a_freed_directory_slot() {
    let scenario = Scenario::<Tree> {
        name: "sweep: a swap build reusing a slot a served reclaim freed",
        setup: Box::new(|| {
            Box::pin(async {
                let mut tree = served_tree().await;
                let c = tree.id("c");
                let built = ops::swap_copy_tree(&tree, c, "built", &[])
                    .await
                    .expect("the swap builds 'c/built'");
                tree.nodes.push(("c/built".to_string(), built));
                let block = tree
                    .state
                    .resident_block(NodeBlock::index(built))
                    .expect("its block is in memory");
                block.watch_chain(Node::index(built), None);
                let z = ops::served_create(&tree, built, "z")
                    .await
                    .expect("'c/built/z' is created");
                assert!(ops::served_detach(&tree, z).await, "'c/built/z' is removed");
                assert!(
                    ops::served_detach(&tree, built).await,
                    "'c/built' is removed"
                );
                tree
            })
        }),
        ops: vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let counts = ops::served_sweep(&tree, &[]).await;
                assert_eq!(
                    total(counts),
                    2,
                    "the sweep frees 'c/built' and 'c/built/z'"
                );
            })
        })],
        cancellable: Vec::new(),
        check: Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let copied = ops::swap_copy_tree(&tree, tree.id("c"), "copy", &["i", "j"]).await;
                assert_eq!(
                    copied,
                    Some(tree.id("c/built")),
                    "the swap publishes 'c/copy' in the slot the sweep freed"
                );
                crate::holding(&tree.state, &tree.repository, Rules::default()).await;
            })
        }),
    };
    crate::explored(scenario, Modelling::default(), Exploration::default());
}

/// A sweep dropped while its walk of a root's subtree loads a block, here the block of `w` past the
/// fill, which the store stalls, loses no root: the next sweep frees `p/d` and everything below it,
/// and the check between finds the next sweep reaches them.
#[tokio::test]
async fn a_sweep_dropped_while_it_loads_a_block_leaves_its_roots_to_the_next() {
    crate::in_context(async {
        let tree = Tree::spread(&["p/keep", "p/d/x"], "fill", &["p/d/w"]).await;
        tree.state
            .node_detach(tree.repository.clone(), tree.id("p/d"), Detach::Always)
            .await
            .expect("'p/d' detaches");
        let (tree, store) = tree.failing().await;
        invariants::block_of(&tree.state, &tree.repository, tree.id("p/d")).await;
        store.stall(true);
        {
            let mut sweeping = Box::pin(tree.state.node_sweep_detached(&tree.repository));
            tokio::select! {
                biased;
                swept = &mut sweeping => panic!("the sweep ended past a stalled read: {swept:?}"),
                () = async {
                    while store.stalls() == 0 {
                        tokio::task::yield_now().await;
                    }
                } => {}
            }
        }
        store.stall(false);
        let report = crate::holding(&tree.state, &tree.repository, Rules::default()).await;
        assert!(
            report.unreleased.contains(&tree.id("p/d")),
            "the dropped sweep freed nothing"
        );
        let counts = tree
            .state
            .node_sweep_detached(&tree.repository)
            .await
            .expect("the next sweep");
        assert_eq!(total(counts), 3, "the next sweep frees 'p/d', 'x' and 'w'");
    })
    .await;
}

/// A walk of a root's subtree ends at a slot holding no node: a freed slot's `sibling` links its
/// block's free list, and the link, read as a node id, may name a node. Here `f`, removed, heads
/// its chain with `c/k`, freed by an earlier reclaim, whose link names `c/h`, removed and retained,
/// both naming `f` their parent. The sweep frees `f` and keeps `c/h`; a walk going on through the
/// freed slot would find `c/h` below `f`, and keep `f` with it.
#[tokio::test]
async fn a_subtree_walk_ends_at_a_freed_slot() {
    crate::in_context(async {
        let tree = served_tree().await;
        let (f, x, y) = (tree.id("f"), tree.id("c/k"), tree.id("c/h"));
        for node in [y, x, f] {
            assert!(ops::served_detach(&tree, node).await, "{node} is removed");
        }
        let block = invariants::block_of(&tree.state, &tree.repository, f).await;
        for node in [x, y] {
            tree.state
                .node_edit_for_tests(&block, node, |record| record.parent = f);
        }
        let first = ops::served_sweep(&tree, &[y, f]).await;
        assert_eq!(total(first), 1, "the first reclaim frees 'c/k' alone");
        assert_eq!(
            block.read().node_block().node_unused_count,
            1,
            "'c/k' is its block's one free slot, whose link no grab reads"
        );
        tree.state
            .node_edit_for_tests(&block, f, |record| record.child = x);
        tree.state
            .node_edit_for_tests(&block, x, |record| record.sibling = y);

        let second = ops::served_sweep(&tree, &[y]).await;
        let report = crate::holding(&tree.state, &tree.repository, Rules::default()).await;
        assert!(
            !report.unreleased.contains(&f),
            "the reclaim freed 'f', its walk ending at the freed slot"
        );
        assert!(report.unreleased.contains(&y), "it kept 'c/h'");
        assert_eq!(total(second), 1, "it freed 'f' alone");
    })
    .await;
}
