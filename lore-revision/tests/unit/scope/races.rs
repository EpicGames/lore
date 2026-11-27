// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Creates, renames, archives and restores of scopes, raced in pairs through every order of the
//! steps [`BranchStep`] marks between their reads and writes. Each order must end in a state the
//! operations that succeeded end in when run one at a time, in an order that runs one that ended
//! before another started ahead of it, and in which every name maps to the scope that holds it.

use std::collections::BTreeMap;
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_revision::branch;
use lore_revision::branch::record;
use lore_revision::repository::RepositoryContext;
use lore_revision::scope;
use lore_revision::scope::Scopes;
use lore_revision::test_util::BranchStep;
use lore_revision::test_util::step_branch_writes;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use super::scope_id;
use super::scoped;
use crate::branch::null_repository;

/// The scope the races are about.
pub(super) const RACED: u8 = 1;
/// A scope that races the raced scope for a name.
const OTHER: u8 = 2;
/// A scope that races [`OTHER`] for a name.
const THIRD: u8 = 3;
/// The name the raced scope has when a race starts with it live or archived.
const OLD: &str = "old";
const BEE: &str = "bee";
const SEA: &str = "sea";
const NAMES: [&str; 3] = [OLD, BEE, SEA];

/// An operation of a race.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Op {
    Create(u8, &'static str),
    Rename(u8, &'static str),
    Archive(u8),
    Restore(u8, &'static str),
}

/// How an operation ended: the error variants a race allows, and a refusal by the scope's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Succeeded,
    Refused,
    Taken,
    NotFound,
}

impl Op {
    async fn run(self, repository: Arc<RepositoryContext>) -> Outcome {
        let written = match self {
            Op::Create(id, name) => {
                scope::create(repository, scope_id(id), name, "creator", 7).await
            }
            Op::Rename(id, name) => scope::rename(repository, scope_id(id), name).await,
            Op::Archive(id) => scope::archive(repository, scope_id(id)).await,
            Op::Restore(id, name) => scope::restore(repository, scope_id(id), name).await,
        };
        match written {
            Ok(Ok(_)) => Outcome::Succeeded,
            Ok(Err(_)) => Outcome::Refused,
            Err(err) if err.is_already_exists() => Outcome::Taken,
            Err(err) if err.is_not_found() => Outcome::NotFound,
            Err(err) => panic!("{self:?} failed with {err:?}"),
        }
    }
}

/// What the scopes are when a race starts.
#[derive(Clone, Copy, Debug)]
enum Start {
    /// The raced scope is live as [`OLD`].
    Live,
    /// The raced scope is archived from [`OLD`].
    Archived,
    /// [`Start::Live`], then a rename to [`BEE`] stopped after its claim.
    StoppedRename,
    /// [`Start::Archived`], then a restore under [`BEE`] stopped after its claim.
    StoppedRestore,
    /// [`Start::Live`], then an archive stopped before it released [`OLD`], which still maps to
    /// the raced scope.
    StoppedArchive,
    /// The raced scope created as [`SEA`] and renamed [`OLD`], so its metadata marks [`SEA`]
    /// released.
    Renamed,
}

impl Start {
    /// Makes the scopes what `self` names in `repository`, answering the model of them.
    async fn prepare(self, repository: &Arc<RepositoryContext>) -> Model {
        let mut model = Model::default();
        let created: &[Op] = if matches!(self, Start::Renamed) {
            &[Op::Create(RACED, SEA), Op::Rename(RACED, OLD)]
        } else {
            &[Op::Create(RACED, OLD)]
        };
        let archived = matches!(self, Start::Archived | Start::StoppedRestore);
        for &op in created
            .iter()
            .chain(archived.then_some(&Op::Archive(RACED)))
        {
            assert_eq!(
                op.run(repository.clone()).await,
                Outcome::Succeeded,
                "{op:?}"
            );
            assert!(model.apply(op), "{op:?}");
        }
        let stopped = match self {
            Start::StoppedRename => Some(Op::Rename(RACED, BEE)),
            Start::StoppedRestore => Some(Op::Restore(RACED, BEE)),
            _ => None,
        };
        if let Some(op) = stopped {
            stop_at(repository, op, BranchStep::RestoreNameClaimed).await;
            model.names.insert(BEE, RACED);
            model.pending = Some((RACED, BEE));
        }
        if matches!(self, Start::StoppedArchive) {
            stop_at(repository, Op::Archive(RACED), BranchStep::ArchiveReleasing).await;
            model.scopes.insert(RACED, (OLD, true));
        }
        model
    }
}

/// Runs `op` in `repository` to `step`, and stops it there, as a process that ends there does.
pub(super) async fn stop_at(repository: &Arc<RepositoryContext>, op: Op, step: BranchStep) {
    let mut racer = Racer::new(op);
    loop {
        match racer.advance(repository).await {
            Advanced::Stopped(reached) if reached == step => return,
            Advanced::Stopped(_) => {}
            Advanced::Ended(outcome) => panic!("{op:?} ended before {step:?}: {outcome:?}"),
        }
    }
}

/// The names and metadata of the scopes of a race.
#[derive(Clone, Debug, Default, PartialEq)]
struct Model {
    /// The scope each name maps to.
    names: BTreeMap<&'static str, u8>,
    /// The name each scope's metadata holds, and whether it says the scope is archived.
    scopes: BTreeMap<u8, (&'static str, bool)>,
    /// A scope whose metadata records a claim that stopped, and the name it claimed, which maps
    /// to the scope.
    pending: Option<(u8, &'static str)>,
}

impl Model {
    /// Runs `op` as it runs alone, answering whether it succeeds. A write of a scope with a
    /// pending claim settles it first, which releases the name the claim held.
    fn apply(&mut self, op: Op) -> bool {
        let id = match op {
            Op::Create(id, _) | Op::Rename(id, _) | Op::Archive(id) | Op::Restore(id, _) => id,
        };
        if !matches!(op, Op::Create(..)) {
            self.settle(id);
        }
        let held = |model: &Self, name: &str| model.names.get(name).is_some_and(|&by| by != id);
        match (op, self.scopes.get(&id).copied()) {
            (Op::Create(_, name), None) => {
                if held(self, name) {
                    return false;
                }
                self.scopes.insert(id, (name, false));
                self.names.insert(name, id);
                true
            }
            (Op::Create(_, name), Some(record)) => {
                record == (name, false) && self.pending.is_none_or(|(of, _)| of != id)
            }
            (Op::Archive(_), Some((name, _))) => {
                if self.names.get(name) == Some(&id) {
                    self.names.remove(name);
                }
                self.scopes.insert(id, (name, true));
                true
            }
            (Op::Rename(_, name), Some((old, false))) => {
                if old == name {
                    return true;
                }
                if held(self, name) {
                    return false;
                }
                self.names.remove(old);
                self.names.insert(name, id);
                self.scopes.insert(id, (name, false));
                true
            }
            (Op::Restore(_, name), Some((old, true))) => {
                if old != name && self.names.get(old) == Some(&id) {
                    self.names.remove(old);
                }
                if held(self, name) {
                    return false;
                }
                self.names.insert(name, id);
                self.scopes.insert(id, (name, false));
                true
            }
            (Op::Restore(_, name), Some((held, false))) => held == name,
            (Op::Rename(..) | Op::Restore(..), _) | (Op::Archive(_), None) => false,
        }
    }

    /// Releases the name a pending claim of scope `id` claimed, and drops the claim.
    fn settle(&mut self, id: u8) {
        if let Some((of, name)) = self.pending
            && of == id
        {
            self.pending = None;
            if self.names.get(name) == Some(&id) {
                self.names.remove(name);
            }
        }
    }

    /// The names, metadata and pending claims `repository` holds.
    async fn read(repository: &Arc<RepositoryContext>) -> Model {
        let mut model = Model::default();
        for name in NAMES {
            let held = record::name_mapping::<Scopes>(repository.clone(), name)
                .await
                .expect("reading a name mapping");
            if !held.is_zero() {
                model.names.insert(name, known_scope(held));
            }
        }
        for id in [RACED, OTHER] {
            let Some(hash) = record::metadata_key::<Scopes>(repository.clone(), scope_id(id))
                .await
                .expect("reading a metadata key")
            else {
                continue;
            };
            let metadata = record::load_metadata::<scope::ScopeError>(repository.clone(), hash)
                .await
                .expect("reading scope metadata");
            let known = |key| metadata.get_string(key).ok().map(known_name);
            let name = known(branch::NAME).expect("the metadata names the scope");
            let archived = metadata.get_u64(branch::ARCHIVED).is_ok()
                && metadata.get_u64(branch::RESTORED).is_err();
            model.scopes.insert(id, (name, archived));
            if let Some(pending) = known(branch::RESTORING) {
                model.pending = Some((id, pending));
            }
        }
        model
    }

    /// Whether every name maps to the scope that holds it and no other: a live scope's name maps
    /// to it, and a name maps only to a live scope that holds it, to the one a pending claim
    /// claims, or to an archived scope that holds it, as an archive that stopped before its release
    /// leaves it.
    fn agrees(&self) -> bool {
        let live = self
            .scopes
            .iter()
            .filter(|&(&id, &(_, archived))| {
                !archived && self.pending.is_none_or(|(of, _)| of != id)
            })
            .all(|(&id, &(name, _))| self.names.get(name) == Some(&id));
        let held = self.names.iter().all(|(&name, &id)| {
            self.pending == Some((id, name))
                || self.scopes.get(&id).is_some_and(|&(held, _)| held == name)
        });
        live && held
    }
}

/// The byte of the race's scope `id` is.
fn known_scope(id: lore_base::types::Context) -> u8 {
    [RACED, OTHER, THIRD]
        .into_iter()
        .find(|&byte| scope_id(byte) == id)
        .unwrap_or_else(|| panic!("a name maps to {id}, which is no scope of the race"))
}

/// The name of the race `name` is.
fn known_name(name: &str) -> &'static str {
    NAMES
        .into_iter()
        .find(|&known| known == name)
        .unwrap_or_else(|| panic!("{name} is no name of the race"))
}

/// Where an operation the test let go on stopped.
enum Advanced {
    Stopped(BranchStep),
    Ended(Outcome),
}

/// An operation run as a task that stops at each [`BranchStep`] it reaches, until the test lets
/// it go on. The task starts at the first [`Racer::advance`], so no two run at once.
struct Racer {
    op: Op,
    stepped: Option<(
        mpsc::UnboundedReceiver<BranchStep>,
        Arc<Notify>,
        JoinSet<Outcome>,
    )>,
    started: usize,
}

impl Racer {
    fn new(op: Op) -> Self {
        Self {
            op,
            stepped: None,
            started: 0,
        }
    }

    /// Lets the operation go on, starting it the first time, to its next step or its end.
    async fn advance(&mut self, repository: &Arc<RepositoryContext>) -> Advanced {
        let (reached, _, task) = if let Some(stepped) = &mut self.stepped {
            stepped.1.notify_one();
            stepped
        } else {
            let (sender, reached) = mpsc::unbounded_channel();
            let resume = Arc::new(Notify::new());
            let mut task = JoinSet::new();
            lore_spawn!(
                task,
                step_branch_writes(sender, resume.clone(), self.op.run(repository.clone()))
            );
            self.stepped.insert((reached, resume, task))
        };
        match reached.recv().await {
            Some(step) => Advanced::Stopped(step),
            None => Advanced::Ended(
                task.join_next()
                    .await
                    .expect("the operation's task")
                    .expect("the operation runs to its end"),
            ),
        }
    }
}

/// An operation of a race that ended, and the points of the race where it started and ended.
struct Finished {
    op: Op,
    outcome: Outcome,
    started: usize,
    ended: usize,
}

/// What one order of a race left.
struct Run {
    initial: Model,
    finished: Vec<Finished>,
    end: Model,
    trace: Vec<(usize, Option<BranchStep>)>,
    taken: Vec<usize>,
    options: Vec<usize>,
}

/// Runs `ops` from `start`, letting go on at each point the operation `pick` chooses among those
/// that haven't ended, by its index in them.
async fn run_order(start: Start, ops: &[Op], mut pick: impl FnMut(usize) -> usize) -> Run {
    let repository = null_repository().await;
    let initial = start.prepare(&repository).await;
    let mut racers: Vec<Racer> = ops.iter().map(|&op| Racer::new(op)).collect();
    let mut finished: Vec<Option<Finished>> = ops.iter().map(|_| None).collect();
    let (mut trace, mut taken, mut options) = (Vec::new(), Vec::new(), Vec::new());
    loop {
        let unended: Vec<usize> = (0..ops.len()).filter(|&i| finished[i].is_none()).collect();
        if unended.is_empty() {
            break;
        }
        let point = taken.len();
        let choice = pick(point);
        let next = unended[choice];
        taken.push(choice);
        options.push(unended.len());
        let racer = &mut racers[next];
        if racer.stepped.is_none() {
            racer.started = point;
        }
        match racer.advance(&repository).await {
            Advanced::Stopped(step) => trace.push((next, Some(step))),
            Advanced::Ended(outcome) => {
                finished[next] = Some(Finished {
                    op: racer.op,
                    outcome,
                    started: racer.started,
                    ended: point,
                });
                trace.push((next, None));
            }
        }
    }
    Run {
        initial,
        finished: finished
            .into_iter()
            .map(|finished| finished.expect("every operation ended"))
            .collect(),
        end: Model::read(&repository).await,
        trace,
        taken,
        options,
    }
}

/// Runs `ops` from `start` in every order of their steps, and checks each with [`check`].
async fn race(start: Start, ops: &[Op]) {
    scoped(async {
        let mut choices: Vec<usize> = Vec::new();
        loop {
            let run = run_order(start, ops, |point| choices.get(point).copied().unwrap_or(0)).await;
            check(start, ops, &run);
            let Some(point) = (0..run.taken.len())
                .rev()
                .find(|&point| run.taken[point] + 1 < run.options[point])
            else {
                return;
            };
            choices = run.taken[..point].to_vec();
            choices.push(run.taken[point] + 1);
        }
    })
    .await;
}

/// Checks that every name of `run`'s end maps to the scope that holds it ([`Model::agrees`]),
/// and that the operations that succeeded explain how the race ended ([`explained`]).
fn check(start: Start, ops: &[Op], run: &Run) {
    let outcomes: Vec<_> = run.finished.iter().map(|f| (f.op, f.outcome)).collect();
    assert!(
        run.end.agrees(),
        "{start:?} {ops:?}: a name maps to a scope that doesn't hold it\nsteps: {:?}\n\
         outcomes: {outcomes:?}\nend: {:?}",
        run.trace,
        run.end
    );
    assert!(
        explained(&run.initial, &run.finished, &run.end),
        "{start:?} {ops:?}: no order of the operations that succeeded ends as the race did\n\
         steps: {:?}\noutcomes: {outcomes:?}\nend: {:?}",
        run.trace,
        run.end
    );
}

/// Whether running the operations of `finished` that succeeded one at a time, in some order that
/// keeps one that ended before another started first, ends as `end`, from `initial` or, where an
/// operation that failed may have settled a pending claim before the others ran, from `initial`
/// with the claim settled.
fn explained(initial: &Model, finished: &[Finished], end: &Model) -> bool {
    let succeeded: Vec<usize> = (0..finished.len())
        .filter(|&i| finished[i].outcome == Outcome::Succeeded)
        .collect();
    let mut starts = vec![initial.clone()];
    if let Some((id, _)) = initial.pending {
        let mut settled = initial.clone();
        settled.settle(id);
        starts.push(settled);
    }
    if finished
        .iter()
        .any(|other| other.outcome != Outcome::Succeeded)
    {
        let mut released = initial.clone();
        released
            .names
            .retain(|name, id| released_scope(initial, *id, name).is_none());
        if released != *initial {
            starts.push(released);
        }
    }
    starts.into_iter().any(|start| {
        orders(&succeeded).into_iter().any(|order| {
            let in_time = order.iter().enumerate().all(|(position, &earlier)| {
                order[position + 1..]
                    .iter()
                    .all(|&later| finished[later].ended > finished[earlier].started)
            });
            let mut model = start.clone();
            in_time
                && order.iter().all(|&i| model.apply(finished[i].op))
                && model.names == end.names
                && model.scopes == end.scopes
                && model.pending == end.pending
        })
    })
}

/// The archived scope `id` that `name`, a name its metadata holds, maps to in `model`, as an
/// archive that stopped before its release leaves it, and which a failed operation may release.
fn released_scope(model: &Model, id: u8, name: &str) -> Option<u8> {
    (model.scopes.get(&id) == Some(&(known_name(name), true))).then_some(id)
}

/// Every order of `items`.
fn orders(items: &[usize]) -> Vec<Vec<usize>> {
    if items.is_empty() {
        return vec![Vec::new()];
    }
    (0..items.len())
        .flat_map(|first| {
            let mut rest = items.to_vec();
            let head = rest.remove(first);
            orders(&rest).into_iter().map(move |mut order| {
                order.insert(0, head);
                order
            })
        })
        .collect()
}

/// Each operation that writes the raced scope, and a create of another scope that takes a name
/// the raced scope's writes claim.
const OPS: [Op; 9] = [
    Op::Create(RACED, OLD),
    Op::Create(OTHER, BEE),
    Op::Rename(RACED, BEE),
    Op::Rename(RACED, SEA),
    Op::Archive(RACED),
    Op::Restore(RACED, OLD),
    Op::Restore(RACED, BEE),
    Op::Restore(RACED, SEA),
    Op::Create(OTHER, OLD),
];

/// Races every pair of [`OPS`], from `start`.
async fn race_pairs(start: Start) {
    for (i, &first) in OPS.iter().enumerate() {
        for &second in &OPS[i..] {
            race(start, &[first, second]).await;
        }
    }
}

#[tokio::test]
async fn pairs_race_on_a_live_scope() {
    race_pairs(Start::Live).await;
}

#[tokio::test]
async fn pairs_race_on_an_archived_scope() {
    race_pairs(Start::Archived).await;
}

#[tokio::test]
async fn pairs_race_on_a_scope_whose_rename_stopped() {
    race_pairs(Start::StoppedRename).await;
}

#[tokio::test]
async fn pairs_race_on_a_scope_whose_restore_stopped() {
    race_pairs(Start::StoppedRestore).await;
}

#[tokio::test]
async fn pairs_race_on_a_scope_renamed_once() {
    race_pairs(Start::Renamed).await;
}

#[tokio::test]
async fn pairs_race_on_a_scope_whose_archive_stopped() {
    race_pairs(Start::StoppedArchive).await;
}

#[tokio::test]
async fn two_creates_of_new_scopes_race_for_one_name() {
    let creates = [Op::Create(OTHER, SEA), Op::Create(THIRD, SEA)];
    scoped(async {
        let mut choices: Vec<usize> = Vec::new();
        loop {
            let run = run_order(Start::Live, &creates, |point| {
                choices.get(point).copied().unwrap_or(0)
            })
            .await;
            let succeeded = run
                .finished
                .iter()
                .filter(|f| f.outcome == Outcome::Succeeded)
                .count();
            assert_eq!(succeeded, 1, "steps: {:?}", run.trace);
            let Some(point) = (0..run.taken.len())
                .rev()
                .find(|&point| run.taken[point] + 1 < run.options[point])
            else {
                return;
            };
            choices = run.taken[..point].to_vec();
            choices.push(run.taken[point] + 1);
        }
    })
    .await;
}

/// A write of a scope whose claim stopped settles it: the name the claim held is free again, and
/// the scope keeps the state it had.
#[tokio::test]
async fn a_stopped_claim_is_settled_by_the_next_write() {
    scoped(async {
        for (start, op, end) in [
            (Start::StoppedRename, Op::Archive(RACED), (OLD, true)),
            (Start::StoppedRename, Op::Rename(RACED, SEA), (SEA, false)),
            (Start::StoppedRestore, Op::Archive(RACED), (OLD, true)),
            (Start::StoppedRestore, Op::Restore(RACED, SEA), (SEA, false)),
        ] {
            let repository = null_repository().await;
            start.prepare(&repository).await;
            assert_eq!(
                op.run(repository.clone()).await,
                Outcome::Succeeded,
                "{start:?} {op:?}"
            );
            let model = Model::read(&repository).await;
            assert_eq!(model.pending, None, "{start:?} {op:?}");
            assert_eq!(model.scopes.get(&RACED), Some(&end), "{start:?} {op:?}");
            assert!(
                !model.names.contains_key(BEE),
                "{start:?} {op:?}: {model:?}"
            );
            assert!(model.agrees(), "{start:?} {op:?}: {model:?}");
            assert_eq!(
                Op::Create(OTHER, BEE).run(repository.clone()).await,
                Outcome::Succeeded,
                "{start:?} {op:?}"
            );
        }
    })
    .await;
}
