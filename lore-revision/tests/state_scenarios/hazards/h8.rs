// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! H8, the attach half: an attach dropped between its slot's grab and its publish leaving a typed
//! slot in no chain, its record naming a live directory as its parent. The slot is Discarded from
//! its grab to its publish, so a dropped attach leaves a detached slot, counted as one. Each
//! scenario drops its attach at every point it parks at; the serialize scenario pins that the slot
//! in flight keeps the name it stored through a save's repack.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;

use crate::harness::catalogue::modelled::Modelling;
use crate::harness::catalogue::ops;
use crate::harness::fixtures::Tree;
use crate::harness::invariants::Rules;
use crate::harness::model::Modelled;
use crate::harness::scheduler::Exploration;
use crate::harness::scheduler::Op;
use crate::harness::scheduler::Scenario;

struct Context {
    tree: Tree,
    /// Whether the attach the scenario drops ran to its end.
    done: AtomicBool,
}

impl Modelled for Context {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        self.tree.states()
    }
}

/// An op of `attach`, which records in the context that it ran to its end.
fn attaching(
    attach: impl Fn(Arc<Context>) -> Pin<Box<dyn Future<Output = ()> + Send>> + 'static,
) -> Op<Context> {
    Box::new(move |c: Arc<Context>| {
        let attached = attach(c.clone());
        Box::pin(async move {
            attached.await;
            c.done.store(true, Ordering::Relaxed);
        })
    })
}

/// A served create of `d/y`, as the VFS provider's CREATE makes one, beside a served create of
/// `d/z`, which moves `d`'s chain count, so the first may publish again with its slot held across
/// attempts.
#[test]
fn a_served_create_dropped_at_every_point_leaves_no_slot_outside_every_chain() {
    explore(
        "h8: a served create dropped at every point",
        true,
        false,
        vec![
            attaching(|c| {
                Box::pin(async move {
                    ops::served_create(&c.tree, c.tree.id("d"), "y").await;
                })
            }),
            Box::new(|c: Arc<Context>| {
                Box::pin(async move {
                    ops::served_create(&c.tree, c.tree.id("d"), "z").await;
                })
            }),
        ],
    );
}

/// A VFS provider diff's checked create of `d/y`: its attach, then its directory's mark.
#[test]
fn a_checked_create_dropped_at_every_point_leaves_no_slot_outside_every_chain() {
    explore(
        "h8: a checked create dropped at every point",
        false,
        false,
        vec![attaching(|c| {
            Box::pin(async move {
                let _ = ops::checked_create(&c.tree, c.tree.id("d"), "y").await;
            })
        })],
    );
}

/// An add of `d/y` and its parent's mark, as the scan adds a directory it finds new.
#[test]
fn an_add_dropped_at_every_point_leaves_no_slot_outside_every_chain() {
    explore(
        "h8: an add dropped at every point",
        false,
        false,
        vec![attaching(|c| {
            Box::pin(async move {
                ops::scan_add(&c.tree, c.tree.id("d"), "y").await;
            })
        })],
    );
}

/// A checked create of `d/y` alongside a save of the State, beyond the contract (Serialize runs
/// alone), which repacks the name table of the block the attach writes: the node keeps its name,
/// in flight or published, as the invariants' check of each live node's name against its hash
/// shows.
#[test]
fn an_attach_alongside_a_save_keeps_its_name() {
    explore(
        "h8: an attach alongside a save",
        false,
        true,
        vec![
            attaching(|c| {
                Box::pin(async move {
                    let _ = ops::checked_create(&c.tree, c.tree.id("d"), "y").await;
                })
            }),
            Box::new(|c: Arc<Context>| {
                Box::pin(async move {
                    c.tree.saved().await;
                })
            }),
        ],
    );
}

/// Explores `ops` on a tree holding `d/f`, the first dropped at each point it parks at: every
/// slot the invariants find is live, free, or Discarded or below a Discarded node; a run whose
/// first op ran to its end leaves no slot detached, and some dropped run leaves one. On a served
/// tree, every live dirty node's ancestors are dirty too. A save's steps touch what no footprint
/// records (the State's data lock, the unused permit, the blocks' images), so with one among
/// `ops` every interleaving is run.
fn explore(name: &'static str, served: bool, saves: bool, ops: Vec<Op<Context>>) {
    let leaked = Arc::new(AtomicUsize::new(0));
    let counted = leaked.clone();
    let scenario = Scenario::<Context> {
        name,
        setup: Box::new(move || {
            Box::pin(async move {
                let tree = Tree::new(&["d/f"]).await;
                if served {
                    tree.state.mark_served();
                }
                Context {
                    tree,
                    done: AtomicBool::new(false),
                }
            })
        }),
        ops,
        cancellable: vec![0],
        check: Box::new(move |c: Arc<Context>| {
            let leaked = counted.clone();
            Box::pin(async move {
                let report = crate::holding(
                    &c.tree.state,
                    &c.tree.repository,
                    Rules {
                        dirty_closure: served,
                        unique_names: true,
                        no_ext_flags: false,
                        no_builds: false,
                    },
                )
                .await;
                if c.done.load(Ordering::Relaxed) {
                    assert_eq!(
                        report.detached, 0,
                        "a finished attach leaves no slot behind"
                    );
                } else if report.detached > 0 {
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
            sleep_sets: !saves,
            ..Exploration::default()
        },
    );
    assert!(
        leaked.load(Ordering::Relaxed) > 0,
        "no run dropped the attach holding its slot, in {} runs",
        covered.runs
    );
}
