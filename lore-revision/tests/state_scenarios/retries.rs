// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A checked attach or reattach whose publish found its directory's chain changed looks its name
//! up again through only what was prepended since the lookup that missed (`find_subnode_since`),
//! so a retry costs the nodes added meanwhile rather than the whole chain, and still finds a name
//! another writer took meanwhile; where a move's weight landed, it walks the whole chain again.
//!
//! The finds' steps and the children walks' steps are counted apart (`step_counting`): a find's
//! lookup is a `NameSearch`, a retry's search since its mark a children walk. Each scenario runs
//! every schedule within its preemption bound, the widest it explores in a few minutes, three to
//! six: the race takes one, a writer landing between a lookup and its publish.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore_revision::node::NodeID;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::AttachOutcome;
use lore_revision::state::State;
use lore_revision::state::step_counting;

use crate::harness::catalogue::modelled::Modelling;
use crate::harness::catalogue::ops;
use crate::harness::fixtures::Tree;
use crate::harness::invariants::Rules;
use crate::harness::model::Modelled;
use crate::harness::scheduler::Exploration;
use crate::harness::scheduler::Op;
use crate::harness::scheduler::Scenario;

/// The files `d/c0` to `d/c7` before the ops, so one lookup in `d` takes this many find steps.
const CHILDREN: u64 = 8;

struct Context {
    tree: Tree,
    /// The find and walk steps counted on the scenario's thread when its setup ended.
    before: (u64, u64),
    /// Each op's answer.
    answers: [OnceLock<Option<AttachOutcome>>; 2],
}

impl Modelled for Context {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        self.tree.states()
    }
}

/// What a run did: each op's answer, and the find and walk steps it took.
struct Run<'a> {
    context: &'a Context,
    answers: [Option<AttachOutcome>; 2],
    find_steps: u64,
    walk_steps: u64,
}

impl Run<'_> {
    fn id(&self, path: &str) -> NodeID {
        self.context.tree.id(path)
    }

    fn retried(&self) -> bool {
        self.context.tree.state.attach_retries() > 0
    }
}

fn steps() -> (u64, u64) {
    (
        step_counting::thread_chain_steps(),
        step_counting::thread_walk_steps(),
    )
}

/// `d` holding `CHILDREN` files, with `extra` beside them.
async fn setup(extra: &'static [&'static str]) -> Context {
    let mut paths: Vec<String> = (0..CHILDREN).map(|child| format!("d/c{child}")).collect();
    paths.extend(extra.iter().map(|path| (*path).to_string()));
    let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
    let tree = Tree::new(&paths).await;
    Context {
        tree,
        before: steps(),
        answers: [OnceLock::new(), OnceLock::new()],
    }
}

type Answering = fn(&Context) -> Pin<Box<dyn Future<Output = Option<AttachOutcome>> + Send + '_>>;

/// Op `op`, its answer kept.
fn answering(op: usize, run: Answering) -> Op<Context> {
    Box::new(move |c: Arc<Context>| {
        Box::pin(async move {
            let answer = run(&c).await;
            let _ = c.answers[op].set(answer);
        })
    })
}

/// A checked create of `d/x`.
fn create_x(c: &Context) -> Pin<Box<dyn Future<Output = Option<AttachOutcome>> + Send + '_>> {
    Box::pin(ops::checked_create(&c.tree, c.tree.id("d"), "x"))
}

/// A checked create of `d/y`.
fn create_y(c: &Context) -> Pin<Box<dyn Future<Output = Option<AttachOutcome>> + Send + '_>> {
    Box::pin(ops::checked_create(&c.tree, c.tree.id("d"), "y"))
}

/// A checked create of `d/z`.
fn create_z(c: &Context) -> Pin<Box<dyn Future<Output = Option<AttachOutcome>> + Send + '_>> {
    Box::pin(ops::checked_create(&c.tree, c.tree.id("d"), "z"))
}

/// An unserved checked reattach of `e/m` into `d` as `name`.
async fn move_m(c: &Context, name: &str) -> Option<AttachOutcome> {
    let tree = &c.tree;
    let moved = tree
        .state
        .node_reattach(tree.repository.clone(), tree.id("e/m"), tree.id("d"), name)
        .await
        .expect("moving e/m into d");
    Some(moved)
}

/// [`move_m`] as `x`.
fn move_m_x(c: &Context) -> Pin<Box<dyn Future<Output = Option<AttachOutcome>> + Send + '_>> {
    Box::pin(move_m(c, "x"))
}

/// [`move_m`] as `y`.
fn move_m_y(c: &Context) -> Pin<Box<dyn Future<Output = Option<AttachOutcome>> + Send + '_>> {
    Box::pin(move_m(c, "y"))
}

/// An unserved reattach of `e/m` into `d` as `x`, replacing `d/x`.
fn move_m_over_x(c: &Context) -> Pin<Box<dyn Future<Output = Option<AttachOutcome>> + Send + '_>> {
    Box::pin(async move {
        let tree = &c.tree;
        let moved = tree
            .state
            .node_reattach_replacing(
                tree.repository.clone(),
                tree.id("e/m"),
                tree.id("d"),
                "x",
                tree.id("d/x"),
            )
            .await
            .expect("moving e/m over d/x");
        Some(moved)
    })
}

/// An unserved checked reattach of `d/c3` into `e`, which counts a move's weight on `d`.
fn move_c3_out(c: &Context) -> Pin<Box<dyn Future<Output = Option<AttachOutcome>> + Send + '_>> {
    Box::pin(async move {
        let tree = &c.tree;
        let moved = tree
            .state
            .node_reattach(tree.repository.clone(), tree.id("d/c3"), tree.id("e"), "c3")
            .await
            .expect("moving d/c3 into e");
        Some(moved)
    })
}

/// Explores the two ops on `extra`'s tree within `preemptions` preemptions a run, `check`
/// judging each run, which must keep every invariant.
fn explore(
    name: &'static str,
    extra: &'static [&'static str],
    ops: [Answering; 2],
    preemptions: usize,
    check: fn(&Run<'_>),
) {
    step_counting::set_chain_step_counting(true);
    let scenario = Scenario::<Context> {
        name,
        setup: Box::new(move || Box::pin(setup(extra))),
        ops: vec![answering(0, ops[0]), answering(1, ops[1])],
        cancellable: vec![],
        check: Box::new(move |c: Arc<Context>| {
            Box::pin(async move {
                let (find, walk) = steps();
                let run = Run {
                    context: &c,
                    answers: [0, 1].map(|op| c.answers[op].get().copied().flatten()),
                    find_steps: find - c.before.0,
                    walk_steps: walk - c.before.1,
                };
                check(&run);
                crate::holding(&c.tree.state, &c.tree.repository, Rules::CLOSED).await;
            })
        }),
    };
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

fn attached(answer: Option<AttachOutcome>) -> NodeID {
    match answer {
        Some(AttachOutcome::Attached(node)) => node,
        other => panic!("attached: {other:?}"),
    }
}

/// Exactly one of the answers attached, and the other answered the node it attached.
fn one_landed(run: &Run<'_>) -> NodeID {
    let landed: Vec<NodeID> = run
        .answers
        .iter()
        .filter_map(|answer| ops::attached(*answer))
        .collect();
    assert_eq!(landed.len(), 1, "one landed: {:?}", run.answers);
    assert!(
        run.answers
            .contains(&Some(AttachOutcome::Exists(landed[0]))),
        "the other answered it: {:?}",
        run.answers
    );
    landed[0]
}

/// Two creates of other names in one directory: both attach, and no lookup walked the chain
/// again: a retry walked only what was prepended since, one node at most.
#[test]
fn a_retried_create_walks_only_what_was_prepended_since_its_lookup() {
    static RETRIED: AtomicUsize = AtomicUsize::new(0);
    RETRIED.store(0, Ordering::Relaxed);
    explore(
        "retries: two creates of other names in one directory",
        &[],
        [create_x, create_y],
        3,
        |run| {
            attached(run.answers[0]);
            attached(run.answers[1]);
            assert!(
                run.find_steps >= CHILDREN && run.find_steps <= 2 * CHILDREN + 1,
                "two lookups, none again: {} find steps",
                run.find_steps
            );
            if run.retried() {
                RETRIED.fetch_add(1, Ordering::Relaxed);
                assert!(
                    run.walk_steps <= 1,
                    "a retry walked {} nodes",
                    run.walk_steps
                );
            }
        },
    );
    assert!(RETRIED.load(Ordering::Relaxed) > 0, "no run had a retry");
}

/// Two creates of one name in one directory: one attaches, and the other answers its node, a
/// retry finding it among the nodes prepended since its lookup.
#[test]
fn a_retried_create_finds_its_name_prepended_since_its_lookup() {
    static RETRIED: AtomicUsize = AtomicUsize::new(0);
    RETRIED.store(0, Ordering::Relaxed);
    explore(
        "retries: two creates of one name in one directory",
        &[],
        [create_x, create_x],
        4,
        |run| {
            one_landed(run);
            if run.retried() {
                RETRIED.fetch_add(1, Ordering::Relaxed);
            }
        },
    );
    assert!(RETRIED.load(Ordering::Relaxed) > 0, "no run had a retry");
}

/// A move into a directory beside a create of another name there: both land, and no lookup
/// walked the chain again; some run's move searched again, through the one node prepended since.
#[test]
fn a_retried_move_walks_only_what_was_prepended_since_its_lookup() {
    static SEARCHED: AtomicUsize = AtomicUsize::new(0);
    SEARCHED.store(0, Ordering::Relaxed);
    explore(
        "retries: a move into a directory beside a create there",
        &["e/m"],
        [move_m_y, create_z],
        4,
        |run| {
            assert_eq!(attached(run.answers[0]), run.id("e/m"));
            attached(run.answers[1]);
            assert!(
                run.find_steps <= 2 * CHILDREN + 1,
                "two lookups, none again: {} find steps",
                run.find_steps
            );
            assert!(
                run.walk_steps <= 1,
                "a retry walked {} nodes",
                run.walk_steps
            );
            if run.walk_steps > 0 {
                SEARCHED.fetch_add(1, Ordering::Relaxed);
            }
        },
    );
    assert!(
        SEARCHED.load(Ordering::Relaxed) > 0,
        "no run searched again since a lookup"
    );
}

/// A move into a directory as a name a create there takes too: one lands, and the other answers
/// the node that did.
#[test]
fn a_move_and_a_create_of_one_name_land_one() {
    explore(
        "retries: a move and a create of one name",
        &["e/m"],
        [move_m_x, create_x],
        4,
        |run| {
            one_landed(run);
        },
    );
}

/// Whether the replacing move beside the create looked its name up again, its section finding the
/// chain changed: no create retried, and more find steps than one lookup each takes, the create's
/// missing over the `CHILDREN + 1` nodes `d` holds, the move's meeting `x` at the head or behind
/// `z`. A move's retry counts in no attach retry.
fn replacing_move_retried(run: &Run<'_>) -> bool {
    !run.retried() && run.find_steps > CHILDREN + 3
}

/// A move replacing the node holding its name, beside a create of another name there: the move
/// lands over the holder, which a retry after a lookup that met it finds by walking the whole
/// chain again, and the create lands.
#[test]
fn a_replacing_move_beside_a_create_lands_over_its_holder() {
    static RETRIED: AtomicUsize = AtomicUsize::new(0);
    RETRIED.store(0, Ordering::Relaxed);
    explore(
        "retries: a replacing move beside a create",
        &["d/x", "e/m"],
        [move_m_over_x, create_z],
        6,
        |run| {
            assert_eq!(attached(run.answers[0]), run.id("e/m"));
            attached(run.answers[1]);
            if replacing_move_retried(run) {
                RETRIED.fetch_add(1, Ordering::Relaxed);
                assert_eq!(
                    run.walk_steps, 0,
                    "the move's retry searched only what was prepended since"
                );
            }
        },
    );
    assert!(
        RETRIED.load(Ordering::Relaxed) > 0,
        "no run had the move retry"
    );
}

/// A create beside a move out of its directory, which counts a move's weight there: where the
/// move lands between the create's lookup and its publish, the create's retry finds the mark stale
/// and walks the whole chain again; the create lands either way.
#[test]
fn a_retried_create_after_a_move_out_walks_the_whole_chain() {
    static RETRIED: AtomicUsize = AtomicUsize::new(0);
    RETRIED.store(0, Ordering::Relaxed);
    explore(
        "retries: a create beside a move out of its directory",
        &["e/"],
        [create_x, move_c3_out],
        4,
        |run| {
            attached(run.answers[0]);
            attached(run.answers[1]);
            if run.retried() {
                RETRIED.fetch_add(1, Ordering::Relaxed);
                assert!(
                    run.find_steps >= 2 * CHILDREN - 1,
                    "a stale retry walked the whole chain again: {} find steps",
                    run.find_steps
                );
            }
        },
    );
    assert!(RETRIED.load(Ordering::Relaxed) > 0, "no run had a retry");
}
