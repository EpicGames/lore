// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Creates, restores, archives and protects of one branch, raced through every order of the steps
//! [`BranchStep`] marks between their reads and writes. Each order must end in the state that the
//! operations that succeeded end in when run one at a time. That serial order runs an operation
//! that ended before another started ahead of it.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::BranchPoint;
use lore_revision::branch;
use lore_revision::repository::RepositoryContext;
use lore_revision::test_util::BranchStep;
use lore_revision::test_util::step_branch_writes;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use super::BranchError;
use super::BranchId;
use super::branch_id;
use super::branch_point;
use super::create_on;
use super::mapped;
use super::null_repository;
use super::with_execution;
use crate::helper::committed_repository;
use crate::helper::create_branch_on_instance;
use crate::helper::in_memory;
use crate::helper::setup_test_execution;

/// The branch the races are about.
const RACED: u8 = 1;
/// A branch that races the raced branch for a name.
const OTHER: u8 = 2;
/// The name the raced branch has when a race starts with it live or archived.
const OLD: &str = "old";
const BEE: &str = "bee";
const SEA: &str = "sea";
const NAMES: [&str; 3] = [OLD, BEE, SEA];

/// Every order of the steps.
const EVERY_ORDER: usize = usize::MAX;
/// The switches away from an operation that hasn't ended that an order of three operations
/// makes at most.
const PREEMPTIONS: usize = 3;

/// An operation of a race.
#[derive(Clone, Copy, Debug)]
enum Op {
    /// A create of branch `id` named `name`, as a file-system create does, which restores a branch
    /// that has metadata.
    Create { id: u8, name: &'static str },
    /// An archive of branch `id`.
    Archive { id: u8 },
    /// A protect of branch `id`, in a repository without a remote.
    Protect { id: u8 },
}

impl Op {
    async fn run(self, repository: Arc<RepositoryContext>) -> Result<(), BranchError> {
        match self {
            Op::Create { id, name } => {
                create_on(&repository, branch_id(id), name, vec![branch_point(9, 9)])
                    .await
                    .map(|_| ())
            }
            Op::Archive { id } => branch::delete(repository, branch_id(id)).await,
            Op::Protect { id } => branch::protect(repository, branch_id(id)).await,
        }
    }
}

fn create(id: u8, name: &'static str) -> Op {
    Op::Create { id, name }
}

fn archive(id: u8) -> Op {
    Op::Archive { id }
}

/// What the raced branch is when a race starts.
#[derive(Clone, Copy, Debug)]
enum Start {
    Absent,
    /// Live as [`OLD`].
    Live,
    /// Archived from [`OLD`].
    Archived,
    /// Live as [`OLD`], then its name released and nothing else, as an older version archives.
    OlderArchived,
    /// [`Start::OlderArchived`] for a branch created without a latest revision.
    OlderArchivedWithoutLatest,
    /// [`Start::Archived`], then a restore under [`BEE`] stopped after its claim.
    Stopped,
    /// [`Start::Archived`], then a restore under [`OLD`], the branch's own name, stopped after its
    /// claim.
    StoppedUnderOld,
    /// [`Start::Stopped`], then an archive that settled the restore stopped before it released
    /// [`BEE`], which the metadata names as released.
    Released,
}

impl Start {
    /// Makes the raced branch what `self` names in `repository`, answering the model of it.
    async fn prepare(self, repository: &Arc<RepositoryContext>) -> Model {
        let mut model = Model::default();
        match self {
            Start::Absent => {}
            Start::OlderArchivedWithoutLatest => {
                create_on(repository, branch_id(RACED), OLD, Vec::new())
                    .await
                    .expect("the raced branch is created");
                assert!(model.apply(create(RACED, OLD)));
                model.no_latest.insert(RACED);
            }
            _ => {
                create(RACED, OLD)
                    .run(repository.clone())
                    .await
                    .expect("the raced branch is created");
                assert!(model.apply(create(RACED, OLD)));
            }
        }
        if matches!(
            self,
            Start::Archived | Start::Stopped | Start::StoppedUnderOld | Start::Released
        ) {
            archive(RACED)
                .run(repository.clone())
                .await
                .expect("the raced branch is archived");
            assert!(model.apply(archive(RACED)));
        }
        if matches!(
            self,
            Start::OlderArchived | Start::OlderArchivedWithoutLatest
        ) {
            branch::release_name(repository.clone(), OLD, branch_id(RACED))
                .await
                .expect("the raced branch's name is released");
            model.names.remove(OLD);
        }
        if matches!(self, Start::Stopped | Start::Released) {
            stop_at(
                repository,
                create(RACED, BEE),
                BranchStep::RestoreNameClaimed,
            )
            .await;
            model.names.insert(BEE, RACED);
            model.pending = Some((RACED, BEE));
        }
        if matches!(self, Start::StoppedUnderOld) {
            stop_at(
                repository,
                create(RACED, OLD),
                BranchStep::RestoreNameClaimed,
            )
            .await;
            model.names.insert(OLD, RACED);
            model.pending = Some((RACED, OLD));
        }
        if matches!(self, Start::Released) {
            stop_at(
                repository,
                archive(RACED),
                BranchStep::PendingRestoreSettled,
            )
            .await;
            model.pending = None;
            model.released = Some((RACED, BEE));
        }
        model
    }
}

/// Runs `op` in `repository` to `step`, and stops it there, as a process that ends there does.
async fn stop_at(repository: &Arc<RepositoryContext>, op: Op, step: BranchStep) {
    advance_to(&mut Racer::new(op), repository, step).await;
}

/// Lets `racer` go on in `repository` until it reaches `step`.
async fn advance_to(racer: &mut Racer, repository: &Arc<RepositoryContext>, step: BranchStep) {
    loop {
        match racer.advance(repository).await {
            Advanced::Stopped(reached) if reached == step => return,
            Advanced::Stopped(_) => {}
            Advanced::Ended(outcome) => {
                panic!("{:?} ended before {step:?}: {outcome:?}", racer.op)
            }
        }
    }
}

/// Lets `racer` go on in `repository` to its end, answering its outcome.
async fn finish(racer: &mut Racer, repository: &Arc<RepositoryContext>) -> Result<(), BranchError> {
    loop {
        if let Advanced::Ended(outcome) = racer.advance(repository).await {
            return outcome;
        }
    }
}

/// What stopped, leaving [`BEE`] mapped to the raced branch, archived from [`OLD`], after a restore
/// of the branch under it.
#[derive(Clone, Copy, Debug)]
enum Stop {
    /// The restore, after its claim.
    Restore,
    /// The restore after its claim, then an archive that settled it, before its release.
    Settle,
    /// The restore after it recorded itself, which an archive then settled, and after it then
    /// claimed the name.
    LateClaim,
}

/// Leaves [`BEE`] mapped to the raced branch, archived from [`OLD`] in `repository`, as `left`
/// stops.
async fn leave_bee_mapped(repository: &Arc<RepositoryContext>, left: Stop) {
    match left {
        Stop::Restore => {
            stop_at(
                repository,
                create(RACED, BEE),
                BranchStep::RestoreNameClaimed,
            )
            .await;
        }
        Stop::Settle => {
            stop_at(
                repository,
                create(RACED, BEE),
                BranchStep::RestoreNameClaimed,
            )
            .await;
            stop_at(
                repository,
                archive(RACED),
                BranchStep::PendingRestoreSettled,
            )
            .await;
        }
        Stop::LateClaim => {
            let mut restore = Racer::new(create(RACED, BEE));
            advance_to(&mut restore, repository, BranchStep::RestoreRecorded).await;
            settle_unclaimed_restore(repository).await;
            advance_to(&mut restore, repository, BranchStep::RestoreNameClaimed).await;
        }
    }
    assert_eq!(
        mapped(repository, BEE).await,
        Some(branch_id(RACED)),
        "{left:?}"
    );
}

/// Archives the raced branch in `repository`, which settles a restore of it that recorded itself
/// and hasn't claimed its name: the archive releases no name, so it fails with `BranchNotFound`.
async fn settle_unclaimed_restore(repository: &Arc<RepositoryContext>) {
    let outcome = archive(RACED).run(repository.clone()).await;
    assert!(
        outcome.as_ref().is_err_and(|err| err.is_branch_not_found()),
        "got {outcome:?}"
    );
}

/// The names and metadata of the branches of a race.
#[derive(Clone, Debug, Default, PartialEq)]
struct Model {
    /// The branch each name maps to.
    names: BTreeMap<&'static str, u8>,
    /// The name each branch's metadata holds.
    metadata: BTreeMap<u8, &'static str>,
    /// A branch whose metadata records a restore that stopped, and the name it claimed, which maps
    /// to the branch.
    pending: Option<(u8, &'static str)>,
    /// A branch whose metadata names a name a settled restore had claimed, which may still map to
    /// the branch. Not read back.
    released: Option<(u8, &'static str)>,
    /// The branches without a latest revision, which a restore renames none of. Not read back.
    no_latest: BTreeSet<u8>,
}

impl Model {
    /// Runs `op` as it runs alone, answering whether it succeeds. A create of a branch with
    /// metadata restores it, which a rename refuses while the branch's old name maps to it, or
    /// where the branch has no latest revision. A restore or archive of a branch with a pending
    /// restore settles it first. A restore releases a released name first, and an archive one
    /// other than the branch's name, whether they then succeed or not. An archive succeeds where it
    /// released a name that mapped to the branch: the restore's it settled, or the branch's own.
    fn apply(&mut self, op: Op) -> bool {
        match op {
            Op::Create { id, name } => {
                if self.metadata.contains_key(&id) {
                    self.settle(id);
                    self.release_marked(id, None);
                }
                if self.names.contains_key(name) {
                    return false;
                }
                if let Some(old) = self.metadata.get(&id)
                    && *old != name
                    && (self.names.get(old) == Some(&id) || self.no_latest.contains(&id))
                {
                    return false;
                }
                if self.released.is_some_and(|(held, _)| held == id) {
                    self.released = None;
                }
                self.metadata.insert(id, name);
                self.names.insert(name, id);
                true
            }
            Op::Archive { id } => {
                let released = if self.pending.is_some_and(|(held, _)| held == id) {
                    self.settle(id)
                } else {
                    self.release_marked(id, self.metadata.get(&id).copied());
                    false
                };
                match self.metadata.get(&id) {
                    Some(name) if self.names.get(name) == Some(&id) => {
                        self.names.remove(name);
                        true
                    }
                    _ => released,
                }
            }
            Op::Protect { id } => self.metadata.contains_key(&id),
        }
    }

    /// Releases the name a pending restore of branch `id` claimed, and marks it released in place
    /// of the restore, answering whether the name mapped to the branch.
    fn settle(&mut self, id: u8) -> bool {
        let Some((held, name)) = self.pending else {
            return false;
        };
        if held != id {
            return false;
        }
        self.pending = None;
        self.released = Some((id, name));
        if self.names.get(name) != Some(&id) {
            return false;
        }
        self.names.remove(name);
        true
    }

    /// Releases the name branch `id`'s metadata marks released, unless it is `except`.
    fn release_marked(&mut self, id: u8, except: Option<&'static str>) {
        if let Some((held, name)) = self.released
            && held == id
            && Some(name) != except
            && self.names.get(name) == Some(&id)
        {
            self.names.remove(name);
        }
    }

    /// The names, metadata and pending restores `repository` holds.
    async fn read(repository: &Arc<RepositoryContext>) -> Model {
        let mut model = Model::default();
        for name in NAMES {
            if let Some(held) = mapped(repository, name).await {
                model.names.insert(name, known(held));
            }
        }
        for id in [RACED, OTHER] {
            if let Some(name) = metadata_name(repository, id).await {
                model.metadata.insert(id, name);
            }
            if let Some(name) = stamps(repository, id)
                .await
                .and_then(|stamps| stamps.restoring)
            {
                model.pending = Some((id, name));
            }
        }
        model
    }
}

/// What the metadata of a branch says of archives and restores.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Stamps {
    archived: bool,
    restored: bool,
    restoring: Option<&'static str>,
    released: Option<&'static str>,
}

/// The [`Stamps`] of branch `id`'s metadata, `None` where it has none.
async fn stamps(repository: &Arc<RepositoryContext>, id: u8) -> Option<Stamps> {
    let metadata = match branch::metadata(repository.clone(), branch_id(id)).await {
        Ok(metadata) => metadata,
        Err(err) if err.is_branch_not_found() => return None,
        Err(err) => panic!("reading the metadata of branch {id}: {err:?}"),
    };
    let known = |key: &str| {
        metadata.get_string(key).ok().map(|name| {
            NAMES
                .into_iter()
                .find(|&known| known == name)
                .unwrap_or_else(|| panic!("branch {id} records {name} as {key}"))
        })
    };
    Some(Stamps {
        archived: metadata.get_u64(branch::ARCHIVED).is_ok(),
        restored: metadata.get_u64(branch::RESTORED).is_ok(),
        restoring: known(branch::RESTORING),
        released: known(branch::RELEASED),
    })
}

/// The byte of the race's branch `id` is.
fn known(id: BranchId) -> u8 {
    [RACED, OTHER]
        .into_iter()
        .find(|&byte| branch_id(byte) == id)
        .unwrap_or_else(|| panic!("a name maps to {id}, which is no branch of the race"))
}

/// The name the metadata of branch `id` holds, `None` where it has none.
async fn metadata_name(repository: &Arc<RepositoryContext>, id: u8) -> Option<&'static str> {
    match branch::metadata(repository.clone(), branch_id(id)).await {
        Ok(metadata) => {
            let name = branch::name(&metadata).expect("the metadata holds a name");
            Some(
                NAMES
                    .into_iter()
                    .find(|&known| known == name)
                    .unwrap_or_else(|| panic!("branch {id} is named {name}")),
            )
        }
        Err(err) if err.is_branch_not_found() => None,
        Err(err) => panic!("reading the metadata of branch {id}: {err:?}"),
    }
}

/// Where an operation the test let go on stopped.
enum Advanced {
    Stopped(BranchStep),
    Ended(Result<(), BranchError>),
}

/// The task running an operation, the steps it reports and what it waits for to go on.
type Stepped = (
    mpsc::UnboundedReceiver<BranchStep>,
    Arc<Notify>,
    JoinSet<Result<(), BranchError>>,
);

/// An operation run as a task that stops at each [`BranchStep`] it reaches, until the test lets
/// it go on. The task starts at the first [`Racer::advance`], so no two run at once.
struct Racer {
    op: Op,
    stepped: Option<Stepped>,
    started: usize,
    settled: bool,
    released: Option<&'static str>,
}

impl Racer {
    fn new(op: Op) -> Self {
        Self {
            op,
            stepped: None,
            started: 0,
            settled: false,
            released: None,
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
/// `settled` says whether it settled a pending restore, and an archive's `released` is the name
/// the metadata held where it was about to release it.
struct Finished {
    op: Op,
    outcome: Result<(), BranchError>,
    started: usize,
    ended: usize,
    settled: bool,
    released: Option<&'static str>,
}

/// What one order of a race left.
struct Run {
    initial: Model,
    finished: Vec<Finished>,
    end: Model,
    /// The [`Stamps`] of each branch with metadata, as the race left them.
    end_stamps: BTreeMap<u8, Stamps>,
    /// The operation let go on at each point, and the step it stopped at, `None` for its end.
    trace: Vec<(usize, Option<BranchStep>)>,
    /// The option taken at each point, and how many there were.
    taken: Vec<usize>,
    options: Vec<usize>,
}

/// Runs `ops` from `start`, letting go on at each point the operation `pick` chooses among those
/// that haven't ended, by its index in them. Where the last operation let go on hasn't ended and
/// the order has switched away from such operations `preemptions` times, it is the only choice.
async fn run_order(
    start: Start,
    ops: &[Op],
    preemptions: usize,
    mut pick: impl FnMut(usize, &[usize]) -> usize,
) -> Run {
    let repository = null_repository().await;
    let initial = start.prepare(&repository).await;
    let mut racers: Vec<Racer> = ops.iter().map(|&op| Racer::new(op)).collect();
    let mut finished: Vec<Option<Finished>> = ops.iter().map(|_| None).collect();
    let (mut trace, mut taken, mut options) = (Vec::new(), Vec::new(), Vec::new());
    let mut last = None;
    let mut preempted = 0;
    loop {
        let unended: Vec<usize> = (0..ops.len()).filter(|&i| finished[i].is_none()).collect();
        if unended.is_empty() {
            break;
        }
        let choices = match last {
            Some(last) if preempted >= preemptions && unended.contains(&last) => vec![last],
            _ => unended.clone(),
        };
        let point = taken.len();
        let choice = pick(point, &choices);
        let next = choices[choice];
        if last.is_some_and(|last| last != next && unended.contains(&last)) {
            preempted += 1;
        }
        taken.push(choice);
        options.push(choices.len());
        let racer = &mut racers[next];
        if racer.stepped.is_none() {
            racer.started = point;
        }
        match racer.advance(&repository).await {
            Advanced::Stopped(step) => {
                racer.settled |= step == BranchStep::PendingRestoreSettled;
                if let (BranchStep::ArchiveReleasing, Op::Archive { id }) = (step, racer.op) {
                    racer.released = metadata_name(&repository, id).await;
                }
                trace.push((next, Some(step)));
            }
            Advanced::Ended(outcome) => {
                finished[next] = Some(Finished {
                    op: racer.op,
                    outcome,
                    started: racer.started,
                    ended: point,
                    settled: racer.settled,
                    released: racer.released,
                });
                trace.push((next, None));
            }
        }
        last = Some(next);
    }
    let mut end_stamps = BTreeMap::new();
    for id in [RACED, OTHER] {
        if let Some(stamps) = stamps(&repository, id).await {
            end_stamps.insert(id, stamps);
        }
    }
    Run {
        initial,
        end_stamps,
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

/// Runs `ops` from `start` in every order of their steps that switches away from an operation
/// that hasn't ended at most `preemptions` times, and checks each with [`check`].
async fn race(start: Start, ops: &[Op], preemptions: usize) {
    Box::pin(with_execution(async {
        let mut choices: Vec<usize> = Vec::new();
        loop {
            let run = run_order(start, ops, preemptions, |point, _| {
                choices.get(point).copied().unwrap_or(0)
            })
            .await;
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
    }))
    .await;
}

/// Checks that each operation of `run` that failed failed as one that finds the branch or a name
/// taken, or the branch gone, and that the operations that succeeded explain how the race ended
/// ([`explained`]).
fn check(start: Start, ops: &[Op], run: &Run) {
    let outcomes: Vec<_> = run
        .finished
        .iter()
        .map(|finished| {
            (
                finished.op,
                finished.outcome.as_ref().map_err(ToString::to_string),
            )
        })
        .collect();
    for finished in &run.finished {
        if let Err(err) = &finished.outcome {
            assert!(
                err.is_branch_already_exists() || err.is_branch_not_found(),
                "{start:?} {ops:?}: {:?} failed with {err:?}\nsteps: {:?}",
                finished.op,
                run.trace
            );
        }
    }
    assert!(
        explained(&run.initial, &run.finished, &run.end),
        "{start:?} {ops:?}: no order of the operations that succeeded ends as the race did\n\
         steps: {:?}\noutcomes: {outcomes:?}\nend: {:?}",
        run.trace,
        run.end
    );
    assert!(
        agrees(&run.end, &run.end_stamps),
        "{start:?} {ops:?}: the metadata disagrees with the names\nsteps: {:?}\n\
         outcomes: {outcomes:?}\nend: {:?}\nstamps: {:?}",
        run.trace,
        run.end,
        run.end_stamps
    );
}

/// Whether the metadata of each branch says what the names do: metadata that says the branch was
/// restored last has its name map to it, and metadata that says it was archived last, with no
/// pending restore, has no name map to it but the one it marks released.
fn agrees(end: &Model, end_stamps: &BTreeMap<u8, Stamps>) -> bool {
    end_stamps.iter().all(|(&id, stamps)| {
        if stamps.restored {
            end.metadata
                .get(&id)
                .is_some_and(|name| end.names.get(name) == Some(&id))
        } else if stamps.archived && stamps.restoring.is_none() {
            end.names
                .iter()
                .all(|(&name, &held)| held != id || Some(name) == stamps.released)
        } else {
            true
        }
    })
}

/// Whether running the operations of `finished` that succeeded one at a time, in some order that
/// keeps one that ended before another started first, ends as `end`. Outcomes no such order has
/// are allowed:
/// - Two creates of one branch under one name both succeed: they claim one mapping for it.
/// - An archive succeeds with the branch archived already, where it settled a restore of it that
///   failed, or released the name a restore of it that failed had claimed.
/// - An operation that failed settled a pending restore, or released a released name, before the
///   others ran.
/// - An archive that swapped the metadata, then failed with `BranchNotFound` because a failed
///   restore that stalled released the name first, ends as one that succeeded.
fn explained(initial: &Model, finished: &[Finished], end: &Model) -> bool {
    let succeeded: Vec<usize> = (0..finished.len())
        .filter(|&i| finished[i].outcome.is_ok())
        .collect();
    let preempted: Vec<usize> = (0..finished.len())
        .filter(|&i| {
            matches!(finished[i].op, Op::Archive { id } if failed_create(finished, id, finished[i].released))
                && finished[i].released.is_some()
                && finished[i]
                    .outcome
                    .as_ref()
                    .is_err_and(|err| err.is_branch_not_found())
        })
        .collect();
    let mut counted = Vec::new();
    for subset in 0..(1usize << preempted.len()) {
        let mut ops = succeeded.clone();
        ops.extend(
            preempted
                .iter()
                .enumerate()
                .filter(|&(bit, _)| subset & (1 << bit) != 0)
                .map(|(_, &i)| i),
        );
        counted.push(ops);
    }
    let mut starts = vec![initial.clone()];
    if let Some((id, _)) = initial.pending
        && finished
            .iter()
            .any(|other| other.outcome.is_err() && other.settled)
    {
        let mut settled = initial.clone();
        settled.settle(id);
        starts.push(settled);
    }
    if let Some((id, _)) = initial.released && finished.iter().any(|other| {
        other.outcome.is_err()
            && matches!(other.op, Op::Create { id: of, .. } | Op::Archive { id: of } if of == id)
    }) {
        let mut released = initial.clone();
        released.release_marked(id, None);
        starts.push(released);
    }
    starts.into_iter().any(|start| {
        counted.iter().flat_map(|ops| orders(ops)).any(|order| {
            let in_time = order.iter().enumerate().all(|(position, &earlier)| {
                order[position + 1..]
                    .iter()
                    .all(|&later| finished[later].ended > finished[earlier].started)
            });
            let mut model = start.clone();
            in_time
                && order
                    .iter()
                    .all(|&i| succeeds(&mut model, &finished[i], finished))
                && ends_as(&model, end)
        })
    })
}

/// Whether `finished`, which succeeded, succeeds on `model`, run alone or as an allowed outcome
/// of [`explained`].
fn succeeds(model: &mut Model, finished: &Finished, all: &[Finished]) -> bool {
    if model.apply(finished.op) {
        return true;
    }
    match finished.op {
        Op::Create { id, name } => {
            model.names.get(name) == Some(&id) && model.metadata.get(&id) == Some(&name)
        }
        Op::Archive { id } => {
            (finished.settled && failed_create(all, id, None))
                || finished
                    .released
                    .is_some_and(|released| failed_create(all, id, Some(released)))
        }
        Op::Protect { .. } => false,
    }
}

/// Whether `model` matches `end` in its names, metadata and pending restore.
fn ends_as(model: &Model, end: &Model) -> bool {
    model.names == end.names && model.metadata == end.metadata && model.pending == end.pending
}

/// Whether a create of branch `id` failed, under `name` where one is given.
fn failed_create(finished: &[Finished], id: u8, name: Option<&str>) -> bool {
    finished.iter().any(|other| {
        other.outcome.is_err()
            && matches!(other.op, Op::Create { id: other_id, name: other_name }
                if other_id == id && name.is_none_or(|name| name == other_name))
    })
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

/// A create, a restore that renames a branch, and an archive, each run alone, stop at each of
/// their steps in order and succeed.
#[tokio::test]
async fn an_operation_run_alone_stops_at_each_of_its_steps() {
    use BranchStep::*;
    Box::pin(with_execution(async {
        for (start, op, steps) in [
            (
                Start::Absent,
                create(RACED, BEE),
                &[
                    CreateNameRead,
                    CreateUnrestored,
                    CreateMetadataStored,
                    CreateNameClaimed,
                ][..],
            ),
            (
                Start::Archived,
                create(RACED, BEE),
                &[
                    RestoreMetadataRead,
                    RestoreChecked,
                    RestoreRecorded,
                    RestoreNameClaimed,
                ][..],
            ),
            (
                Start::Stopped,
                create(RACED, SEA),
                &[
                    RestoreMetadataRead,
                    PendingRestoreSettled,
                    RestoreMetadataRead,
                    RestoreChecked,
                    RestoreRecorded,
                    RestoreNameClaimed,
                ][..],
            ),
            (
                Start::Stopped,
                archive(RACED),
                &[
                    ArchiveMetadataRead,
                    PendingRestoreSettled,
                    ArchiveMetadataRead,
                ][..],
            ),
            (
                Start::Live,
                archive(RACED),
                &[ArchiveMetadataRead, ArchiveLookedUp, ArchiveReleasing][..],
            ),
        ] {
            let run = run_order(start, &[op], EVERY_ORDER, |_, _| 0).await;
            let reached: Vec<BranchStep> = run.trace.iter().filter_map(|&(_, step)| step).collect();
            assert_eq!(reached, steps, "{start:?} {op:?}");
            assert!(
                run.finished[0].outcome.is_ok(),
                "{start:?} {op:?}: got {:?}",
                run.finished[0].outcome
            );
        }
    }))
    .await;
}

/// A create of a branch under a new name, run while the branch's own create has stored its
/// metadata and not yet claimed its name, fails with `BranchAlreadyExists` naming the branch, and
/// the first create then succeeds: its name maps to the branch, the metadata holds it, and the
/// other name maps to nothing.
#[tokio::test]
async fn a_rename_refuses_a_branch_whose_create_has_not_claimed_its_name() {
    Box::pin(with_execution(async {
        let mut order = [0, 0, 0].into_iter();
        let run = run_order(
            Start::Absent,
            &[create(RACED, OLD), create(RACED, BEE)],
            EVERY_ORDER,
            |_, choices| {
                let racer = order.next().unwrap_or(1);
                choices.iter().position(|&i| i == racer).unwrap_or(0)
            },
        )
        .await;

        assert_eq!(
            run.trace[2],
            (0, Some(BranchStep::CreateMetadataStored)),
            "steps: {:?}",
            run.trace
        );
        let Err(BranchError::BranchAlreadyExists(exists)) = &run.finished[1].outcome else {
            panic!("got {:?}", run.finished[1].outcome);
        };
        assert_eq!(exists.branch, branch_id(RACED).to_string());
        assert!(
            run.finished[1].ended < run.finished[0].ended,
            "steps: {:?}",
            run.trace
        );
        assert!(
            run.finished[0].outcome.is_ok(),
            "got {:?}",
            run.finished[0].outcome
        );
        assert_eq!(
            run.end,
            Model {
                names: BTreeMap::from([(OLD, RACED)]),
                metadata: BTreeMap::from([(RACED, OLD)]),
                ..Model::default()
            }
        );
    }))
    .await;
}

#[tokio::test]
async fn racing_creates_of_a_new_branch_under_two_names_end_as_if_one_at_a_time() {
    race(
        Start::Absent,
        &[create(RACED, BEE), create(RACED, SEA)],
        EVERY_ORDER,
    )
    .await;
}

#[tokio::test]
async fn racing_creates_of_a_new_branch_under_one_name_end_as_if_one_at_a_time() {
    race(
        Start::Absent,
        &[create(RACED, BEE), create(RACED, BEE)],
        EVERY_ORDER,
    )
    .await;
}

#[tokio::test]
async fn racing_creates_of_two_new_branches_under_one_name_end_as_if_one_at_a_time() {
    race(
        Start::Absent,
        &[create(RACED, BEE), create(OTHER, BEE)],
        EVERY_ORDER,
    )
    .await;
}

#[tokio::test]
async fn racing_archive_and_rename_of_a_live_branch_end_as_if_one_at_a_time() {
    race(
        Start::Live,
        &[archive(RACED), create(RACED, BEE)],
        EVERY_ORDER,
    )
    .await;
}

#[tokio::test]
async fn racing_archives_of_a_live_branch_end_as_if_one_at_a_time() {
    race(Start::Live, &[archive(RACED), archive(RACED)], EVERY_ORDER).await;
}

#[tokio::test]
async fn racing_archive_and_restore_under_its_name_of_a_live_branch_end_as_if_one_at_a_time() {
    race(
        Start::Live,
        &[archive(RACED), create(RACED, OLD)],
        EVERY_ORDER,
    )
    .await;
}

#[tokio::test]
async fn racing_renames_of_an_archived_branch_end_as_if_one_at_a_time() {
    race(
        Start::Archived,
        &[create(RACED, BEE), create(RACED, SEA)],
        EVERY_ORDER,
    )
    .await;
}

#[tokio::test]
async fn racing_restore_under_its_name_and_rename_of_an_archived_branch_end_as_if_one_at_a_time() {
    race(
        Start::Archived,
        &[create(RACED, OLD), create(RACED, BEE)],
        EVERY_ORDER,
    )
    .await;
}

#[tokio::test]
async fn racing_restores_under_its_name_of_an_archived_branch_end_as_if_one_at_a_time() {
    race(
        Start::Archived,
        &[create(RACED, OLD), create(RACED, OLD)],
        EVERY_ORDER,
    )
    .await;
}

#[tokio::test]
async fn racing_rename_of_an_archived_branch_and_create_of_another_under_the_name_end_as_if_one_at_a_time()
 {
    race(
        Start::Archived,
        &[create(RACED, BEE), create(OTHER, BEE)],
        EVERY_ORDER,
    )
    .await;
}

#[tokio::test]
async fn racing_rename_and_archive_of_an_archived_branch_end_as_if_one_at_a_time() {
    race(
        Start::Archived,
        &[create(RACED, BEE), archive(RACED)],
        EVERY_ORDER,
    )
    .await;
}

#[tokio::test]
async fn racing_restore_under_its_name_rename_and_archive_of_an_archived_branch_end_as_if_one_at_a_time()
 {
    race(
        Start::Archived,
        &[create(RACED, OLD), create(RACED, BEE), archive(RACED)],
        PREEMPTIONS,
    )
    .await;
}

#[tokio::test]
async fn racing_two_renames_and_archive_of_an_archived_branch_end_as_if_one_at_a_time() {
    race(
        Start::Archived,
        &[create(RACED, BEE), create(RACED, SEA), archive(RACED)],
        PREEMPTIONS,
    )
    .await;
}

#[tokio::test]
async fn racing_two_restores_under_its_name_and_rename_of_an_archived_branch_end_as_if_one_at_a_time()
 {
    race(
        Start::Archived,
        &[create(RACED, OLD), create(RACED, OLD), create(RACED, BEE)],
        PREEMPTIONS,
    )
    .await;
}

#[tokio::test]
async fn racing_two_renames_and_restore_under_its_name_of_an_archived_branch_end_as_if_one_at_a_time()
 {
    race(
        Start::Archived,
        &[create(RACED, BEE), create(RACED, SEA), create(RACED, OLD)],
        PREEMPTIONS,
    )
    .await;
}

#[tokio::test]
async fn racing_creates_of_a_new_branch_under_three_names_end_as_if_one_at_a_time() {
    race(
        Start::Absent,
        &[create(RACED, OLD), create(RACED, BEE), create(RACED, SEA)],
        PREEMPTIONS,
    )
    .await;
}

#[tokio::test]
async fn racing_archive_rename_and_restore_under_its_name_of_a_live_branch_end_as_if_one_at_a_time()
{
    race(
        Start::Live,
        &[archive(RACED), create(RACED, BEE), create(RACED, OLD)],
        PREEMPTIONS,
    )
    .await;
}

#[tokio::test]
async fn racing_three_renames_of_an_archived_branch_under_two_names_end_as_if_one_at_a_time() {
    race(
        Start::Archived,
        &[create(RACED, BEE), create(RACED, SEA), create(RACED, BEE)],
        PREEMPTIONS,
    )
    .await;
}

#[tokio::test]
async fn racing_renames_of_a_branch_with_a_stopped_restore_end_as_if_one_at_a_time() {
    race(
        Start::Stopped,
        &[create(RACED, SEA), create(RACED, OLD)],
        EVERY_ORDER,
    )
    .await;
}

#[tokio::test]
async fn racing_restore_under_the_stopped_restores_name_and_archive_end_as_if_one_at_a_time() {
    race(
        Start::Stopped,
        &[create(RACED, BEE), archive(RACED)],
        EVERY_ORDER,
    )
    .await;
}

#[tokio::test]
async fn racing_rename_and_create_of_another_under_the_stopped_restores_name_end_as_if_one_at_a_time()
 {
    race(
        Start::Stopped,
        &[create(RACED, SEA), create(OTHER, BEE)],
        EVERY_ORDER,
    )
    .await;
}

#[tokio::test]
async fn racing_archives_of_a_branch_with_a_stopped_restore_end_as_if_one_at_a_time() {
    race(
        Start::Stopped,
        &[archive(RACED), archive(RACED)],
        EVERY_ORDER,
    )
    .await;
}

#[tokio::test]
async fn racing_archives_of_a_branch_with_a_stopped_restore_under_its_own_name_end_as_if_one_at_a_time()
 {
    race(
        Start::StoppedUnderOld,
        &[archive(RACED), archive(RACED)],
        EVERY_ORDER,
    )
    .await;
}

#[tokio::test]
async fn racing_two_renames_and_archive_of_a_branch_with_a_stopped_restore_end_as_if_one_at_a_time()
{
    race(
        Start::Stopped,
        &[create(RACED, BEE), create(RACED, SEA), archive(RACED)],
        PREEMPTIONS,
    )
    .await;
}

#[tokio::test]
async fn racing_renames_of_a_branch_a_stopped_settle_left_end_as_if_one_at_a_time() {
    race(
        Start::Released,
        &[create(RACED, SEA), create(RACED, BEE)],
        EVERY_ORDER,
    )
    .await;
}

#[tokio::test]
async fn racing_rename_and_archive_of_a_branch_a_stopped_settle_left_end_as_if_one_at_a_time() {
    race(
        Start::Released,
        &[create(RACED, SEA), archive(RACED)],
        EVERY_ORDER,
    )
    .await;
}

#[tokio::test]
async fn racing_rename_and_create_of_another_under_the_released_name_end_as_if_one_at_a_time() {
    race(
        Start::Released,
        &[create(RACED, SEA), create(OTHER, BEE)],
        EVERY_ORDER,
    )
    .await;
}

#[tokio::test]
async fn racing_two_renames_and_archive_of_a_branch_a_stopped_settle_left_end_as_if_one_at_a_time()
{
    race(
        Start::Released,
        &[create(RACED, BEE), create(RACED, SEA), archive(RACED)],
        PREEMPTIONS,
    )
    .await;
}

#[tokio::test]
async fn racing_restore_and_protect_of_an_archived_branch_end_as_if_one_at_a_time() {
    race(
        Start::Archived,
        &[create(RACED, BEE), Op::Protect { id: RACED }],
        EVERY_ORDER,
    )
    .await;
}

#[tokio::test]
async fn racing_renames_of_a_branch_an_older_version_archived_end_as_if_one_at_a_time() {
    race(
        Start::OlderArchived,
        &[create(RACED, BEE), create(RACED, SEA)],
        EVERY_ORDER,
    )
    .await;
}

#[tokio::test]
async fn racing_restore_and_archive_of_a_branch_an_older_version_archived_end_as_if_one_at_a_time()
{
    race(
        Start::OlderArchived,
        &[create(RACED, OLD), archive(RACED)],
        EVERY_ORDER,
    )
    .await;
}

#[tokio::test]
async fn racing_restore_and_rename_of_a_branch_without_a_latest_revision_end_as_if_one_at_a_time() {
    race(
        Start::OlderArchivedWithoutLatest,
        &[create(RACED, OLD), create(RACED, BEE)],
        EVERY_ORDER,
    )
    .await;
}

/// A restore that stops after it recorded itself or after its claim, as one whose process ends
/// there does, is settled by the next restore or archive of the branch, under any name. A restore
/// succeeds, and so does an archive where the stopped restore had claimed its name; one where it
/// hadn't releases no name and fails with `BranchNotFound`. The stopped restore's name maps to
/// nothing it didn't restore, and no record of it stays.
#[tokio::test]
async fn a_stopped_restore_is_settled_by_the_next_restore_or_archive() {
    Box::pin(with_execution(async {
        for stop in [BranchStep::RestoreRecorded, BranchStep::RestoreNameClaimed] {
            for (op, names) in [
                (create(RACED, BEE), BTreeMap::from([(BEE, RACED)])),
                (create(RACED, SEA), BTreeMap::from([(SEA, RACED)])),
                (create(RACED, OLD), BTreeMap::from([(OLD, RACED)])),
                (archive(RACED), BTreeMap::new()),
            ] {
                let repository = null_repository().await;
                Start::Archived.prepare(&repository).await;
                stop_at(&repository, create(RACED, BEE), stop).await;

                let outcome = op.run(repository.clone()).await;

                if stop == BranchStep::RestoreRecorded && matches!(op, Op::Archive { .. }) {
                    assert!(
                        outcome.as_ref().is_err_and(|err| err.is_branch_not_found()),
                        "{stop:?} {op:?}: got {outcome:?}"
                    );
                } else {
                    assert!(outcome.is_ok(), "{stop:?} {op:?}: got {outcome:?}");
                }
                let end = Model::read(&repository).await;
                assert_eq!(end.names, names, "{stop:?} {op:?}");
                assert_eq!(end.pending, None, "{stop:?} {op:?}");
            }
        }
    }))
    .await;
}

/// An archive that looked up a name a failed restore had claimed, which the restore then released,
/// swaps the metadata, and a restore under the name, or under the name in another case, that reads
/// the archived metadata claims it and finishes before the archive releases it: the archive leaves
/// the name to that restore, and both succeed, the branch live under the name, as an archive
/// followed by the restore leaves it.
#[tokio::test]
async fn an_archive_leaves_the_name_to_a_restore_that_claimed_it_after_its_swap() {
    use BranchStep::*;
    Box::pin(with_execution(async {
        for name in [OLD, "Old"] {
            let repository = null_repository().await;
            Start::Archived.prepare(&repository).await;
            let mut failed = Racer::new(create(RACED, OLD));
            advance_to(&mut failed, &repository, RestoreNameClaimed).await;
            let mut restore = Racer::new(create(RACED, name));
            advance_to(&mut restore, &repository, PendingRestoreSettled).await;
            let mut archiver = Racer::new(archive(RACED));
            advance_to(&mut archiver, &repository, ArchiveLookedUp).await;
            let outcome = finish(&mut failed, &repository).await;
            assert!(
                outcome
                    .as_ref()
                    .is_err_and(|err| err.is_branch_already_exists()),
                "{name}: got {outcome:?}"
            );
            advance_to(&mut restore, &repository, RestoreChecked).await;
            advance_to(&mut archiver, &repository, ArchiveReleasing).await;

            let restored = finish(&mut restore, &repository).await;
            let archived = finish(&mut archiver, &repository).await;

            assert!(restored.is_ok(), "{name}: got {restored:?}");
            assert!(archived.is_ok(), "{name}: got {archived:?}");
            assert_eq!(
                mapped(&repository, name).await,
                Some(branch_id(RACED)),
                "{name}"
            );
            assert!(
                stamps(&repository, RACED)
                    .await
                    .is_some_and(|stamps| stamps.restored),
                "{name}: the branch is restored"
            );
        }
    }))
    .await;
}

/// A restore under a name in one case that records itself and stalls, while an archive settles it
/// and a restore under the name in another case restores the branch, finds its final swap refused
/// and leaves the name, which maps to the branch through the same key, to the restore that holds
/// it.
#[tokio::test]
async fn a_failed_restore_leaves_its_name_to_a_restore_under_it_in_another_case() {
    use BranchStep::*;
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        Start::Archived.prepare(&repository).await;
        let mut slow = Racer::new(create(RACED, "Bee"));
        advance_to(&mut slow, &repository, RestoreRecorded).await;
        settle_unclaimed_restore(&repository).await;
        create(RACED, BEE)
            .run(repository.clone())
            .await
            .expect("the branch is restored");

        let outcome = finish(&mut slow, &repository).await;

        assert!(
            outcome
                .as_ref()
                .is_err_and(|err| err.is_branch_already_exists()),
            "got {outcome:?}"
        );
        assert_eq!(mapped(&repository, BEE).await, Some(branch_id(RACED)));
    }))
    .await;
}

/// An archive of a branch whose metadata marks its own name released in another case, as a stopped
/// settle of a restore under that case leaves it while the name maps to the branch, archives it as
/// a branch its name maps to, rather than releasing the name first and failing with
/// `BranchNotFound`.
#[tokio::test]
async fn an_archive_reads_a_released_name_in_another_case_as_its_own() {
    use BranchStep::*;
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        create(RACED, BEE)
            .run(repository.clone())
            .await
            .expect("the branch is created");
        archive(RACED)
            .run(repository.clone())
            .await
            .expect("the branch is archived");
        stop_at(&repository, create(RACED, "Bee"), RestoreNameClaimed).await;
        stop_at(&repository, archive(RACED), PendingRestoreSettled).await;
        assert_eq!(mapped(&repository, BEE).await, Some(branch_id(RACED)));

        let outcome = archive(RACED).run(repository.clone()).await;

        assert!(outcome.is_ok(), "got {outcome:?}");
        assert_eq!(mapped(&repository, BEE).await, None);
    }))
    .await;
}

/// A protect that reads a restored branch's metadata before an archive and swaps it after finds
/// the metadata changed and reads it again: the branch ends archived and protected, where a store
/// of the metadata it read first would say the branch is live again.
#[tokio::test]
async fn a_protect_racing_an_archive_keeps_the_archive() {
    Box::pin(with_execution(async {
        let repository = null_repository().await;
        Start::Archived.prepare(&repository).await;
        create(RACED, OLD)
            .run(repository.clone())
            .await
            .expect("the branch is restored");
        let mut protect = Racer::new(Op::Protect { id: RACED });
        assert!(matches!(
            protect.advance(&repository).await,
            Advanced::Stopped(BranchStep::ProtectMetadataRead)
        ));

        archive(RACED)
            .run(repository.clone())
            .await
            .expect("the branch is archived");
        let outcome = loop {
            match protect.advance(&repository).await {
                Advanced::Stopped(_) => {}
                Advanced::Ended(outcome) => break outcome,
            }
        };

        assert!(outcome.is_ok(), "got {outcome:?}");
        let metadata = branch::metadata(repository.clone(), branch_id(RACED))
            .await
            .expect("the branch has metadata");
        assert_eq!(metadata.get_bool(branch::PROTECT).ok(), Some(true));
        assert_eq!(
            stamps(&repository, RACED).await,
            Some(Stamps {
                archived: true,
                restored: false,
                restoring: None,
                released: None,
            })
        );
        assert_eq!(mapped(&repository, OLD).await, None);
    }))
    .await;
}

/// A restore under a name that stopped after its claim, a settle of it that stopped before its
/// release, or a restore that claimed after its settle and stopped, as processes that end there
/// do, leaves the name mapped to the branch. A create of another branch under the name then fails
/// with `BranchAlreadyExists`, and a forced create takes the name over: it settles the stopped
/// restore, or releases the name a settle left marked released without archiving the branch, so
/// the name maps to the new branch and the branch stays archived under its old name, with no
/// record of the restore and the name marked released.
#[tokio::test]
async fn a_forced_create_takes_over_the_name_a_stopped_restore_left() {
    Box::pin(LORE_CONTEXT.scope(setup_test_execution(), async {
        for left in [Stop::Restore, Stop::Settle, Stop::LateClaim] {
            let (repository, _tempdir) = committed_repository("", &[]).await;
            let (latest, main) = lore_revision::instance::load_current_anchor_boxed(&repository)
                .await
                .expect("the committed revision is current");
            create_on(
                &repository,
                branch_id(RACED),
                OLD,
                vec![BranchPoint {
                    branch: main,
                    revision: latest,
                }],
            )
            .await
            .expect("an unclaimed name is created");
            branch::delete(repository.clone(), branch_id(RACED))
                .await
                .expect("the branch is archived");
            leave_bee_mapped(&repository, left).await;

            let unforced = create_branch_on_instance(&repository, BEE, None, "", false).await;
            assert!(
                unforced
                    .as_ref()
                    .is_err_and(|err| err.is_branch_already_exists()),
                "{left:?}: got {unforced:?}"
            );
            create_branch_on_instance(&repository, BEE, None, "", true)
                .await
                .unwrap_or_else(|err| {
                    panic!("{left:?}: a forced create takes the name over: {err:?}")
                });

            let holder = mapped(&repository, BEE).await;
            assert!(
                holder.is_some_and(|holder| holder != branch_id(RACED)),
                "{left:?}: got {holder:?}"
            );
            assert_eq!(
                metadata_name(&repository, RACED).await,
                Some(OLD),
                "{left:?}"
            );
            assert_eq!(
                stamps(&repository, RACED).await,
                Some(Stamps {
                    archived: true,
                    restored: false,
                    restoring: None,
                    released: Some(BEE),
                }),
                "{left:?}"
            );
        }
    }))
    .await;
}

/// A restore under a name that records itself and stalls, while an archive settles it and a
/// restore under another name releases the marked name and restores the branch, then claims the
/// name and stops, leaves the name mapped to the branch with nothing in its metadata naming it. A
/// forced create of the name releases that mapping and takes the name, and leaves the branch live
/// under its other name, where archiving it would archive the live branch.
#[tokio::test]
async fn a_forced_create_takes_over_a_name_the_live_branch_holds_no_record_of() {
    use BranchStep::*;
    Box::pin(LORE_CONTEXT.scope(setup_test_execution(), async {
        let (repository, _tempdir) = committed_repository("", &[]).await;
        let (latest, main) = lore_revision::instance::load_current_anchor_boxed(&repository)
            .await
            .expect("the committed revision is current");
        create_on(
            &repository,
            branch_id(RACED),
            OLD,
            vec![BranchPoint {
                branch: main,
                revision: latest,
            }],
        )
        .await
        .expect("an unclaimed name is created");
        branch::delete(repository.clone(), branch_id(RACED))
            .await
            .expect("the branch is archived");
        let mut stalled = Racer::new(create(RACED, BEE));
        advance_to(&mut stalled, &repository, RestoreRecorded).await;
        settle_unclaimed_restore(&repository).await;
        create(RACED, SEA)
            .run(repository.clone())
            .await
            .expect("the branch is restored under another name");
        advance_to(&mut stalled, &repository, RestoreNameClaimed).await;
        drop(stalled);
        assert_eq!(mapped(&repository, BEE).await, Some(branch_id(RACED)));

        create_branch_on_instance(&repository, BEE, None, "", true)
            .await
            .expect("a forced create takes the name over");

        let holder = mapped(&repository, BEE).await;
        assert!(
            holder.is_some_and(|holder| holder != branch_id(RACED)),
            "got {holder:?}"
        );
        assert_eq!(mapped(&repository, SEA).await, Some(branch_id(RACED)));
        assert!(
            stamps(&repository, RACED)
                .await
                .is_some_and(|stamps| stamps.restored),
            "the branch stays live"
        );
    }))
    .await;
}

/// A forced create of a name in another case than the one its holder's metadata names, or than the
/// one a stopped restore of the holder records, reads the holder as holding it, as the name's
/// mapping key does: it archives a live holder, and settles the stopped restore, rather than
/// release the mapping alone, so no branch is left live without its name or with a restore record.
#[tokio::test]
async fn a_forced_create_reads_a_name_in_another_case_as_its_holders() {
    Box::pin(LORE_CONTEXT.scope(setup_test_execution(), async {
        for stopped in [false, true] {
            let (repository, _tempdir) = committed_repository("", &[]).await;
            let (latest, main) = lore_revision::instance::load_current_anchor_boxed(&repository)
                .await
                .expect("the committed revision is current");
            let held = if stopped { OLD } else { "Bee" };
            create_on(
                &repository,
                branch_id(RACED),
                held,
                vec![BranchPoint {
                    branch: main,
                    revision: latest,
                }],
            )
            .await
            .expect("an unclaimed name is created");
            if stopped {
                branch::delete(repository.clone(), branch_id(RACED))
                    .await
                    .expect("the branch is archived");
                stop_at(
                    &repository,
                    create(RACED, "Bee"),
                    BranchStep::RestoreNameClaimed,
                )
                .await;
            }
            assert_eq!(
                mapped(&repository, BEE).await,
                Some(branch_id(RACED)),
                "{stopped}"
            );

            create_branch_on_instance(&repository, BEE, None, "", true)
                .await
                .unwrap_or_else(|err| {
                    panic!("{stopped}: a forced create takes the name over: {err:?}")
                });

            let holder = mapped(&repository, BEE).await;
            assert!(
                holder.is_some_and(|holder| holder != branch_id(RACED)),
                "{stopped}: got {holder:?}"
            );
            let metadata = branch::metadata(repository.clone(), branch_id(RACED))
                .await
                .expect("the old holder keeps its metadata");
            assert!(
                metadata.get_u64(branch::ARCHIVED).is_ok()
                    && metadata.get_u64(branch::RESTORED).is_err(),
                "{stopped}: the old holder is archived"
            );
            assert!(
                metadata.get_string(branch::RESTORING).is_err(),
                "{stopped}: no restore record stays"
            );
        }
    }))
    .await;
}

/// A settle that stopped before its release, or a restore that claimed after its settle and then
/// stopped, leaves its name mapped to the archived branch and marked released in its metadata. The
/// next restore of the branch, under any name, releases it and removes the mark, and succeeds; the
/// next archive releases it and keeps the mark, and fails with `BranchNotFound`, as the branch is
/// archived. Either way no name but the restored one maps to the branch.
#[tokio::test]
async fn a_name_a_stopped_settle_left_is_released_by_the_next_restore_or_archive() {
    Box::pin(with_execution(async {
        for left in [Stop::Settle, Stop::LateClaim] {
            for (op, names, released) in [
                (create(RACED, BEE), BTreeMap::from([(BEE, RACED)]), None),
                (create(RACED, SEA), BTreeMap::from([(SEA, RACED)]), None),
                (create(RACED, OLD), BTreeMap::from([(OLD, RACED)]), None),
                (archive(RACED), BTreeMap::new(), Some(BEE)),
            ] {
                let repository = null_repository().await;
                Start::Archived.prepare(&repository).await;
                leave_bee_mapped(&repository, left).await;

                let outcome = op.run(repository.clone()).await;

                match op {
                    Op::Archive { .. } => assert!(
                        outcome.as_ref().is_err_and(|err| err.is_branch_not_found()),
                        "{left:?} {op:?}: got {outcome:?}"
                    ),
                    _ => assert!(outcome.is_ok(), "{left:?} {op:?}: got {outcome:?}"),
                }
                let end = Model::read(&repository).await;
                assert_eq!(end.names, names, "{left:?} {op:?}");
                assert_eq!(
                    stamps(&repository, RACED)
                        .await
                        .and_then(|stamps| stamps.released),
                    released,
                    "{left:?} {op:?}"
                );
            }
        }
    }))
    .await;
}

/// A dry run of a restore under a new name, where a stopped restore claimed the branch's old name,
/// or a stopped settle of it left that name marked released, succeeds as the restore does, and
/// changes nothing.
#[tokio::test]
async fn a_dry_run_rename_over_a_stopped_restore_of_the_old_name_succeeds() {
    use BranchStep::*;
    Box::pin(with_execution(async {
        for settle_stopped in [false, true] {
            let repository = null_repository().await;
            Start::Archived.prepare(&repository).await;
            stop_at(&repository, create(RACED, OLD), RestoreNameClaimed).await;
            if settle_stopped {
                stop_at(&repository, archive(RACED), PendingRestoreSettled).await;
            }
            assert_eq!(mapped(&repository, OLD).await, Some(branch_id(RACED)));
            let before = Model::read(&repository).await;

            let outcome = branch::create(
                repository.clone(),
                &in_memory::token(),
                branch_id(RACED),
                BEE,
                branch::default_category(),
                "creator",
                1,
                vec![branch_point(9, 9)],
                true,
                false,
            )
            .await;

            assert!(outcome.is_ok(), "{settle_stopped}: got {outcome:?}");
            assert_eq!(Model::read(&repository).await, before, "{settle_stopped}");
        }
    }))
    .await;
}

/// A dry run of a restore under the name a stopped restore claimed, or one a stopped settle left
/// marked released, succeeds as the restore does, and changes nothing.
#[tokio::test]
async fn a_dry_run_restore_under_a_stopped_restores_name_succeeds() {
    Box::pin(with_execution(async {
        for left in [Stop::Restore, Stop::Settle] {
            let repository = null_repository().await;
            Start::Archived.prepare(&repository).await;
            leave_bee_mapped(&repository, left).await;
            let before = Model::read(&repository).await;

            let outcome = branch::create(
                repository.clone(),
                &in_memory::token(),
                branch_id(RACED),
                BEE,
                branch::default_category(),
                "creator",
                1,
                vec![branch_point(9, 9)],
                true,
                false,
            )
            .await;

            assert!(outcome.is_ok(), "{left:?}: got {outcome:?}");
            assert_eq!(Model::read(&repository).await, before, "{left:?}");
        }
    }))
    .await;
}
