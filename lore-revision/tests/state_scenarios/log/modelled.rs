// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A scenario held to the model ([`modelled`]): what its runs' logs, its check's own work and its
//! checkpoints each catch, and what the checkpoints count.

use std::cell::Cell;
use std::panic::AssertUnwindSafe;
use std::rc::Rc;
use std::sync::Arc;

use lore_base::types::Hash;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFlags;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::Ext;
use lore_revision::state::Mark;
use lore_revision::state::NameChange;
use lore_revision::state::State;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;
use lore_revision::state::node_discard_fused;
use lore_storage::hash::hash_string;

use crate::harness::catalogue::call::SelfReported;
use crate::harness::catalogue::modelled::Labels;
use crate::harness::catalogue::modelled::Modelling;
use crate::harness::catalogue::modelled::followed;
use crate::harness::catalogue::modelled::modelled;
use crate::harness::catalogue::modelled::round_trip;
use crate::harness::catalogue::ops;
use crate::harness::fixtures::Tree;
use crate::harness::fixtures::execution;
use crate::harness::invariants;
use crate::harness::model::Modelled;
use crate::harness::scheduler::Check;
use crate::harness::scheduler::Exploration;
use crate::harness::scheduler::Op;
use crate::harness::scheduler::Scenario;
use crate::probes::panic_message;

/// The scenario of `ops` and `check` on an unserved tree holding `d/f` and `e/`.
fn scenario(ops: Vec<Op<Tree>>, check: Check<Tree>) -> Scenario<Tree> {
    Scenario {
        name: "modelled",
        setup: Box::new(|| Box::pin(Tree::new(&["d/f", "e/"]))),
        ops,
        cancellable: vec![],
        check,
    }
}

/// An op creating `name` in `e`, as a VFS provider diff's checked create makes one.
fn create(name: &'static str) -> Op<Tree> {
    Box::new(move |tree: Arc<Tree>| {
        Box::pin(async move {
            let created = ops::checked_create(&tree, tree.id("e"), name).await;
            assert!(
                ops::attached(created).is_some(),
                "the name is free: {created:?}"
            );
        })
    })
}

/// A test's own edit turning `d/f` dirty, its directory left clean: a write of no section.
async fn dirty_below_clean(tree: &Tree) {
    let f = tree.id("d/f");
    let block = invariants::block_of(&tree.state, &tree.repository, f).await;
    tree.state.node_edit_for_tests(&block, f, |node| {
        node.flags |= NodeFlags::Dirty.bits();
    });
}

fn edit_dirty_below_clean() -> Op<Tree> {
    Box::new(|tree: Arc<Tree>| Box::pin(async move { dirty_below_clean(&tree).await }))
}

fn nothing() -> Check<Tree> {
    Box::new(|_| Box::pin(async {}))
}

/// What the run that fails `scenario`, held to the model as `modelling` says, panics with.
fn failure<C: Modelled + Send + Sync + 'static>(
    scenario: Scenario<C>,
    modelling: Modelling,
) -> String {
    let explored = std::panic::catch_unwind(AssertUnwindSafe(|| {
        modelled(scenario, modelling).explore(Exploration::default())
    }));
    panic_message(explored.expect_err("a run fails"))
}

#[test]
fn a_runs_write_of_no_section_fails_it() {
    let message = failure(
        scenario(vec![edit_dirty_below_clean()], nothing()),
        Modelling::default(),
    );
    assert!(
        message.contains("a write with no section's label"),
        "{message}"
    );
}

/// A tree whose context names its committed State alone, not the State its runs write.
struct Unnamed(Tree);

impl Modelled for Unnamed {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        vec![(self.0.committed.clone(), self.0.repository.clone())]
    }
}

/// A raw add of `e/y`, in no call, in the State an [`Unnamed`] context does not name.
async fn create_unnamed(unnamed: Arc<Unnamed>) {
    let tree = &unnamed.0;
    tree.state
        .node_add(tree.repository.clone(), tree.id("e"), ops::file("y"), "y")
        .await
        .expect("the add runs");
}

/// The scenario whose run creates `e/y` in the State its context does not name where `in_run`,
/// and whose check does where not.
fn unnamed(in_run: bool) -> Scenario<Unnamed> {
    Scenario {
        name: "modelled, a State unnamed",
        setup: Box::new(|| Box::pin(async { Unnamed(Tree::new(&["d/f", "e/"]).await) })),
        ops: vec![Box::new(move |unnamed: Arc<Unnamed>| {
            Box::pin(async move {
                if in_run {
                    create_unnamed(unnamed).await;
                }
            })
        })],
        cancellable: vec![],
        check: Box::new(move |unnamed: Arc<Unnamed>| {
            Box::pin(async move {
                if !in_run {
                    create_unnamed(unnamed).await;
                }
            })
        }),
    }
}

/// A write to a State the context does not name, which the model never started: the replay of the
/// run's log finds the run's, and the replay of a check's records finds the check's.
#[test]
fn a_write_to_a_state_the_model_never_started_fails_the_run_or_check_making_it() {
    let message = failure(unnamed(true), Modelling::default());
    assert!(
        message.contains("the model found: ") && message.contains("never started"),
        "{message}"
    );
    let message = failure(unnamed(false), Modelling::default());
    assert!(
        message.contains("the model found, after the check") && message.contains("never started"),
        "{message}"
    );
}

/// A test's own edit, admitted, leaves the model and the State agreeing on a dirty node under a
/// clean directory: the checkpoint's invariants, under the rules the model gives an unserved State
/// whose run dropped nothing, hold closure and fail.
#[test]
fn the_checkpoint_holds_the_invariants_the_model_gives() {
    let message = failure(
        scenario(vec![edit_dirty_below_clean()], nothing()),
        Modelling {
            labels: Labels::Free,
            ..Modelling::default()
        },
    );
    assert!(
        message.contains("the checkpoint found")
            && message.contains("dirty 'd/f'")
            && message.contains("under clean 'd'"),
        "{message}"
    );
}

/// A check that marks a node, then loads the State's save and marks the node the run created in
/// it: the model follows its writes and the State it loads and writes, so the checkpoint after it
/// finds nothing.
#[test]
fn the_model_follows_what_a_check_run_first_writes_and_loads() {
    let check: Check<Tree> = Box::new(|tree: Arc<Tree>| {
        Box::pin(async move {
            ops::mark_dirty(&tree, tree.id("d/f")).await;
            let saved = tree.saved().await;
            let y = saved
                .find_subnode(tree.repository.clone(), tree.id("e"), hash_string("y"))
                .await
                .expect("the save holds the create");
            let mark = Mark::dirty(
                NodeFlags::DirtyModify,
                Ext::Set(NodeExtFlags::NoFlags),
                false,
            );
            saved
                .node_mark(&tree.repository, y, mark)
                .await
                .expect("the loaded State marks its node");
        })
    });
    let scenario = modelled(scenario(vec![create("y")], check), Modelling::default());
    assert!(scenario.explore(Exploration::default()).complete);
    assert_eq!(scenario.checks().runs.get(), 1);
}

/// `d/f` respelled `name` on a thread no scheduler holds, so nothing is recorded: a change the log
/// does not tell.
fn respelled_unrecorded(tree: &Arc<Tree>, name: &'static str) {
    let tree = tree.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the runtime starts");
        runtime.block_on(lore_base::runtime::LORE_CONTEXT.scope(execution(), async {
            tree.state
                .node_rename(
                    tree.repository.clone(),
                    tree.id("d/f"),
                    NameChange::Rename(name),
                )
                .await
                .expect("'d/f' is respelled");
        }));
    })
    .join()
    .expect("the respelling thread");
}

/// A run that respells `d/f` behind the model, and a check that respells it back, recorded: the
/// model, which never held the run's spelling, agrees with the State after the check, so only
/// the compare where the run ended finds the difference.
#[test]
fn a_difference_the_run_leaves_is_found_before_the_check_hides_it() {
    let respell: Op<Tree> =
        Box::new(|tree: Arc<Tree>| Box::pin(async move { respelled_unrecorded(&tree, "F") }));
    let back: Check<Tree> = Box::new(|tree: Arc<Tree>| {
        Box::pin(async move {
            tree.state
                .node_rename(
                    tree.repository.clone(),
                    tree.id("d/f"),
                    NameChange::Rename("f"),
                )
                .await
                .expect("'d/f' is respelled back");
        })
    });
    let message = failure(scenario(vec![respell], back), Modelling::default());
    assert!(
        message.contains("where the run ended") && message.contains("name"),
        "{message}"
    );
}

/// A check's write of no section, which a check run first is held to as a run's are; one run after
/// the checkpoint, for a check the model cannot follow, is not.
#[test]
fn a_checks_write_of_no_section_fails_it_where_it_runs_first() {
    let check = || -> Check<Tree> {
        Box::new(|tree: Arc<Tree>| Box::pin(async move { dirty_below_clean(&tree).await }))
    };
    let message = failure(scenario(vec![create("y")], check()), Modelling::default());
    assert!(
        message.contains("a write with no section's label"),
        "{message}"
    );
    let ran = Rc::new(Cell::new(0));
    let counted = ran.clone();
    let counting: Check<Tree> = Box::new(move |tree: Arc<Tree>| {
        counted.set(counted.get() + 1);
        Box::pin(async move { dirty_below_clean(&tree).await })
    });
    let after = modelled(
        scenario(vec![create("y")], counting),
        Modelling {
            check_first: false,
            ..Modelling::default()
        },
    );
    assert!(after.explore(Exploration::default()).complete);
    assert_eq!(
        ran.get(),
        after.checks().runs.get(),
        "the check ran after each run"
    );
}

/// A clear dropped at each point it parks at beside a mark of its file's sibling: a clear dropped
/// between clearing its directory and marking it again leaves the directory clean over the marked
/// file (C12), which a dropped run excuses, and holds where the scenario says drops excuse
/// nothing.
#[test]
fn a_dropped_run_excuses_closure_where_the_modelling_says() {
    let clear_dropped = || Scenario::<Tree> {
        name: "modelled, a clear dropped beside a mark",
        setup: Box::new(|| {
            Box::pin(async {
                let tree = Tree::new(&["x/p/a", "x/p/c", "x/q/g"]).await;
                ops::mark_dirty(&tree, tree.id("x/p/a")).await;
                tree
            })
        }),
        ops: vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move { ops::scan_clear(&tree, tree.id("x/p/a")).await })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move { ops::scan_mark_modified(&tree, tree.id("x/p/c")).await })
            }),
        ],
        cancellable: vec![0],
        check: nothing(),
    };
    let cancelling = Exploration {
        cancellation: true,
        ..Exploration::default()
    };
    let excused = modelled(clear_dropped(), Modelling::default());
    assert!(excused.explore(cancelling).complete);
    let explored = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let held = Modelling {
            drops_excuse_closure: false,
            ..Modelling::default()
        };
        modelled(clear_dropped(), held).explore(cancelling)
    }));
    let message = panic_message(explored.expect_err("a dropped run leaves closure open"));
    assert!(message.contains("under clean"), "{message}");
}

/// The clear dropped beside the mark, as above, and a check that, where the drop left `x/p` clean
/// over the marked `x/p/c`, edits `d/f` dirty under a clean `d`: the dropped call excuses the
/// breach it left alone, so the checkpoint finds the edit's, which the model holds itself.
#[test]
fn a_dropped_call_excuses_no_breach_it_did_not_leave() {
    let left = Scenario::<Tree> {
        name: "modelled, a clear dropped beside a breach it did not leave",
        setup: Box::new(|| {
            Box::pin(async {
                let tree = Tree::new(&["x/p/a", "x/p/c", "d/f"]).await;
                ops::mark_dirty(&tree, tree.id("x/p/a")).await;
                tree
            })
        }),
        ops: vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move { ops::scan_clear(&tree, tree.id("x/p/a")).await })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move { ops::scan_mark_modified(&tree, tree.id("x/p/c")).await })
            }),
        ],
        cancellable: vec![0],
        check: Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let read = |path| tree.state.node(tree.repository.clone(), tree.id(path));
                let p = read("x/p").await.expect("'x/p' reads");
                let c = read("x/p/c").await.expect("'x/p/c' reads");
                if !p.is_dirty() && c.is_dirty() {
                    dirty_below_clean(&tree).await;
                }
            })
        }),
    };
    let explored = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let free = Modelling {
            labels: Labels::Free,
            ..Modelling::default()
        };
        modelled(left, free).explore(Exploration {
            cancellation: true,
            ..Exploration::default()
        })
    }));
    let message = panic_message(explored.expect_err("the edit's breach fails a dropped run"));
    assert!(
        message.contains("the checkpoint found")
            && message.contains("which no dropped or failed call left"),
        "{message}"
    );
}

/// A mark no call holds, dropped at each point it parks at: dropped between its levels it leaves
/// its file dirty under a clean directory, which no call's records tell, so the run excuses closure
/// on the whole State.
#[test]
fn a_drop_outside_every_call_excuses_closure_on_the_whole_state() {
    let bare = Scenario::<Tree> {
        name: "modelled, a mark no call holds dropped",
        setup: Box::new(|| Box::pin(Tree::new(&["x/p/c"]))),
        ops: vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move { ops::mark_dirty(&tree, tree.id("x/p/c")).await })
        })],
        cancellable: vec![0],
        check: nothing(),
    };
    let explored = modelled(bare, Modelling::default());
    assert!(
        explored
            .explore(Exploration {
                cancellation: true,
                ..Exploration::default()
            })
            .complete
    );
    assert!(
        explored.checks().dropped.get() > 0,
        "no run dropped the mark"
    );
}

/// Records of State `state` at `seq`, one per effect, numbered in order.
fn records_of(state: u64, seq: u64, effects: Vec<Effect>) -> Vec<Record> {
    effects
        .into_iter()
        .zip(0u32..)
        .map(|(effect, sub)| Record {
            seq,
            sub,
            section: Section::Unlabelled,
            state,
            task: Some(0),
            effect,
        })
        .collect()
}

/// A check's records but those of a State it loads and does not change: a State loaded and read,
/// its blocks loaded and a walk's head, goes; one loaded and then changed stays whole and in order,
/// one not loaded stays.
#[test]
fn a_checks_records_follow_the_states_it_changes() {
    let loaded = Effect::Created {
        from: Some((1, Hash::default())),
    };
    let read = Effect::Loaded {
        block: 0,
        instance: 1,
        zeroed: false,
    };
    let walked = Effect::Head {
        node: 0,
        changes: 0,
        instance: 1,
    };
    let changed = Effect::Unfinished { set: true };
    let mut records = records_of(1, 0, vec![loaded.clone(), read.clone(), walked]);
    records.extend(records_of(2, 0, vec![loaded, read.clone()]));
    records.extend(records_of(2, 1, vec![changed.clone()]));
    records.extend(records_of(3, 0, vec![read, changed]));
    let kept: Vec<(u64, u64, u32)> = followed(records.clone())
        .iter()
        .map(|record| (record.state, record.seq, record.sub))
        .collect();
    assert_eq!(
        kept,
        [(2, 0, 0), (2, 0, 1), (2, 1, 0), (3, 0, 0), (3, 0, 1)]
    );
}

/// The runs [`round_trip`] picks take the round trip, each saving the one State its run wrote, and
/// none other where no run drops anything; with no round trips, none does.
#[test]
fn the_runs_picked_take_the_round_trip() {
    let explored = |round_trips| {
        let scenario = modelled(
            scenario(vec![create("y"), create("z")], nothing()),
            Modelling {
                round_trips,
                ..Modelling::default()
            },
        );
        assert!(scenario.explore(Exploration::default()).complete);
        let checks = scenario.checks();
        (checks.runs.get(), checks.saved.get())
    };
    let (runs, saved) = explored(true);
    let picked = (0..runs).filter(|run| round_trip(*run)).count();
    assert!(runs > picked, "{runs} runs");
    assert_eq!(saved, picked, "of {runs} runs");
    assert_eq!(explored(false).1, 0);
}

/// A fused discard dropped at each point it parks at, beside a create elsewhere: a run dropping it
/// after it unlinked its root and before it freed what stands below leaves those nodes below a
/// detached root, which the checkpoint counts on an unserved State; and dropped runs enough that
/// some take the round trip by their place among the dropped alone.
#[test]
fn a_discard_dropped_part_way_leaves_orphans_the_checkpoint_counts() {
    let discard: Op<Tree> = Box::new(|tree: Arc<Tree>| {
        Box::pin(async move {
            let d = tree.id("d");
            let _ = node_discard_fused(tree.state.clone(), tree.repository.clone(), d).await;
        })
    });
    let scenario = modelled(
        Scenario {
            name: "modelled, a discard dropped",
            setup: Box::new(|| Box::pin(Tree::new(&["d/a", "d/b", "e/"]))),
            ops: vec![discard, create("y")],
            cancellable: vec![0],
            check: nothing(),
        },
        Modelling::default(),
    );
    let covered = scenario.explore(Exploration {
        cancellation: true,
        ..Exploration::default()
    });
    assert!(covered.complete, "{covered:?}");
    let checks = scenario.checks();
    assert!(checks.dropped.get() > 0, "no run dropped the discard");
    assert!(checks.orphans.get() > 0, "no dropped run left an orphan");
    assert!(
        checks.dropped_round_trips.get() > 0,
        "no dropped run took the round trip by its place among the dropped"
    );
}

/// Two checked creates into one directory under a retry bound of one attempt: a create whose
/// publish the other's overtook looks its name up again and answers `SlowDown`, an outcome of its
/// own the checks count by what it reported. Under the product's bound none does.
#[test]
fn the_checks_count_self_reported_outcomes() {
    let slowed = |bound: Option<u32>| {
        let unasserted = |name: &'static str| -> Op<Tree> {
            Box::new(move |tree: Arc<Tree>| {
                Box::pin(async move {
                    ops::checked_create(&tree, tree.id("e"), name).await;
                })
            })
        };
        let scenario = modelled(
            Scenario {
                name: "modelled, two checked creates under a retry bound",
                setup: Box::new(move || {
                    Box::pin(async move {
                        let tree = Tree::new(&["d/f", "e/"]).await;
                        tree.state.set_retry_bound_for_tests(bound);
                        tree
                    })
                }),
                ops: vec![unasserted("y"), unasserted("z")],
                cancellable: vec![],
                check: nothing(),
            },
            Modelling {
                self_reports: &[SelfReported::SlowDown],
                ..Modelling::default()
            },
        );
        assert!(scenario.explore(Exploration::default()).complete);
        let reported = scenario.checks().self_reported.borrow().clone();
        reported.get(&SelfReported::SlowDown).copied().unwrap_or(0)
    };
    assert!(slowed(Some(1)) > 0, "no create met a bound of one");
    assert_eq!(slowed(None), 0, "the product's bound is never met");
}

/// A served RMDIR of a directory beside a create and removal of a file in it, the RMDIR's attempts
/// lowered to one: an RMDIR whose walk the create and removal overtook answers its retries spent,
/// an outcome of its own the checks count; at the VFS provider's 64, none does.
#[test]
fn the_checks_count_an_rmdir_spending_its_retries() {
    let spent = |attempts: u32| {
        let scenario = modelled(
            Scenario {
                name: "modelled, an RMDIR beside a create and removal in its directory",
                setup: Box::new(move || {
                    Box::pin(async move {
                        ops::set_rmdir_attempts_for_tests(attempts);
                        let tree = Tree::new(&["d/", "e/"]).await;
                        tree.state.mark_served();
                        assert!(ops::served_mark_dir(&tree, tree.id("d")).await);
                        tree
                    })
                }),
                ops: vec![
                    Box::new(|tree: Arc<Tree>| {
                        Box::pin(async move {
                            ops::served_rmdir(&tree, tree.id("d")).await;
                        })
                    }),
                    Box::new(|tree: Arc<Tree>| {
                        Box::pin(async move {
                            if let Some(x) = ops::served_create(&tree, tree.id("d"), "x").await {
                                ops::served_detach(&tree, x).await;
                            }
                        })
                    }),
                ],
                cancellable: vec![],
                check: nothing(),
            },
            Modelling {
                self_reports: &[SelfReported::RetriesSpent],
                ..Modelling::default()
            },
        );
        assert!(scenario.explore(Exploration::default()).complete);
        ops::set_rmdir_attempts_for_tests(64);
        let reported = scenario.checks().self_reported.borrow().clone();
        reported
            .get(&SelfReported::RetriesSpent)
            .copied()
            .unwrap_or(0)
    };
    assert!(spent(1) > 0, "no RMDIR spent an attempt of one");
    assert_eq!(
        spent(64),
        0,
        "the VFS provider's 64 attempts are never spent"
    );
}
