// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The log's records (`hooks::log`), each at the site that makes it: operations run on the
//! deterministic scheduler with recording on, their records read in each run's check, every run's
//! log held to the rules every log keeps ([`check_log`]) and replayed through the model
//! ([`check_model`]). A lock found held, and records made on several threads at once, are the
//! chaos scheduler's.

mod arms;
mod fields;
mod kinds;
mod modelled;
mod numbering;
mod refusals;
mod sections;
mod threads;

use std::cell::Cell;
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::mpsc;

use lore_revision::node::Node;
use lore_revision::node::NodeID;
use lore_revision::state::State;
use lore_revision::state::node::hooks;
use lore_revision::state::node::hooks::Point;
use lore_revision::state::node::hooks::Resource;
use lore_revision::state::node::hooks::Window;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;
use lore_revision::state::node::hooks::log::Words;

use crate::harness::catalogue::call::Call;
use crate::harness::catalogue::call::Spawn;
use crate::harness::catalogue::modelled::Labels;
use crate::harness::catalogue::modelled::check_log;
use crate::harness::log::Collector;
use crate::harness::model::Modelled;
use crate::harness::model::Start;
use crate::harness::model::checkpoint::Checkpoint;
use crate::harness::model::fields::Fields;
use crate::harness::scheduler;
use crate::harness::scheduler::Exploration;
use crate::harness::scheduler::Scenario;

pub(crate) type Setup<C> = fn() -> Pin<Box<dyn Future<Output = C>>>;

pub(crate) type Body<C> = Box<dyn Fn(Arc<C>) -> Pin<Box<dyn Future<Output = ()> + Send>>>;

/// Every run's log and context, as its check leaves them.
type Runs<C> = Rc<RefCell<Vec<(Vec<Record>, Arc<C>)>>>;

/// What one run of `op` alone on the context `setup` builds logged, in `(state, seq, sub)` order,
/// and the context.
pub(crate) fn logged<C: Modelled + Send + Sync + 'static>(
    setup: Setup<C>,
    op: Body<C>,
) -> (Vec<Record>, Arc<C>) {
    logged_as(setup, op, Labels::Required)
}

/// [`logged`], with `labels` saying whether its writes must carry labels.
fn logged_as<C: Modelled + Send + Sync + 'static>(
    setup: Setup<C>,
    op: Body<C>,
    labels: Labels,
) -> (Vec<Record>, Arc<C>) {
    let mut runs = logged_runs(setup, vec![op], vec![], labels, Exploration::default());
    assert_eq!(runs.len(), 1, "one operation alone runs once");
    runs.remove(0)
}

/// Every run's log of `ops`, those of `cancellable` cancelled at each of their points too, explored
/// on the context `setup` builds, as `exploration` says, recording on, each log held to
/// [`check_log`] and [`check_model`] in its run's check, the model started where the run's setup
/// ended.
pub(crate) fn logged_runs<C: Modelled + Send + Sync + 'static>(
    setup: Setup<C>,
    ops: Vec<Body<C>>,
    cancellable: Vec<usize>,
    labels: Labels,
    exploration: Exploration,
) -> Vec<(Vec<Record>, Arc<C>)> {
    let runs: Runs<C> = Rc::default();
    let into = runs.clone();
    // The run's start, read where its setup ends, each run setting it up afresh; the setup and the
    // check run one after the other on the exploring thread.
    let start: Rc<Cell<Start>> = Rc::default();
    let read = start.clone();
    let swept = Rc::new(Cell::new(0));
    let scenario = Scenario::<C> {
        name: "log",
        setup: Box::new(move || {
            let read = read.clone();
            Box::pin(async move {
                let context = setup().await;
                read.set(Start::read(&context).await);
                context
            })
        }),
        ops,
        cancellable,
        check: Box::new(move |context: Arc<C>| {
            let into = into.clone();
            let start = start.take();
            let sweep = swept.replace(swept.get() + 1) < SWEPT_RUNS;
            Box::pin(async move {
                let log = scheduler::run_log();
                let called = (scheduler::run_calls(), scheduler::run_spawns());
                check_log(&log, labels);
                check_model(&start, &*context, (&log, called), labels, sweep).await;
                into.borrow_mut().push((log, context));
            })
        }),
    };
    let covered = scenario.explore(Exploration {
        recording: true,
        ..exploration
    });
    // Every schedule within the preemption bound, where the bound limited it.
    assert!(
        covered.complete || (covered.bound == Some(exploration.preemptions) && !covered.timed_out),
        "explored {} runs without finishing",
        covered.runs
    );
    runs.take()
}

/// The runs of each exploration whose every `Word` is altered field by field: the sweep replays
/// the log once per field of each `Word`.
const SWEPT_RUNS: usize = 4;

/// The run's model, started at `start` and replayed over `log` with the run's calls and how each
/// of its tasks came to be, finds no violation and matches the State: every set is at least as
/// wide as the code wrote in the run. Where `sweep`, every `Word` altered in each field it
/// compares is caught at its own `(state, seq, sub)` exactly where the change it then makes
/// against the model's words leaves its set: the model judges every `Word`. That a set is no wider
/// than its section's code writes rests on the reading of each section's writes
/// (`model::fields::set`). With `Labels::Free`, a test's own edits are admitted, and the State is
/// not compared: an edit of a node's structure tells no record.
async fn check_model<C: Modelled>(
    start: &Start,
    context: &C,
    (log, (calls, spawns)): (&[Record], (Vec<Call>, Vec<Spawn>)),
    labels: Labels,
    sweep: bool,
) {
    let mut fresh = start.model(context, log).await;
    if labels == Labels::Free {
        fresh.admit_unlabelled();
    }
    fresh.gated();
    // The model before any record, kept for the sweep's replays.
    let (mut model, fresh) = if sweep {
        (fresh.clone(), Some(fresh))
    } else {
        (fresh, None)
    };
    if sweep {
        model.judging();
    }
    let dropped_outside = scheduler::run_dropped_outside_calls();
    model.replay_called(log, calls, spawns);
    let violations: Vec<String> = model.violations().iter().map(ToString::to_string).collect();
    assert!(violations.is_empty(), "the model found: {violations:#?}");
    if labels == Labels::Required {
        let checked = model
            .checkpoint(context, Checkpoint::ending(dropped_outside))
            .await;
        assert!(
            checked.violations.is_empty(),
            "the checkpoint found: {:#?}",
            checked.violations
        );
    }
    let Some(fresh) = fresh else {
        return;
    };
    for (index, record) in log.iter().enumerate() {
        let Effect::Word {
            node,
            record: words,
            clear_pending,
        } = &record.effect
        else {
            continue;
        };
        let at = (record.state, record.seq, record.sub);
        let judged = model.judged(at).expect("every Word judged");
        for field in Fields::each().filter(|field| Fields::compared(&words.0).contains(*field)) {
            let (mut altered, mut pending) = (words.0, *clear_pending);
            field.alter(&mut altered, &mut pending);
            let mut written = log.to_vec();
            written[index].effect = Effect::Word {
                node: *node,
                record: Words(altered),
                clear_pending: pending,
            };
            let mut replayed = fresh.clone();
            replayed.replay(&written);
            // The write set's finding alone: an altered word may break a rule of its section's
            // decision too.
            let caught = replayed
                .violations()
                .iter()
                .any(|violation| violation.at() == at && violation.is_write_set());
            assert_eq!(
                caught,
                judged.catches(&altered, pending),
                "{:?}'s word of {node} with its {field:?} altered, judged as {judged:?}: {:#?}",
                record.section,
                replayed.violations()
            );
        }
    }
}

/// A scheduler recording on its thread into a run's log, which stalls the thread in one window it
/// reaches, where it stalls at all, until another thread has done what it waits for.
pub(crate) struct InWindow {
    log: Arc<Collector>,
    stall: Option<Stall>,
}

/// Where an [`InWindow`] stalls, once, at the `at`th window: it tells `stalled`, then waits on
/// `resume`.
struct Stall {
    stalled: mpsc::Sender<()>,
    resume: Mutex<mpsc::Receiver<()>>,
    at: usize,
    reached: AtomicUsize,
}

impl InWindow {
    /// One recording into `log` and stalling at the `at`th window it reaches, from 1, with the
    /// receiver that hears it stall and the sender that resumes it.
    pub(crate) fn stalling(
        log: Arc<Collector>,
        at: usize,
    ) -> (Arc<Self>, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (stalled, in_window) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let stall = Stall {
            stalled,
            resume: Mutex::new(resumed),
            at,
            reached: AtomicUsize::new(0),
        };
        let scheduler = Arc::new(Self {
            log,
            stall: Some(stall),
        });
        (scheduler, in_window, resume)
    }

    /// One recording into `log` that never stalls.
    pub(crate) fn never_stalling(log: Arc<Collector>) -> Arc<Self> {
        Arc::new(Self { log, stall: None })
    }
}

impl hooks::Scheduler for InWindow {
    fn reached(&self, _point: Point) -> Option<Pin<Box<dyn Future<Output = ()> + Send>>> {
        self.log.reserve();
        None
    }

    fn touched(&self, _resource: Resource, _write: bool) {}

    fn window(&self, _window: Window) {
        let Some(stall) = &self.stall else {
            return;
        };
        if stall.reached.fetch_add(1, Ordering::Relaxed) + 1 == stall.at {
            stall.stalled.send(()).expect("the other thread waits");
            let resume = stall.resume.lock().unwrap_or_else(PoisonError::into_inner);
            resume.recv().expect("the other thread is done");
        }
    }

    fn recording(&self) -> bool {
        true
    }

    fn effect(&self, record: Record) {
        self.log.record(record);
    }
}

/// The records of `state`.
pub(crate) fn of<'a>(records: &'a [Record], state: &State) -> Vec<&'a Record> {
    let id = state.hooks().id();
    records.iter().filter(|record| record.state == id).collect()
}

/// The words of `node` each `Word` record of it carries.
fn words_of(records: &[&Record], node: NodeID) -> Vec<Node> {
    records
        .iter()
        .filter_map(|record| match &record.effect {
            Effect::Word {
                node: of,
                record: words,
                ..
            } if *of == node => Some(words.0),
            _ => None,
        })
        .collect()
}

/// The `Word` records of `node`.
fn word_records<'a>(records: &[&'a Record], node: NodeID) -> Vec<&'a Record> {
    records
        .iter()
        .copied()
        .filter(|record| matches!(record.effect, Effect::Word { node: of, .. } if of == node))
        .collect()
}

/// The one record `matches` picks.
fn one<'a>(records: &[&'a Record], what: &str, matches: impl Fn(&Effect) -> bool) -> &'a Record {
    let found: Vec<_> = records
        .iter()
        .filter(|record| matches(&record.effect))
        .collect();
    assert_eq!(found.len(), 1, "one {what} in {records:#?}");
    found[0]
}

/// The records of the section `record` is in.
fn section_of<'a>(records: &[&'a Record], record: &Record) -> Vec<&'a Record> {
    records
        .iter()
        .copied()
        .filter(|other| (other.state, other.seq) == (record.state, record.seq))
        .collect()
}

/// The distinct `seq`s of the sections labelled `label`.
fn sections_labelled(records: &[&Record], label: Section) -> Vec<u64> {
    let mut seqs: Vec<u64> = records
        .iter()
        .filter(|record| record.section == label)
        .map(|record| record.seq)
        .collect();
    seqs.dedup();
    seqs
}
