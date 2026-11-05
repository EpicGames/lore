// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! H18: the C API delete freeing slots under a shared claim while adds, modifications and moves run
//! beside it, and marking a subtree in steps a dropped call can stop between. A node moved out of
//! the deleted subtree after its walk is freed while attached, or staged for deletion where it
//! lands; an add or a modification validated before the delete's mark goes on under it; an add
//! published into an added subtree after the walk is left below a freed slot; and an add whose
//! node the walk met before its marks fails the delete as a mixed subtree.
//!
//! Each scenario runs the C API's own calls on one handle, every interleaving of their steps, a
//! spawned task of a call being a task of its own. The tree holds a tracked part (`t`, `q`),
//! cleared of its change flags as a commit clears them, and an added one (`n`). Each scenario
//! counts the runs that end each way, and fails where one way is never reached. A move into the
//! added subtree pins the refusal of a mixed subtree; a dropped delete of the added subtree, a
//! modify in it, and the two deletes of a subtree and of a directory below it cover the delete's
//! paths beside the hazard's. Batches of several entries run through the delete's own plan and
//! apply, their levels and detaches spread over the futures the apply spawns: roots sharing a
//! parent beside an add, nested entries the plan refuses, and a move into an added root.

use std::sync::Arc;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore::revision_tree::delete::LoreRevisionTreeDeleteEntry;
use lore::revision_tree::delete::apply_plan;
use lore::revision_tree::delete::plan_entries;
use lore_revision::event::LoreErrorCode;
use lore_revision::interface::LoreNodeType;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;

use crate::capi;
use crate::capi::Events;
use crate::capi::Handle;
use crate::capi::PENDING;
use crate::capi::Reported;
use crate::harness::catalogue::modelled::Modelling;
use crate::harness::invariants;
use crate::harness::invariants::Report;
use crate::harness::invariants::Rules;
use crate::harness::model::Modelled;
use crate::harness::scheduler;
use crate::harness::scheduler::Exploration;
use crate::harness::scheduler::Op;
use crate::harness::scheduler::Scenario;

struct Context {
    handle: Handle,
    /// Each node the setup made, by path.
    nodes: Vec<(&'static str, NodeID)>,
    /// Each operation's events and status, by operation.
    events: Vec<Events>,
    status: Vec<AtomicI32>,
}

impl Context {
    fn id(&self, path: &str) -> NodeID {
        capi::id_of(&self.nodes, path)
    }

    fn status(&self, op: usize) -> i32 {
        self.status[op].load(Ordering::SeqCst)
    }
}

impl Modelled for Context {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        vec![(self.handle.state(), self.handle.repository())]
    }
}

/// The tree every scenario starts from: tracked `t/a`, `t/sub/b` and `q/y`, and added `n/c`
/// and `n/m/e`; then `deleted`, each deleted in a call of its own.
async fn setup(ops: usize, deleted: &'static [&'static str]) -> Context {
    const TRACKED: &[&str] = &["t/", "t/a", "t/sub/", "t/sub/b", "q/", "q/y"];
    const ADDED: &[&str] = &["n/", "n/c", "n/m/", "n/m/e"];
    let handle = Handle::load().await;
    let mut nodes = handle.seed_paths(TRACKED).await;
    let tracked: Vec<NodeID> = nodes.iter().map(|(_, id)| *id).collect();
    nodes.extend(handle.seed_paths(ADDED).await);
    handle.settle(&tracked).await;
    let context = Context {
        handle,
        nodes,
        events: (0..ops).map(|_| Events::default()).collect(),
        status: (0..ops).map(|_| AtomicI32::new(PENDING)).collect(),
    };
    for path in deleted {
        let events = Events::default();
        let status = capi::delete(context.handle.tree, &[context.id(path)], &events).await;
        assert_eq!(status, 0, "deleting '{path}' succeeds: {:?}", events.all());
    }
    context
}

/// [`setup`], then the tracked `q/y` moved into the added `n/m` and deleted there: a tracked
/// node staged for deletion below an added directory.
async fn setup_moved(ops: usize) -> Context {
    let context = setup(ops, &[]).await;
    let events = Events::default();
    let status = capi::move_node(
        context.handle.tree,
        context.id("q/y"),
        context.id("n/m"),
        "y",
        &events,
    )
    .await;
    assert_eq!(status, 0, "the move succeeds: {:?}", events.all());
    let events = Events::default();
    let status = capi::delete(context.handle.tree, &[context.id("q/y")], &events).await;
    assert_eq!(
        status,
        0,
        "the delete of the moved node succeeds: {:?}",
        events.all()
    );
    context
}

/// A delete call of `path`, as operation `op`.
fn delete(path: &'static str, op: usize) -> Op<Context> {
    Box::new(move |context: Arc<Context>| {
        Box::pin(async move {
            let status = capi::delete(
                context.handle.tree,
                &[context.id(path)],
                &context.events[op],
            )
            .await;
            context.status[op].store(status, Ordering::SeqCst);
        })
    })
}

/// An add call of file `name` under `parent`, as operation `op`.
fn add(parent: &'static str, name: &'static str, op: usize) -> Op<Context> {
    Box::new(move |context: Arc<Context>| {
        Box::pin(async move {
            let status = capi::add(
                context.handle.tree,
                context.id(parent),
                name,
                LoreNodeType::File,
                &context.events[op],
            )
            .await;
            context.status[op].store(status, Ordering::SeqCst);
        })
    })
}

/// A move call of `path` into `destination` as `name`, as operation `op`.
fn move_to(
    path: &'static str,
    destination: &'static str,
    name: &'static str,
    op: usize,
) -> Op<Context> {
    Box::new(move |context: Arc<Context>| {
        Box::pin(async move {
            let status = capi::move_node(
                context.handle.tree,
                context.id(path),
                context.id(destination),
                name,
                &context.events[op],
            )
            .await;
            context.status[op].store(status, Ordering::SeqCst);
        })
    })
}

/// A modify call of file `path`, as operation `op`.
fn modify(path: &'static str, op: usize) -> Op<Context> {
    Box::new(move |context: Arc<Context>| {
        Box::pin(async move {
            let status =
                capi::modify(context.handle.tree, context.id(path), &context.events[op]).await;
            context.status[op].store(status, Ordering::SeqCst);
        })
    })
}

/// The tree's invariants, with none of a build in flight, and every live child of a node staged
/// for deletion staged for deletion too: a delete stages a whole subtree, and an add, a restore
/// or a modification below a deleted node is refused. Then the commit's drain, which must free
/// every pending node and leave the tree as sound; the report is the one from before it.
async fn checked(context: &Context) -> Report {
    let state = context.handle.state();
    let report = crate::holding(&state, &context.handle.repository(), Rules::CLOSED).await;
    for (id, node) in &report.nodes {
        if *id == ROOT_NODE || node.is_staged_delete() {
            continue;
        }
        let parent = report.nodes[&node.parent];
        assert!(
            !parent.is_staged_delete(),
            "'{}' is not staged for deletion under '{}', which is",
            report.paths[id],
            report.paths[&node.parent]
        );
    }
    let repository = context.handle.repository();
    state
        .node_free_pending(&repository)
        .await
        .expect("the drain");
    let drained = invariants::check(&state, &repository, Rules::CLOSED)
        .await
        .unwrap_or_else(|violations| panic!("invariants after the drain: {violations:#?}"));
    assert_eq!(drained.detached, 0, "the drain frees every pending node");
    report
}

/// The nodes of the added `n` the setup made, all pending once a delete takes it.
const ADDED_COUNT: usize = 4;

/// Whether operation `op`, an add, landed: it reports its node exactly when it succeeds, and a
/// node it published into a subtree a delete then took is pending, Discarded but not freed.
async fn add_landed(context: &Context, op: usize) -> bool {
    let node = capi::added(&context.events[op]);
    assert_eq!(
        node.is_some(),
        context.status(op) == 0,
        "the add reports its node exactly when it succeeds: {:?}",
        context.events[op].all()
    );
    if let Some(node) = node {
        let record = context
            .handle
            .state()
            .node(context.handle.repository(), node)
            .await
            .expect("the added node reads");
        assert!(
            record.is_discarded(),
            "the added node went with the subtree"
        );
    }
    node.is_some()
}

/// The node count operation `op`, a delete of one entry, reported for that entry where it
/// succeeded.
fn deleted_count(context: &Context, op: usize) -> Option<usize> {
    context.events[op]
        .all()
        .iter()
        .find_map(|event| match event {
            Reported::Delete(_, count, LoreErrorCode::None) => {
                Some(usize::try_from(*count).expect("a count fits"))
            }
            _ => None,
        })
}

/// That operation `op`, a delete of one entry, failed that entry as `INTERNAL`, counting nothing.
fn assert_entry_failed(context: &Context, op: usize) {
    let reported = context.events[op].all();
    assert!(
        reported
            .iter()
            .any(|event| matches!(event, Reported::Delete(_, 0, LoreErrorCode::Internal))),
        "the delete's entry fails: {reported:?}"
    );
}

/// How often a scenario's runs ended each way, so a test can assert that both did: an outcome
/// no run reached is an interleaving the exploration missed.
#[derive(Default)]
struct Outcomes {
    yes: AtomicUsize,
    no: AtomicUsize,
}

impl Outcomes {
    fn saw(&self, yes: bool) {
        let count = if yes { &self.yes } else { &self.no };
        count.fetch_add(1, Ordering::Relaxed);
    }

    fn assert_both(&self, what: &str) {
        let (yes, no) = (
            self.yes.load(Ordering::Relaxed),
            self.no.load(Ordering::Relaxed),
        );
        assert!(yes > 0 && no > 0, "{what}: {yes} runs did, {no} did not");
    }
}

/// Explores `scenario` to the end, every interleaving run: the handle's claims are not recorded
/// for the scheduler, and a step that ends by waking another task carries that task's work in
/// its footprint, which sleep sets would trust as the step's own.
fn explore(scenario: Scenario<Context>, exploration: Exploration) {
    crate::explored(
        scenario,
        Modelling::default(),
        Exploration {
            sleep_sets: false,
            ..exploration
        },
    );
}

/// A delete of the added `n` and an add into `n/m`: the delete succeeds, and the add either
/// lands before the delete reaches `n/m`, which then takes it, or is refused. Covers an add into
/// a descendant of an added root between the delete's classification and its marks.
#[test]
fn a_delete_and_an_add_into_its_added_subtree() {
    let outcomes = Arc::new(Outcomes::default());
    let seen = outcomes.clone();
    let scenario = Scenario::<Context> {
        name: "h18: a delete and an add into its added subtree",
        setup: Box::new(|| Box::pin(setup(2, &[]))),
        ops: vec![delete("n", 0), add("n/m", "x", 1)],
        cancellable: vec![],
        check: Box::new(move |context: Arc<Context>| {
            let seen = seen.clone();
            Box::pin(async move {
                let report = checked(&context).await;
                assert_eq!(context.status(0), 0, "the delete succeeds");
                assert!(report.at("n").is_none(), "'n' is gone");
                let landed = add_landed(&context, 1).await;
                assert_eq!(
                    report.detached,
                    ADDED_COUNT + usize::from(landed),
                    "the subtree is pending whole, the add's node with it where it landed"
                );
                assert_eq!(
                    deleted_count(&context, 0),
                    Some(ADDED_COUNT + usize::from(landed)),
                    "the delete counts the subtree, the add's node with it where it landed"
                );
                seen.saw(landed);
            })
        }),
    };
    explore(scenario, Exploration::default());
    outcomes.assert_both("the add landed");
}

/// A delete of the tracked `t` and an add into `t/sub`: the delete succeeds, and the add either
/// lands before the delete marks `t/sub`, which then detaches it as an added child, or is
/// refused.
#[test]
fn a_delete_and_an_add_into_its_tracked_subtree() {
    let outcomes = Arc::new(Outcomes::default());
    let seen = outcomes.clone();
    let scenario = Scenario::<Context> {
        name: "h18: a delete and an add into its tracked subtree",
        setup: Box::new(|| Box::pin(setup(2, &[]))),
        ops: vec![delete("t", 0), add("t/sub", "x", 1)],
        cancellable: vec![],
        check: Box::new(move |context: Arc<Context>| {
            let seen = seen.clone();
            Box::pin(async move {
                let report = checked(&context).await;
                assert_eq!(context.status(0), 0, "the delete succeeds");
                let (_, t) = report.at("t").expect("'t' stays listed");
                assert!(t.is_staged_delete(), "'t' is staged for deletion");
                let landed = add_landed(&context, 1).await;
                assert!(report.at("t/sub/x").is_none(), "'x' is not listed");
                assert_eq!(
                    report.detached,
                    usize::from(landed),
                    "an added 'x' is pending, detached as an added child of a deleted node"
                );
                assert_eq!(
                    deleted_count(&context, 0),
                    Some(4 + usize::from(landed)),
                    "the delete counts 't' with the three below it, and an added 'x'"
                );
                seen.saw(landed);
            })
        }),
    };
    explore(scenario, Exploration::default());
    outcomes.assert_both("the add landed");
}

/// A delete of the added `n` and a move of `n/m` out to `q`: one runs after the other. The move
/// first keeps `q/m` and `q/m/e` added; the delete first refuses the move.
#[test]
fn a_delete_and_a_move_out_of_its_added_subtree() {
    let outcomes = Arc::new(Outcomes::default());
    let seen = outcomes.clone();
    let scenario = Scenario::<Context> {
        name: "h18: a delete and a move out of its added subtree",
        setup: Box::new(|| Box::pin(setup(2, &[]))),
        ops: vec![delete("n", 0), move_to("n/m", "q", "m", 1)],
        cancellable: vec![],
        check: Box::new(move |context: Arc<Context>| {
            let seen = seen.clone();
            Box::pin(async move {
                let report = checked(&context).await;
                assert_eq!(context.status(0), 0, "the delete succeeds");
                assert!(report.at("n").is_none(), "'n' is gone");
                let moved = context.status(1) == 0;
                for path in ["q/m", "q/m/e"] {
                    assert_eq!(
                        report.at(path).map(|(_, node)| node.is_staged_add()),
                        moved.then_some(true),
                        "'{path}' is added where the move landed, and only there"
                    );
                }
                assert_eq!(
                    deleted_count(&context, 0),
                    Some(if moved { 2 } else { ADDED_COUNT }),
                    "the delete counts 'n' and 'n/c', and the moved 'n/m' only where it stayed"
                );
                seen.saw(moved);
            })
        }),
    };
    explore(scenario, Exploration::default());
    outcomes.assert_both("the move landed");
}

/// A delete of the tracked `t` and a move of `t/sub` out to `q`: one runs after the other. The
/// move first leaves `q/sub` and `q/sub/b` not staged for deletion; the delete first refuses the
/// move.
#[test]
fn a_delete_and_a_move_out_of_its_tracked_subtree() {
    let outcomes = Arc::new(Outcomes::default());
    let seen = outcomes.clone();
    let scenario = Scenario::<Context> {
        name: "h18: a delete and a move out of its tracked subtree",
        setup: Box::new(|| Box::pin(setup(2, &[]))),
        ops: vec![delete("t", 0), move_to("t/sub", "q", "sub", 1)],
        cancellable: vec![],
        check: Box::new(move |context: Arc<Context>| {
            let seen = seen.clone();
            Box::pin(async move {
                let report = checked(&context).await;
                assert_eq!(context.status(0), 0, "the delete succeeds");
                if context.status(1) == 0 {
                    for path in ["q/sub", "q/sub/b"] {
                        let (_, node) = report
                            .at(path)
                            .unwrap_or_else(|| panic!("'{path}' is where the move landed"));
                        assert!(
                            !node.is_staged_delete(),
                            "'{path}', moved out before the delete, is not staged for deletion"
                        );
                    }
                }
                assert_eq!(
                    deleted_count(&context, 0),
                    Some(if context.status(1) == 0 { 2 } else { 4 }),
                    "the delete counts 't' and 't/a', and the moved 't/sub' only where it stayed"
                );
                seen.saw(context.status(1) == 0);
            })
        }),
    };
    explore(scenario, Exploration::default());
    outcomes.assert_both("the move landed");
}

/// A delete of the added `n` and a move of the tracked `q/y` into `n/m`: the move first makes
/// `n` a subtree mixing added and tracked nodes, whose delete fails with nothing detached; the
/// delete first refuses the move.
#[test]
fn a_delete_and_a_move_into_its_added_subtree() {
    let outcomes = Arc::new(Outcomes::default());
    let seen = outcomes.clone();
    let scenario = Scenario::<Context> {
        name: "h18: a delete and a move into its added subtree",
        setup: Box::new(|| Box::pin(setup(2, &[]))),
        ops: vec![delete("n", 0), move_to("q/y", "n/m", "y", 1)],
        cancellable: vec![],
        check: Box::new(move |context: Arc<Context>| {
            let seen = seen.clone();
            Box::pin(async move {
                let report = checked(&context).await;
                if context.status(0) == 0 {
                    assert!(report.at("n").is_none(), "'n' is gone");
                    assert_ne!(context.status(1), 0, "the move was refused");
                    assert!(report.at("q/y").is_some(), "'q/y' stays");
                    assert_eq!(deleted_count(&context, 0), Some(ADDED_COUNT));
                } else {
                    assert_eq!(context.status(1), 0, "the move came first");
                    assert_entry_failed(&context, 0);
                    for path in ["n", "n/m", "n/m/y"] {
                        assert!(report.at(path).is_some(), "'{path}' is not detached");
                    }
                }
                seen.saw(context.status(0) == 0);
            })
        }),
    };
    explore(scenario, Exploration::default());
    outcomes.assert_both("the delete succeeded");
}

/// A delete of the tracked `t` and a move of `q/y` into `t/sub`: the move first lands `y` where
/// the delete then stages it; the delete first refuses the move.
#[test]
fn a_delete_and_a_move_into_its_tracked_subtree() {
    let outcomes = Arc::new(Outcomes::default());
    let seen = outcomes.clone();
    let scenario = Scenario::<Context> {
        name: "h18: a delete and a move into its tracked subtree",
        setup: Box::new(|| Box::pin(setup(2, &[]))),
        ops: vec![delete("t", 0), move_to("q/y", "t/sub", "y", 1)],
        cancellable: vec![],
        check: Box::new(move |context: Arc<Context>| {
            let seen = seen.clone();
            Box::pin(async move {
                let report = checked(&context).await;
                assert_eq!(context.status(0), 0, "the delete succeeds");
                let landed = report.at("t/sub/y").is_some();
                assert_eq!(
                    landed,
                    context.status(1) == 0,
                    "'y' lands iff the move succeeds"
                );
                assert_eq!(
                    deleted_count(&context, 0),
                    Some(4 + usize::from(landed)),
                    "the delete counts 't' with the three below it, and a moved 'y'"
                );
                seen.saw(landed);
            })
        }),
    };
    explore(scenario, Exploration::default());
    outcomes.assert_both("the move landed");
}

/// A delete of the tracked `t` and a modify of `t/a`: the modify first is staged over by the
/// delete; the delete first refuses the modify. A modification validated before the delete's
/// mark never drops it.
#[test]
fn a_delete_and_a_modify_in_its_tracked_subtree() {
    let outcomes = Arc::new(Outcomes::default());
    let seen = outcomes.clone();
    let scenario = Scenario::<Context> {
        name: "h18: a delete and a modify in its tracked subtree",
        setup: Box::new(|| Box::pin(setup(2, &[]))),
        ops: vec![delete("t", 0), modify("t/a", 1)],
        cancellable: vec![],
        check: Box::new(move |context: Arc<Context>| {
            let seen = seen.clone();
            Box::pin(async move {
                checked(&context).await;
                assert_eq!(context.status(0), 0, "the delete succeeds");
                assert_eq!(deleted_count(&context, 0), Some(4));
                seen.saw(context.status(1) == 0);
            })
        }),
    };
    explore(scenario, Exploration::default());
    outcomes.assert_both("the modify landed");
}

/// A delete of `t/sub` and a restore of its deleted `b` through an add: the restore first is
/// staged over by the delete; the delete first refuses the restore.
#[test]
fn a_delete_and_a_restore_below_it() {
    let outcomes = Arc::new(Outcomes::default());
    let seen = outcomes.clone();
    let scenario = Scenario::<Context> {
        name: "h18: a delete and a restore below it",
        setup: Box::new(|| Box::pin(setup(2, &["t/sub/b"]))),
        ops: vec![delete("t/sub", 0), add("t/sub", "b", 1)],
        cancellable: vec![],
        check: Box::new(move |context: Arc<Context>| {
            let seen = seen.clone();
            Box::pin(async move {
                checked(&context).await;
                assert_eq!(context.status(0), 0, "the delete succeeds");
                assert_eq!(
                    deleted_count(&context, 0),
                    Some(1 + usize::from(context.status(1) == 0)),
                    "the delete counts 't/sub', and 'b' where its restore came first"
                );
                seen.saw(context.status(1) == 0);
            })
        }),
    };
    explore(scenario, Exploration::default());
    outcomes.assert_both("the restore landed");
}

/// A delete of the added `n` and an add of `n/x`: the add's node, published staged for addition,
/// is an added node from its publish, so the delete succeeds and takes it, and an add into an
/// added directory climbs no further. The add succeeds unless the delete took `n` before the add
/// published.
#[test]
fn a_delete_and_an_add_into_its_added_root() {
    let outcomes = Arc::new(Outcomes::default());
    let seen = outcomes.clone();
    let scenario = Scenario::<Context> {
        name: "h18: a delete and an add into its added root",
        setup: Box::new(|| Box::pin(setup(2, &[]))),
        ops: vec![delete("n", 0), add("n", "x", 1)],
        cancellable: vec![],
        check: Box::new(move |context: Arc<Context>| {
            let seen = seen.clone();
            Box::pin(async move {
                let report = checked(&context).await;
                assert_eq!(context.status(0), 0, "the delete succeeds");
                assert!(report.at("n").is_none(), "'n' is gone");
                let landed = add_landed(&context, 1).await;
                assert_eq!(
                    report.detached,
                    ADDED_COUNT + usize::from(landed),
                    "the subtree is pending whole, the add's node with it where it landed"
                );
                assert_eq!(
                    deleted_count(&context, 0),
                    Some(ADDED_COUNT + usize::from(landed)),
                    "the delete counts the subtree, the add's node with it where it landed"
                );
                seen.saw(landed);
            })
        }),
    };
    explore(scenario, Exploration::default());
    outcomes.assert_both("the add landed");
}

/// A delete of the added `n` and a modify of `n/c`: the modify first is taken with the subtree;
/// the delete first refuses the modify. Either way the subtree is pending whole.
#[test]
fn a_delete_and_a_modify_in_its_added_subtree() {
    let outcomes = Arc::new(Outcomes::default());
    let seen = outcomes.clone();
    let scenario = Scenario::<Context> {
        name: "h18: a delete and a modify in its added subtree",
        setup: Box::new(|| Box::pin(setup(2, &[]))),
        ops: vec![delete("n", 0), modify("n/c", 1)],
        cancellable: vec![],
        check: Box::new(move |context: Arc<Context>| {
            let seen = seen.clone();
            Box::pin(async move {
                // Read before the check's drain frees it: the modify reports success exactly
                // when it wrote the record, which the delete then took along.
                let c = context
                    .handle
                    .state()
                    .node(context.handle.repository(), context.id("n/c"))
                    .await
                    .expect("n/c's record reads");
                assert_eq!(
                    context.status(1) == 0,
                    (c.mode, c.size) == (0o600, 20),
                    "the modify succeeds exactly when it wrote n/c"
                );
                let report = checked(&context).await;
                assert_eq!(context.status(0), 0, "the delete succeeds");
                assert!(report.at("n").is_none(), "'n' is gone");
                assert_eq!(report.detached, ADDED_COUNT, "the subtree is pending whole");
                assert_eq!(deleted_count(&context, 0), Some(ADDED_COUNT));
                seen.saw(context.status(1) == 0);
            })
        }),
    };
    explore(scenario, Exploration::default());
    outcomes.assert_both("the modify landed");
}

/// A delete of the added file `n/c` and a modify of it: the delete detaches the root in its
/// unlink's locked section, so the modify first is taken with it, and the delete first refuses the
/// modify. Either way `n/c` is pending, counted once.
#[test]
fn a_delete_and_a_modify_of_its_added_file() {
    let outcomes = Arc::new(Outcomes::default());
    let seen = outcomes.clone();
    let scenario = Scenario::<Context> {
        name: "h18: a delete and a modify of its added file",
        setup: Box::new(|| Box::pin(setup(2, &[]))),
        ops: vec![delete("n/c", 0), modify("n/c", 1)],
        cancellable: vec![],
        check: Box::new(move |context: Arc<Context>| {
            let seen = seen.clone();
            Box::pin(async move {
                // Read before the check's drain frees it.
                let c = context
                    .handle
                    .state()
                    .node(context.handle.repository(), context.id("n/c"))
                    .await
                    .expect("n/c's record reads");
                assert_eq!(
                    context.status(1) == 0,
                    (c.mode, c.size) == (0o600, 20),
                    "the modify succeeds exactly when it wrote n/c"
                );
                let report = checked(&context).await;
                assert_eq!(context.status(0), 0, "the delete succeeds");
                assert!(report.at("n/c").is_none(), "'n/c' is gone");
                assert_eq!(report.detached, 1, "'n/c' is pending");
                assert_eq!(deleted_count(&context, 0), Some(1));
                seen.saw(context.status(1) == 0);
            })
        }),
    };
    explore(scenario, Exploration::default());
    outcomes.assert_both("the modify landed");
}

/// Two deletes, of the tracked `t` and of `t/sub` below it: the moves permit puts one after the
/// other. `t` first refuses the delete of `t/sub`, staged for deletion already; `t/sub` first
/// leaves `t`'s delete to pass it over. Either way `t` is staged for deletion whole.
#[test]
fn two_deletes_of_a_tracked_subtree_and_a_directory_below_it() {
    let outcomes = Arc::new(Outcomes::default());
    let seen = outcomes.clone();
    let scenario = Scenario::<Context> {
        name: "h18: two deletes of a tracked subtree and a directory below it",
        setup: Box::new(|| Box::pin(setup(2, &[]))),
        ops: vec![delete("t", 0), delete("t/sub", 1)],
        cancellable: vec![],
        check: Box::new(move |context: Arc<Context>| {
            let seen = seen.clone();
            Box::pin(async move {
                let report = checked(&context).await;
                assert_eq!(context.status(0), 0, "the delete of 't' succeeds");
                for path in ["t", "t/a", "t/sub", "t/sub/b"] {
                    let (_, node) = report
                        .at(path)
                        .unwrap_or_else(|| panic!("'{path}' stays listed"));
                    assert!(node.is_staged_delete(), "'{path}' is staged for deletion");
                }
                // 't/sub' first: its delete counted it and 't/sub/b', and 't''s passes it over.
                let counted = if context.status(1) == 0 { 2 } else { 4 };
                assert_eq!(deleted_count(&context, 0), Some(counted));
                assert_eq!(
                    deleted_count(&context, 1),
                    (context.status(1) == 0).then_some(2)
                );
                seen.saw(context.status(1) == 0);
            })
        }),
    };
    explore(scenario, Exploration::default());
    outcomes.assert_both("the delete of 't/sub' succeeded");
}

/// Two deletes, of the added `n` and of `n/m` below it: `n` first refuses the delete of `n/m`,
/// discarded already; `n/m` first detaches it, and `n`'s delete takes the rest. Either way the
/// subtree is pending whole, and nothing is pending twice.
#[test]
fn two_deletes_of_an_added_subtree_and_a_directory_below_it() {
    let outcomes = Arc::new(Outcomes::default());
    let seen = outcomes.clone();
    let scenario = Scenario::<Context> {
        name: "h18: two deletes of an added subtree and a directory below it",
        setup: Box::new(|| Box::pin(setup(2, &[]))),
        ops: vec![delete("n", 0), delete("n/m", 1)],
        cancellable: vec![],
        check: Box::new(move |context: Arc<Context>| {
            let seen = seen.clone();
            Box::pin(async move {
                let report = checked(&context).await;
                assert_eq!(context.status(0), 0, "the delete of 'n' succeeds");
                assert!(report.at("n").is_none(), "'n' is gone");
                assert_eq!(report.detached, ADDED_COUNT, "the subtree is pending whole");
                // 'n/m' first: it went with its own delete, and 'n''s counts 'n' and 'n/c'.
                let counted = if context.status(1) == 0 {
                    2
                } else {
                    ADDED_COUNT
                };
                assert_eq!(deleted_count(&context, 0), Some(counted));
                assert_eq!(
                    deleted_count(&context, 1),
                    (context.status(1) == 0).then_some(2)
                );
                seen.saw(context.status(1) == 0);
            })
        }),
    };
    explore(scenario, Exploration::default());
    outcomes.assert_both("the delete of 'n/m' succeeded");
}

/// The status of a batch delete ([`delete_batch`]) one of whose entries failed.
const BATCH_FAILED: i32 = 1;

/// The status of a batch delete whose plan refused the batch.
const BATCH_REFUSED: i32 = 2;

/// A C API delete of the batch `paths`, as operation `op`, through lore's own plan and apply
/// (`plan_entries`, `apply_plan`), under the moves permit it holds for the whole batch, as the
/// call's task holds it: its levels, and its detaches by directory, spread over the futures the
/// apply spawns. Its status: 0 where every entry went whole, [`BATCH_FAILED`] where one failed,
/// [`BATCH_REFUSED`] where the plan refused the batch.
fn delete_batch(paths: &'static [&'static str], op: usize) -> Op<Context> {
    Box::new(move |context: Arc<Context>| {
        Box::pin(async move {
            let (state, repository) = (context.handle.state(), context.handle.repository());
            let entries: Vec<LoreRevisionTreeDeleteEntry> = (paths.iter().zip(1..))
                .map(|(path, entry_id)| LoreRevisionTreeDeleteEntry {
                    entry_id,
                    node_id: context.id(path),
                })
                .collect();
            let permit = state.moves_permit().await.expect("the moves permit");
            let status = match plan_entries(&state, &repository, &entries).await {
                Ok(planned) => match apply_plan(state.clone(), repository.clone(), planned).await {
                    Ok(()) => 0,
                    Err(_) => BATCH_FAILED,
                },
                Err(_) => BATCH_REFUSED,
            };
            drop(permit);
            context.status[op].store(status, Ordering::SeqCst);
        })
    })
}

/// Asserts each of `paths` is listed in `report` and staged for deletion, or not, as `staged` says.
fn staged(report: &Report, paths: &[&str], staged: bool) {
    for path in paths {
        let (_, node) = report
            .at(path)
            .unwrap_or_else(|| panic!("'{path}' stays listed"));
        assert_eq!(
            node.is_staged_delete(),
            staged,
            "'{path}' staged for deletion"
        );
    }
}

/// A batch delete, through lore's own plan and apply, of the tracked `t/a` and `t/sub`, both in
/// `t`, and of the added `n/c` and `n/m`, both in `n`, beside an add into `t/sub`: every entry
/// goes whole, `t`'s two roots staged in one level spread over the apply's futures, `n`'s two
/// detached from their one directory one after the other. The add lands before `t/sub`'s level,
/// which detaches it as an added child, or is refused after. Within three preemptions a run.
#[test]
fn a_batch_delete_of_roots_sharing_parents_beside_an_add() {
    let outcomes = Arc::new(Outcomes::default());
    let seen = outcomes.clone();
    let scenario = Scenario::<Context> {
        name: "h18: a batch delete of roots sharing parents beside an add",
        setup: Box::new(|| Box::pin(setup(2, &[]))),
        ops: vec![
            delete_batch(&["t/a", "t/sub", "n/c", "n/m"], 0),
            add("t/sub", "x", 1),
        ],
        cancellable: vec![],
        check: Box::new(move |context: Arc<Context>| {
            let seen = seen.clone();
            Box::pin(async move {
                let report = checked(&context).await;
                assert_eq!(context.status(0), 0, "every entry goes whole");
                staged(&report, &["t/a", "t/sub", "t/sub/b"], true);
                staged(&report, &["t", "q", "q/y"], false);
                for path in ["n/c", "n/m", "n/m/e"] {
                    assert!(report.at(path).is_none(), "'{path}' is gone");
                }
                let landed = add_landed(&context, 1).await;
                assert_eq!(
                    report.detached,
                    3 + usize::from(landed),
                    "'n/c', 'n/m' and 'n/m/e' are pending, the add's node with them where it landed"
                );
                seen.saw(landed);
            })
        }),
    };
    explore(
        scenario,
        Exploration {
            preemptions: 3,
            deepening: false,
            ..Exploration::default()
        },
    );
    outcomes.assert_both("the add landed");
}

/// A batch delete naming the tracked `t` and `t/sub` below it, through lore's own plan, beside an
/// add into `t/sub`: the plan refuses the batch, one entry deleting another's ancestor, so nothing
/// is staged or detached, and the add lands whenever it runs.
#[test]
fn a_batch_delete_of_nested_entries_is_refused_whole() {
    let scenario = Scenario::<Context> {
        name: "h18: a batch delete of nested entries",
        setup: Box::new(|| Box::pin(setup(2, &[]))),
        ops: vec![delete_batch(&["t", "t/sub"], 0), add("t/sub", "x", 1)],
        cancellable: vec![],
        check: Box::new(|context: Arc<Context>| {
            Box::pin(async move {
                let report = checked(&context).await;
                assert_eq!(
                    context.status(0),
                    BATCH_REFUSED,
                    "the plan refuses the batch"
                );
                staged(&report, &["t", "t/a", "t/sub", "t/sub/b"], false);
                assert_eq!(context.status(1), 0, "the add lands");
                assert!(report.at("t/sub/x").is_some(), "'t/sub/x' is listed");
                assert_eq!(report.detached, 0, "nothing is pending");
            })
        }),
    };
    explore(scenario, Exploration::default());
}

/// A batch delete, through lore's own plan and apply, of the tracked `t/a` and `t/sub` and the
/// added `n/m`, beside a move of the tracked `q/y` into `n/m`: the moves permit puts one after
/// the other. The move first makes `n/m` a subtree mixing added and tracked nodes, whose entry
/// fails with nothing of it detached, the others going whole; the batch first takes `n/m` along,
/// and the move is refused. Every schedule runs.
#[test]
fn a_batch_delete_beside_a_move_into_one_of_its_added_roots() {
    let outcomes = Arc::new(Outcomes::default());
    let seen = outcomes.clone();
    let scenario = Scenario::<Context> {
        name: "h18: a batch delete beside a move into one of its added roots",
        setup: Box::new(|| Box::pin(setup(2, &[]))),
        ops: vec![
            delete_batch(&["t/a", "t/sub", "n/m"], 0),
            move_to("q/y", "n/m", "y", 1),
        ],
        cancellable: vec![],
        check: Box::new(move |context: Arc<Context>| {
            let seen = seen.clone();
            Box::pin(async move {
                let report = checked(&context).await;
                staged(&report, &["t/a", "t/sub", "t/sub/b"], true);
                let moved = context.status(1) == 0;
                if moved {
                    assert_eq!(context.status(0), BATCH_FAILED, "the mixed entry fails");
                    assert!(report.at("n/m/y").is_some(), "'y' stands in 'n/m'");
                    assert_eq!(report.detached, 0, "nothing of 'n/m' is pending");
                } else {
                    assert_eq!(context.status(0), 0, "every entry goes whole");
                    assert!(report.at("n/m").is_none(), "'n/m' is gone");
                    assert_eq!(report.detached, 2, "'n/m' and 'n/m/e' are pending");
                    staged(&report, &["q/y"], false);
                }
                seen.saw(moved);
            })
        }),
    };
    explore(scenario, Exploration::default());
    outcomes.assert_both("the move went first");
}

/// A delete of the added `n` and a restore, through an add, of the tracked `y` that a move put
/// in `n/m` and a delete staged there: exactly one goes through. The restore first makes `n`'s
/// subtree mixed, so the delete fails with nothing detached; the delete first takes `y` along,
/// and the restore is refused. The restore takes the moves permit, which the delete holds from
/// its check of `n`'s subtree to its detach.
#[test]
fn a_delete_and_a_restore_below_its_added_subtree() {
    let outcomes = Arc::new(Outcomes::default());
    let seen = outcomes.clone();
    let restore: Op<Context> = Box::new(|context: Arc<Context>| {
        Box::pin(async move {
            let status = capi::add(
                context.handle.tree,
                context.id("n/m"),
                "y",
                LoreNodeType::File,
                &context.events[1],
            )
            .await;
            context.status[1].store(status, Ordering::SeqCst);
        })
    });
    let scenario = Scenario::<Context> {
        name: "h18: a delete and a restore below its added subtree",
        setup: Box::new(|| Box::pin(setup_moved(2))),
        ops: vec![delete("n", 0), restore],
        cancellable: vec![],
        check: Box::new(move |context: Arc<Context>| {
            let seen = seen.clone();
            Box::pin(async move {
                let report = checked(&context).await;
                let (deleted, restored) = (context.status(0) == 0, context.status(1) == 0);
                assert!(deleted != restored, "exactly one goes through");
                if restored {
                    assert_entry_failed(&context, 0);
                    let (_, y) = report.at("n/m/y").expect("'y' is restored in 'n/m'");
                    assert!(
                        !y.is_staged_delete() && !y.is_staged_add(),
                        "'y' is tracked"
                    );
                } else {
                    assert!(report.at("n").is_none(), "'n' is gone");
                    assert_eq!(report.detached, ADDED_COUNT + 1, "'y' went with it");
                    assert_eq!(
                        deleted_count(&context, 0),
                        Some(ADDED_COUNT),
                        "'y', staged for deletion, was counted by its own delete"
                    );
                }
                seen.saw(deleted);
            })
        }),
    };
    explore(scenario, Exploration::default());
    outcomes.assert_both("the delete went through");
}

/// A listing of `n/m` beside a delete of the added `n`: it lists `n/m`'s child before the delete
/// reaches it and nothing after, never a node the delete took.
#[test]
fn a_delete_and_a_listing_in_its_added_subtree() {
    let outcomes = Arc::new(Outcomes::default());
    let seen = outcomes.clone();
    let list: Op<Context> = Box::new(|context: Arc<Context>| {
        Box::pin(async move {
            let status =
                capi::list_children(context.handle.tree, context.id("n/m"), &context.events[1])
                    .await;
            context.status[1].store(status, Ordering::SeqCst);
        })
    });
    let scenario = Scenario::<Context> {
        name: "h18: a delete and a listing in its added subtree",
        setup: Box::new(|| Box::pin(setup(2, &[]))),
        ops: vec![delete("n", 0), list],
        cancellable: vec![],
        check: Box::new(move |context: Arc<Context>| {
            let seen = seen.clone();
            Box::pin(async move {
                checked(&context).await;
                assert_eq!(context.status(0), 0, "the delete succeeds");
                assert_eq!(deleted_count(&context, 0), Some(ADDED_COUNT));
                assert_eq!(context.status(1), 0, "the listing succeeds");
                let listed: Vec<NodeID> = context.events[1]
                    .all()
                    .iter()
                    .filter_map(|event| match event {
                        Reported::Child(node, _, _) => Some(*node),
                        _ => None,
                    })
                    .collect();
                assert!(
                    listed.is_empty() || listed == vec![context.id("n/m/e")],
                    "the listing shows 'n/m' whole or empty: {listed:?}"
                );
                seen.saw(listed.is_empty());
            })
        }),
    };
    explore(scenario, Exploration::default());
    outcomes.assert_both("the listing came after");
}

/// The info of the added `n/c` beside a delete of `n`: a file staged for addition before the
/// delete reaches it, a freed id after, never an error.
#[test]
fn a_delete_and_an_info_in_its_added_subtree() {
    let outcomes = Arc::new(Outcomes::default());
    let seen = outcomes.clone();
    let info: Op<Context> = Box::new(|context: Arc<Context>| {
        Box::pin(async move {
            let status =
                capi::node_info(context.handle.tree, context.id("n/c"), &context.events[1]).await;
            context.status[1].store(status, Ordering::SeqCst);
        })
    });
    let scenario = Scenario::<Context> {
        name: "h18: a delete and an info in its added subtree",
        setup: Box::new(|| Box::pin(setup(2, &[]))),
        ops: vec![delete("n", 0), info],
        cancellable: vec![],
        check: Box::new(move |context: Arc<Context>| {
            let seen = seen.clone();
            Box::pin(async move {
                checked(&context).await;
                assert_eq!(context.status(0), 0, "the delete succeeds");
                assert_eq!(deleted_count(&context, 0), Some(ADDED_COUNT));
                assert_eq!(context.status(1), 0, "the info succeeds");
                let info = context.events[1]
                    .all()
                    .iter()
                    .find_map(|event| match event {
                        Reported::NodeInfo(_, kind, action) => Some((*kind, *action)),
                        _ => None,
                    })
                    .expect("the info reports the node");
                let file = (
                    LoreNodeType::File as u32,
                    lore_revision::interface::LoreNodeStagedAction::Add as u32,
                );
                let freed = (
                    LoreNodeType::Directory as u32,
                    lore_revision::interface::LoreNodeStagedAction::None as u32,
                );
                assert!(
                    info == file || info == freed,
                    "a file added or a freed id: {info:?}"
                );
                seen.saw(info == freed);
            })
        }),
    };
    explore(scenario, Exploration::default());
    outcomes.assert_both("the info came after");
}

/// A commit alone, recorded: each record of its State names a task, its spawned tasks the
/// scheduler's, as `subtask!` makes them, so a recording run can tell whose each write is. The
/// claim holds in every schedule, so one with no preemption shows it.
#[test]
fn a_commit_alone_names_a_task_in_each_record() {
    let commit: Op<Context> = Box::new(|context: Arc<Context>| {
        Box::pin(async move {
            let status = capi::commit(context.handle.tree, &context.events[0]).await;
            context.status[0].store(status, Ordering::SeqCst);
        })
    });
    let scenario = Scenario::<Context> {
        name: "h18: a commit alone, recorded",
        setup: Box::new(|| {
            Box::pin(async {
                let context = setup(1, &[]).await;
                capi::set_branch(context.handle.tree).await;
                let events = Events::default();
                let status = capi::modify(context.handle.tree, context.id("q/y"), &events).await;
                assert_eq!(status, 0, "the modify succeeds: {:?}", events.all());
                context
            })
        }),
        ops: vec![commit],
        cancellable: vec![],
        check: Box::new(|context: Arc<Context>| {
            Box::pin(async move {
                assert_eq!(
                    context.status(0),
                    0,
                    "the commit succeeds: {:?}",
                    context.events[0].all()
                );
                let log = scheduler::run_log();
                assert!(
                    log.iter()
                        .any(|record| record.task.is_some_and(|task| task != 0)),
                    "a task the commit spawned wrote: {log:#?}"
                );
            })
        }),
    };
    let covered = scenario.explore(Exploration {
        recording: true,
        sleep_sets: false,
        preemptions: 0,
        deepening: false,
        ..Exploration::default()
    });
    assert!(
        covered.complete || (covered.bound == Some(0) && !covered.timed_out),
        "explored {} runs without finishing",
        covered.runs
    );
}

/// A commit beside a delete of the added `n`, a modify of the tracked `q/y` staged so the commit
/// has something to commit whatever the order: the commit's exclusive claim puts it wholly
/// before the delete, which then stages the committed `n` for deletion, or wholly after it, its
/// drain freeing what the delete left.
#[test]
fn a_delete_and_a_commit() {
    let outcomes = Arc::new(Outcomes::default());
    let seen = outcomes.clone();
    let commit: Op<Context> = Box::new(|context: Arc<Context>| {
        Box::pin(async move {
            let status = capi::commit(context.handle.tree, &context.events[1]).await;
            context.status[1].store(status, Ordering::SeqCst);
        })
    });
    let scenario = Scenario::<Context> {
        name: "h18: a delete and a commit",
        setup: Box::new(|| {
            Box::pin(async {
                let context = setup(2, &[]).await;
                capi::set_branch(context.handle.tree).await;
                let events = Events::default();
                let status = capi::modify(context.handle.tree, context.id("q/y"), &events).await;
                assert_eq!(status, 0, "the modify succeeds: {:?}", events.all());
                context
            })
        }),
        ops: vec![delete("n", 0), commit],
        cancellable: vec![],
        check: Box::new(move |context: Arc<Context>| {
            let seen = seen.clone();
            Box::pin(async move {
                assert_eq!(
                    context.status(1),
                    0,
                    "the commit succeeds: {:?}",
                    context.events[1].all()
                );
                assert_eq!(context.status(0), 0, "the delete succeeds");
                let report = checked(&context).await;
                let committed_first = report.at("n").is_some();
                if committed_first {
                    let (_, n) = report.at("n").expect("'n'");
                    assert!(
                        n.is_staged_delete(),
                        "the committed 'n' is staged for deletion"
                    );
                    assert_eq!(report.detached, 0, "nothing is pending");
                } else {
                    assert_eq!(
                        report.detached, 0,
                        "the commit's drain freed what the delete left"
                    );
                }
                assert_eq!(
                    deleted_count(&context, 0),
                    Some(ADDED_COUNT),
                    "the delete counts 'n' and the three below it, committed or added"
                );
                seen.saw(committed_first);
            })
        }),
    };
    // The model judges no commit's writes, so the scenario runs unrecorded, held to its own check.
    let covered = scenario.explore(Exploration {
        sleep_sets: false,
        ..Exploration::default()
    });
    eprintln!("'{}': {covered:?}", scenario.name);
    assert!(
        covered.complete,
        "explored {} runs without finishing",
        covered.runs
    );
    outcomes.assert_both("the commit came first");
}

/// A delete of the tracked `t` and a move of the added `n/c` into `t/sub`: the move first puts an
/// added file below `t`, which the delete then detaches as an added child; the delete first
/// refuses the move.
#[test]
fn a_delete_and_a_move_of_an_added_node_into_its_tracked_subtree() {
    let outcomes = Arc::new(Outcomes::default());
    let seen = outcomes.clone();
    let scenario = Scenario::<Context> {
        name: "h18: a delete and a move of an added node into its tracked subtree",
        setup: Box::new(|| Box::pin(setup(2, &[]))),
        ops: vec![delete("t", 0), move_to("n/c", "t/sub", "c", 1)],
        cancellable: vec![],
        check: Box::new(move |context: Arc<Context>| {
            let seen = seen.clone();
            Box::pin(async move {
                let report = checked(&context).await;
                assert_eq!(context.status(0), 0, "the delete succeeds");
                let moved = context.status(1) == 0;
                assert!(
                    report.at("t/sub/c").is_none(),
                    "'c' is not listed below 't'"
                );
                assert_eq!(
                    report.at("n/c").is_some(),
                    !moved,
                    "'c' stays in 'n' unless the move went through"
                );
                assert_eq!(
                    report.detached,
                    usize::from(moved),
                    "a moved 'c' is pending, detached as an added child of a deleted node"
                );
                assert_eq!(
                    deleted_count(&context, 0),
                    Some(4 + usize::from(moved)),
                    "the delete counts 't' with the three below it, and a moved 'c'"
                );
                seen.saw(moved);
            })
        }),
    };
    explore(scenario, Exploration::default());
    outcomes.assert_both("the move went through");
}

/// A delete of the added `n`, dropped at each point it parks at, beside an add into `n/m`: the
/// delete goes whole whether or not its call is dropped, and the add either lands before the
/// delete reaches `n/m`, which then takes it, or is refused.
#[test]
fn a_dropped_delete_and_an_add_into_its_added_subtree() {
    let outcomes = Arc::new(Outcomes::default());
    let seen = outcomes.clone();
    let scenario = Scenario::<Context> {
        name: "h18: a dropped delete and an add into its added subtree",
        setup: Box::new(|| Box::pin(setup(2, &[]))),
        ops: vec![delete("n", 0), add("n/m", "x", 1)],
        cancellable: vec![0],
        check: Box::new(move |context: Arc<Context>| {
            let seen = seen.clone();
            Box::pin(async move {
                let landed = add_landed(&context, 1).await;
                let report = checked(&context).await;
                assert!(report.at("n").is_none(), "'n' is gone");
                assert_eq!(
                    report.detached,
                    ADDED_COUNT + usize::from(landed),
                    "the subtree is pending whole, the add's node with it where it landed"
                );
                if context.status(0) == 0 {
                    assert_eq!(
                        deleted_count(&context, 0),
                        Some(ADDED_COUNT + usize::from(landed)),
                        "a delete not dropped counts the subtree, the add's node with it"
                    );
                }
                seen.saw(context.status(0) == PENDING);
            })
        }),
    };
    explore(
        scenario,
        Exploration {
            cancellation: true,
            ..Exploration::default()
        },
    );
    outcomes.assert_both("the delete's call was dropped");
}

/// A delete call of `path`, the scenario `name`, dropped at each point it parks at: the subtree is
/// deleted whole, and the call reports nothing once dropped.
fn a_dropped_delete(name: &'static str, path: &'static str, deleted: fn(&Report, &Context)) {
    let outcomes = Arc::new(Outcomes::default());
    let seen = outcomes.clone();
    let scenario = Scenario::<Context> {
        name,
        setup: Box::new(|| Box::pin(setup(1, &[]))),
        ops: vec![delete(path, 0)],
        cancellable: vec![0],
        check: Box::new(move |context: Arc<Context>| {
            let seen = seen.clone();
            Box::pin(async move {
                let report = checked(&context).await;
                deleted(&report, &context);
                seen.saw(context.status(0) == PENDING);
                if context.status(0) == PENDING {
                    let reported = context.events[0].all();
                    assert!(
                        reported.iter().all(|event| !matches!(
                            event,
                            Reported::Delete(..) | Reported::Batch(..)
                        )),
                        "a dropped delete reports nothing: {reported:?}"
                    );
                }
            })
        }),
    };
    explore(
        scenario,
        Exploration {
            cancellation: true,
            ..Exploration::default()
        },
    );
    outcomes.assert_both("the call was dropped");
}

/// A delete of the added `n` dropped at each point: `n` goes whole.
#[test]
fn a_dropped_delete_of_an_added_subtree_takes_it_whole() {
    a_dropped_delete(
        "h18: a dropped delete of an added subtree",
        "n",
        |report, context| {
            for path in ["n", "n/c", "n/m", "n/m/e"] {
                assert!(report.at(path).is_none(), "'{path}' is gone");
            }
            assert_eq!(report.detached, ADDED_COUNT, "the subtree is pending whole");
            if context.status(0) == 0 {
                assert_eq!(deleted_count(context, 0), Some(ADDED_COUNT));
            }
        },
    );
}

/// A delete of the tracked `t` dropped at each point: `t` is staged for deletion whole.
#[test]
fn a_dropped_delete_of_a_tracked_subtree_stages_it_whole() {
    a_dropped_delete(
        "h18: a dropped delete of a tracked subtree",
        "t",
        |report, context| {
            for path in ["t", "t/a", "t/sub", "t/sub/b"] {
                let (_, node) = report
                    .at(path)
                    .unwrap_or_else(|| panic!("'{path}' stays listed"));
                assert!(node.is_staged_delete(), "'{path}' is staged for deletion");
            }
            if context.status(0) == 0 {
                assert_eq!(deleted_count(context, 0), Some(4));
            }
        },
    );
}
