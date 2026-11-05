// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A scenario held to the model: each run's log held to the rules every log keeps
//! ([`check_log`]) and replayed through the model started where the run's setup ended, the
//! scenario's own check recorded and followed, and the run's checkpoint ([`modelled`]).

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;

use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;

use crate::harness::catalogue::call::SelfReported;
use crate::harness::log::recorded;
use crate::harness::model::Modelled;
use crate::harness::model::Start;
use crate::harness::model::changes;
use crate::harness::model::checkpoint::Checkpoint;
use crate::harness::model::compared;
use crate::harness::scheduler;
use crate::harness::scheduler::Exploration;
use crate::harness::scheduler::Scenario;

/// Whether a run's writes must carry a section's label: every write a module section makes does;
/// a test's own direct edits (`node_edit_for_tests`) do not.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Labels {
    Required,
    Free,
}

/// Whether the run counted `run` from 0 takes the round trip by its place: each whose count plus
/// one is a power of two, the first two and a handful more, spread over the exploration, as its
/// first runs repeat one schedule's choices but the last. A scenario's runs are so counted, and
/// its dropped runs among themselves, as a drop part way is what a save could lose something of.
pub fn round_trip(run: usize) -> bool {
    (run + 1).is_power_of_two()
}

/// How a scenario's runs are held to the model ([`modelled`]).
#[derive(Clone, Copy)]
pub struct Modelling {
    /// Whether the run's writes must carry a section's label. With `Labels::Free` the model still
    /// holds the State against itself, so a test's own edit of a node's structure, which tells
    /// no record, fails the run.
    pub labels: Labels,
    /// Whether the scenario's own check runs before the checkpoint, recorded, the model following
    /// what it loads and writes, so the check finds the State as the run left it; or after it,
    /// unrecorded, for a check the model cannot follow, one writing outside every section.
    pub check_first: bool,
    /// Whether runs take the round trip: those [`round_trip`] picks among all of the scenario's
    /// runs and among its dropped ones.
    pub round_trips: bool,
    /// Whether a run that dropped an operation excuses closure on an unserved State, where the
    /// drop broke it: a dropped call excuses the breaches its records formed; a task dropped
    /// outside every call, the whole State. Not where no operation the scenario drops leaves a
    /// dirty node under a clean one, a save or a drain. A failed call's breaches, and a fault the
    /// context tells outside every call, are excused either way.
    pub drops_excuse_closure: bool,
    /// The self-reported outcomes its runs admit, as a lowered retry bound reaches them; none
    /// under the product's bounds, which no explored schedule meets.
    pub self_reports: &'static [SelfReported],
    /// Whether its runs drop calls of kinds their production callers never drop
    /// ([`crate::harness::catalogue::Kind::cancellable`]), to show what the model makes of such a
    /// drop; elsewhere each is a violation.
    pub uncancellable_drops: bool,
}

impl Default for Modelling {
    fn default() -> Self {
        Self {
            labels: Labels::Required,
            check_first: true,
            round_trips: true,
            drops_excuse_closure: true,
            self_reports: &[],
            uncancellable_drops: false,
        }
    }
}

/// What the checkpoints of a modelled scenario's runs counted, over every run so far.
#[derive(Default)]
pub struct Checks {
    /// The runs checked.
    pub runs: Cell<usize>,
    /// The runs that dropped an operation.
    pub dropped: Cell<usize>,
    /// The dropped runs that took the round trip by their place among dropped runs alone.
    pub dropped_round_trips: Cell<usize>,
    /// The States saved and loaded again for the round trip.
    pub saved: Cell<usize>,
    /// The unserved States' orphans the checkpoints counted, after each scenario's own check.
    pub orphans: Cell<usize>,
    /// The calls that reported an outcome of their own, by what they reported.
    pub self_reported: RefCell<BTreeMap<SelfReported, usize>>,
}

/// A scenario whose every run is held to the model: explored with recording on.
pub struct ModelledScenario<C> {
    scenario: Scenario<C>,
    checks: Rc<Checks>,
}

impl<C: Modelled + Send + Sync + 'static> ModelledScenario<C> {
    /// Explores the scenario as `exploration` says, recording, at its preemption bound alone: the
    /// modelled checks make each run dear, and the bounds below it run no schedule it does not.
    pub fn explore(&self, exploration: Exploration) -> scheduler::Covered {
        self.scenario.explore(Exploration {
            recording: true,
            deepening: false,
            ..exploration
        })
    }

    pub fn checks(&self) -> &Checks {
        &self.checks
    }
}

/// `scenario` with each run's log held to [`check_log`] and replayed through the model started
/// where the run's setup ended, then, around the scenario's own check as `modelling` says, the
/// run's checkpoint (`Model::checkpoint`): the model against the State, the orphans, the
/// invariants under the rules the model gives, on every State in the first run and on those a
/// record changed after, and the round trip on the runs `modelling` picks. A check run first finds
/// the model held against the State as the run left it, before its own work changes it, and its
/// records are held to the log's rules and replayed, but those of a State it loads and does not
/// change. Nothing is evicted: no operation follows a run's check.
pub fn modelled<C: Modelled + Send + Sync + 'static>(
    scenario: Scenario<C>,
    modelling: Modelling,
) -> ModelledScenario<C> {
    let Scenario {
        name,
        setup,
        ops,
        cancellable,
        check,
    } = scenario;
    // The run's start, read where its setup ends, each run setting it up afresh; the setup and the
    // check run one after the other on the exploring thread.
    let start: Rc<Cell<Start>> = Rc::default();
    let read = start.clone();
    let setup: scheduler::Setup<C> = Box::new(move || {
        let (read, made) = (read.clone(), setup());
        Box::pin(async move {
            let context = made.await;
            read.set(Start::read(&context).await);
            context
        })
    });
    let own = Rc::new(check);
    let checks = Rc::new(Checks::default());
    let counted = checks.clone();
    let check: scheduler::Check<C> = Box::new(move |context: Arc<C>| {
        let (start, own, counted) = (start.take(), own.clone(), counted.clone());
        let dropped = scheduler::run_cancelled();
        let run = counted.runs.replace(counted.runs.get() + 1);
        let earlier = counted
            .dropped
            .replace(counted.dropped.get() + usize::from(dropped));
        let by_drop = dropped && round_trip(earlier) && !round_trip(run);
        let round_trip = modelling.round_trips && (round_trip(run) || by_drop);
        if modelling.round_trips && by_drop {
            counted
                .dropped_round_trips
                .set(counted.dropped_round_trips.get() + 1);
        }
        Box::pin(async move {
            let log = scheduler::run_log();
            let (calls, spawns) = (scheduler::run_calls(), scheduler::run_spawns());
            let dropped_outside = scheduler::run_dropped_outside_calls();
            check_log(&log, modelling.labels);
            context.mend();
            let mut model = start.model(&*context, &log).await;
            if modelling.labels == Labels::Free {
                model.admit_unlabelled();
            }
            model.admitting_self_reports(modelling.self_reports);
            model.gated();
            if !modelling.drops_excuse_closure {
                model.holding_drops_to_closure();
            }
            if modelling.uncancellable_drops {
                model.admitting_uncancellable_drops();
            }
            model.replay_called(&log, calls, spawns);
            let found: Vec<String> = model.violations().iter().map(ToString::to_string).collect();
            assert!(found.is_empty(), "the model found: {found:#?}");
            for (&what, &count) in model.self_reported() {
                *counted.self_reported.borrow_mut().entry(what).or_default() += count;
            }
            if modelling.check_first {
                let mut found = compared(&model, &*context);
                for (state, _) in context.states() {
                    if let Some(modelled) = model.state(state.hooks().id()) {
                        found.extend(modelled.orphans().violations);
                    }
                }
                assert!(found.is_empty(), "where the run ended: {found:#?}");
                let ((), records) = recorded(own(context.clone())).await;
                check_log(&records, modelling.labels);
                let records = followed(records);
                model.provide_needed(&*context, &records).await;
                model.replay(&records);
                let found: Vec<String> =
                    model.violations().iter().map(ToString::to_string).collect();
                assert!(
                    found.is_empty(),
                    "the model found, after the check: {found:#?}"
                );
            }
            let checkpoint = Checkpoint {
                dropped: dropped_outside && modelling.drops_excuse_closure,
                round_trip,
                evict: false,
                every_state: run == 0,
            };
            let checked = model.checkpoint(&*context, checkpoint).await;
            assert!(
                checked.violations.is_empty(),
                "the checkpoint found: {:#?}",
                checked.violations
            );
            assert_eq!(
                checked.window, 0,
                "a served orphan in the rename window, which no modelled scenario makes yet"
            );
            counted.saved.set(counted.saved.get() + checked.saved);
            counted.orphans.set(counted.orphans.get() + checked.orphans);
            if !modelling.check_first {
                own(context).await;
            }
        })
    });
    ModelledScenario {
        scenario: Scenario {
            name,
            setup,
            ops,
            cancellable,
            check,
        },
        checks,
    }
}

/// `records` but those of each State they load and do not change, which the model need not
/// follow: a check reading back a save, whose reads it judges itself.
pub fn followed(mut records: Vec<Record>) -> Vec<Record> {
    let loaded: BTreeSet<u64> = records
        .iter()
        .filter(|record| matches!(record.effect, Effect::Created { from: Some(_) }))
        .map(|record| record.state)
        .collect();
    if loaded.is_empty() {
        return records;
    }
    let changed: BTreeSet<u64> = records
        .iter()
        .filter(|record| changes(&record.effect))
        .map(|record| record.state)
        .collect();
    records.retain(|record| !loaded.contains(&record.state) || changed.contains(&record.state));
    records
}

/// Whether `record` writes and so carries its section's label: a word, a slot, a structural change,
/// or a refusal or an admission of one. Wider than a change of its State (`model::changes`) by the
/// refusals and admissions, which change nothing, and narrower by the State-wide records, which no
/// section makes.
fn writes(record: &Record) -> bool {
    matches!(
        record.effect,
        Effect::Word { .. }
            | Effect::Grab { .. }
            | Effect::Free { .. }
            | Effect::Publish { .. }
            | Effect::BuildAdd { .. }
            | Effect::Abandon { .. }
            | Effect::Detach { .. }
            | Effect::Pend { .. }
            | Effect::Discarding { .. }
            | Effect::Move { .. }
            | Effect::Rename { .. }
            | Effect::Repin { .. }
            | Effect::Refused { .. }
            | Effect::Admitted { .. }
    )
}

/// The rules every run's log keeps:
/// - each `(state, seq, sub)` once (the drain sorts by it);
/// - every record made in a task;
/// - each `(state, seq)` made by one task, its records numbered `sub` 0, 1, … with no gap, so no
///   two sections shared a `seq`; that no record left its section is each test's to show;
/// - with `Labels::Required`, every write, slot, structural and refusal record labelled;
/// - no writer's snapshot grew its thread's buffer, any thread's so far: an allocation under a
///   block's lock; and the log's upkeep met no fault.
pub fn check_log(log: &[Record], labels: Labels) {
    assert_eq!(
        lore_revision::node::words_grown(),
        0,
        "a writer's snapshot grew its thread's buffer"
    );
    assert_eq!(
        lore_revision::state::node::hooks::log::faults(),
        0,
        "the log's upkeep met a fault"
    );
    for pair in log.windows(2) {
        assert!(
            (pair[0].state, pair[0].seq, pair[0].sub) < (pair[1].state, pair[1].seq, pair[1].sub),
            "the log is in (state, seq, sub) order, each once: {:?} then {:?}",
            pair[0],
            pair[1]
        );
    }
    for record in log {
        assert!(record.task.is_some(), "a record with no task: {record:?}");
    }
    for section in log.chunk_by(|a, b| (a.state, a.seq) == (b.state, b.seq)) {
        assert!(
            section
                .iter()
                .zip(0u32..)
                .all(|(record, sub)| record.sub == sub),
            "a section's records numbered in order: {section:#?}"
        );
        assert!(
            section.iter().all(|record| record.task == section[0].task),
            "a section made by one task: {section:#?}"
        );
    }
    if labels == Labels::Required {
        for record in log {
            assert!(
                !writes(record) || record.section != Section::Unlabelled,
                "a write with no section's label: {record:?}"
            );
        }
    }
}
