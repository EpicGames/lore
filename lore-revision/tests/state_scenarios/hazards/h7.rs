// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! H7: a bottom-up mark stops at the first ancestor already marked, which may belong to a mark
//! not yet above it, so a swap's clean detach of a higher ancestor takes a subtree holding a
//! node just created.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore_revision::node::NodeID;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;

use crate::harness::catalogue::modelled::Modelling;
use crate::harness::catalogue::ops;
use crate::harness::fixtures::Tree;
use crate::harness::invariants::Rules;
use crate::harness::model::Modelled;
use crate::harness::scheduler::Exploration;
use crate::harness::scheduler::Scenario;

struct Context {
    tree: Tree,
    created: Mutex<Option<NodeID>>,
}

impl Modelled for Context {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        self.tree.states()
    }
}

/// A directory mark and a create below a clean directory, and a clean detach of that
/// directory's parent: a create that answered attached is still in the tree.
#[test]
fn two_marks_and_a_clean_detach_keep_a_created_node() {
    explore("h7: two markers and a clean detach", vec![]);
}

/// [`two_marks_and_a_clean_detach_keep_a_created_node`], the create also dropped at every
/// point it parks at: a create cancelled midway leaves no dirty node under a clean one for the
/// detach to take. A mark cancelled midway is explored in the served scenarios
/// (`a_cancelled_climbing_mark_and_a_clean_detach_of_an_ancestor`).
#[test]
fn a_mark_a_create_cancelled_midway_and_a_clean_detach_keep_a_created_node() {
    explore(
        "h7: a marker, a cancelled create and a clean detach",
        vec![1],
    );
}

/// Explores the scenario to completion, asserting some run's create attached, so the check on
/// the created node ran.
fn explore(name: &'static str, cancellable: Vec<usize>) {
    let created = Arc::new(AtomicUsize::new(0));
    let cancellation = !cancellable.is_empty();
    let covered = crate::explored(
        scenario(name, cancellable, created.clone()),
        Modelling::default(),
        Exploration {
            cancellation,
            ..Exploration::default()
        },
    );
    assert!(
        created.load(Ordering::Relaxed) > 0,
        "no run's create attached in {} runs",
        covered.runs
    );
}

/// The scenario's ops: a mark of `p/d`, a create in it, and a clean detach of `p`, dropping the
/// ops `cancellable` names where exploration cancels; `created` counts the runs whose create
/// attached.
fn scenario(
    name: &'static str,
    cancellable: Vec<usize>,
    created: Arc<AtomicUsize>,
) -> Scenario<Context> {
    Scenario::<Context> {
        name,
        setup: Box::new(|| {
            Box::pin(async {
                let tree = Tree::new(&["p/", "p/d/"]).await;
                tree.state.mark_served();
                Context {
                    tree,
                    created: Mutex::new(None),
                }
            })
        }),
        ops: vec![
            Box::new(|c: Arc<Context>| {
                Box::pin(async move {
                    let d = c.tree.id("p/d");
                    ops::served_mark_dir(&c.tree, d).await;
                })
            }),
            Box::new(|c: Arc<Context>| {
                Box::pin(async move {
                    let d = c.tree.id("p/d");
                    let created = ops::served_create(&c.tree, d, "y").await;
                    *c.created.lock().expect("the record") = created;
                })
            }),
            Box::new(|c: Arc<Context>| {
                Box::pin(async move {
                    let p = c.tree.id("p");
                    ops::swap_detach_if_clean(&c.tree, p).await;
                })
            }),
        ],
        cancellable,
        check: Box::new(move |c: Arc<Context>| {
            let created_runs = created.clone();
            Box::pin(async move {
                let report = crate::holding(
                    &c.tree.state,
                    &c.tree.repository,
                    Rules {
                        dirty_closure: true,
                        unique_names: true,
                        no_ext_flags: false,
                        no_builds: false,
                    },
                )
                .await;
                if let Some(created) = *c.created.lock().expect("the record") {
                    created_runs.fetch_add(1, Ordering::Relaxed);
                    assert!(
                        report.nodes.contains_key(&created),
                        "the created node {created} is no longer in the tree"
                    );
                }
            })
        }),
    }
}
