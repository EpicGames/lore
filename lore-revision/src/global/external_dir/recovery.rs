// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The steps of a move of the external root, each with the check that finds its effect already on
//! disk, the direction recovery takes an interrupted move, and the leftover list.
//!
//! A move records each phase before it runs the phase's step, and runs the steps after it in
//! order. A step first checks whether its effect is on disk, so a step a crash interrupted runs
//! again, and one it finished runs no second time. `from` and `to` are the record's roots, the
//! default location where empty, and *the target* is `to`.
//!
//! | Kind | Phase | Step | Done when |
//! |---|---|---|---|
//! | `rename` | `mark` | Checks that the user owns the current root, on Unix, sets its mode to `0o700`, and writes a marker of a new root ID and generation 1 into it. First move only. | The current root holds a marker. |
//! | `rename` | `identify` | Writes the setting: the current root, with its marker's root ID and generation. First move only. | The setting holds the marker's root ID. |
//! | `rename` | `place` | To the default location, first removes the command's redirect there. Sets the current root's mode to `0o700`, on Unix, makes the target's missing parents, with access for the user only, and renames the root to the target. | The target holds the setting's marker. |
//! | `copy` | `copy` | Makes the target's missing parents and a temporary directory beside it, with access for the user only, removing one an earlier attempt left. Sets the current root's mode to `0o700`, on Unix, so the copy takes it, and copies the current root into it faithfully, leaving out its marker and the contents of each `write-staging/`, then writes the setting's marker into it. | The temporary directory or the target holds the setting's marker. |
//! | `copy` | `place` | To the default location, first removes the command's redirect there. Renames the temporary directory to the target. | The target holds the setting's marker. |
//! | `create` | `place` | Makes the target's missing parents, then the target at a temporary name beside it, with a marker of a new root ID and generation 1, and renames it to the target. | The target holds a marker. |
//! | `abandon` | `place` | To the default location, first removes the command's redirect there. Then as `create`, with a marker of the setting's root ID and the next generation. | The target holds a marker of the setting's root ID and the next generation. |
//! | every kind | `switch` | Takes the target off the leftover list, then writes the setting: the target, with its marker's root ID and generation. | The setting holds them, and the list doesn't name the target. |
//! | `copy` | `unmark` | Deletes the marker of the current root, `from`, durably. A root that isn't there, as where its volume is gone, is listed as a leftover in the leftover list's `marked`, so its marker and contents go when it returns. | `from` holds no marker, or is listed. |
//! | `copy` | `set-aside` | Lists an aside name beside `from`, with a random suffix, in the leftover list's `paths`, and renames `from` to it, durably. | No directory is at `from`. |
//! | every kind | `redirect` | Makes the default location match the setting: a redirect to a configured root. A setting that names the default location leaves it as it is. | What [`redirect_to`] finds. |
//! | every kind | (last) | Removes the record. | The record is absent. |
//!
//! `place` through `redirect`, and `switch` and `redirect` for `adopt`, form the final switch,
//! which runs under the store locks.
//!
//! Undo runs fixed steps, also each with its check, for `rename`, `copy` and `create`. *What the
//! move made* is the temporary directory beside the target, for `copy` and `create`, and the
//! target where it holds a marker the move placed there, a marker of the setting for `rename` and
//! `copy` and any for `create`, or where the leftover list names an aside name for it that is
//! still free.
//!
//! | [`UndoStep`] | Step | Done when |
//! |---|---|---|
//! | `Unmark` | Deletes the marker of what the move made, durably. | It holds no marker. |
//! | `List` | Lists an aside name beside it, with a random suffix, in the leftover list's `paths`. | The list names a free aside name for it. |
//! | `SetAside` | Renames it to that name, durably. | Nothing is at its path. |
//! | `Redirect` | As the `redirect` phase, for the setting. | As the `redirect` phase. |
//! | `Forget` | Removes the record. | The record is absent. |
//!
//! [`direction`] picks Finish, Undo, Wait or Refuse from the setting, the roots available and the
//! kind, by the table of the LEP:
//!
//! | Setting names | Available | Direction |
//! |---|---|---|
//! | `from` | `from` | Undo |
//! | `from` | `to` only, after a `rename` or a `create` | Finish |
//! | `to` | `to` | Finish |
//! | `from` or `to` | none of the above | Wait |
//! | neither | any | Refuse |
//! | any | any, for `adopt` and `abandon` | Finish |
//!
//! With no setting, the setting names the default location, which is then available while it is
//! missing or a directory: so a `create` before its `switch`, and a first `rename` before its
//! `identify`, are undone. An `adopt` whose root has gone can't finish: recovery fails on its
//! record, which only a takeover replaces.

use std::num::NonZeroU64;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lore_error_set::prelude::*;
use lore_io::clone::Kind;

use super::ExternalDirError;
use super::leftovers::LEFTOVERS;
use super::leftovers::Leftovers;
use super::locks::Joining;
use super::locks::LOCK_POLL;
use super::locks::LocksError;
use super::locks::Taken;
use super::locks::lock_stores_until;
use super::locks::store_set;
use super::locks::try_lock_stores;
use super::marker::MARKER;
use super::marker::Marker;
use super::record::MOVE_RECORD;
use super::record::MoveKind;
use super::record::MovePhase;
use super::record::MoveRecord;
use super::redirect::Redirect;
use super::redirect::redirect_to;
use super::resolve::root_available;
use super::resolve::root_path;
use super::setting::RootId;
use super::setting::Setting;
use crate::fs::walk::Progress;
use crate::fs::walk::Visit;
use crate::util::config;
use crate::util::in_use::rename_is_retryable;
use crate::util::in_use::retry_while_in_use;

/// Stops the move at the [`crate::test_util::MoveStep`] `$variant`, where a test runs it
/// under `test_util::step_moves`, and fails it where the test says. Nothing without `test-util`.
macro_rules! stop_at {
    ($variant:ident) => {
        stop_at!(@step crate::test_util::MoveStep::$variant)
    };
    ($variant:ident($arg:expr)) => {
        stop_at!(@step crate::test_util::MoveStep::$variant($arg))
    };
    (@step $step:expr) => {
        #[cfg(feature = "test-util")]
        {
            let step = $step;
            if !crate::test_util::move_step(step).await {
                return Err(ExternalDirError::internal(format!(
                    "a test failed the move at {step:?}"
                )));
            }
        }
    };
}

/// The suffix of the temporary name beside a target that a new root is made at before it is
/// placed.
const PLACING_SUFFIX: &str = ".lore-placing";
/// The suffix of the temporary name beside a target that a move by copy copies the root into.
const COPYING_SUFFIX: &str = ".lore-copying";
/// The suffix of an aside name, before its random part.
const ASIDE_SUFFIX: &str = ".lore-aside-";

/// Which way recovery takes an interrupted move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Runs the step the phase names and the steps after it.
    Finish,
    /// Runs the fixed steps that undo the move.
    Undo,
    /// Changes nothing, and keeps the record for a later recovery.
    Wait,
    /// Changes nothing, and fails.
    Refuse,
}

/// Which root of a move the setting names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Named {
    /// The root the move leaves.
    From,
    /// The root the move goes to.
    To,
    /// Neither.
    Neither,
}

/// A step of an undo, where [`crate::test_util::MoveStep::Undo`] stops it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UndoStep {
    /// Deletes the marker of what the move made.
    Unmark,
    /// Lists its aside name in the leftover list.
    List,
    /// Renames it to its aside name.
    SetAside,
    /// Makes the redirect match the setting.
    Redirect,
    /// Removes the record.
    Forget,
}

/// An interrupted move, as recovery reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assessment {
    /// The move record.
    pub record: MoveRecord,
    /// Which root the setting names.
    pub named: Named,
    /// Whether the root the move leaves is available.
    pub from_available: bool,
    /// Whether the root the move goes to is available.
    pub to_available: bool,
    /// Whether the move is an adopt whose root has gone, which recovery can't finish.
    pub adopted_root_gone: bool,
    /// The way recovery takes the move.
    pub direction: Direction,
}

/// The direction the table in the module's documentation gives a move of `kind`, where the
/// setting names `named` and the roots available are as `from_available` and `to_available` say.
pub fn direction(
    kind: MoveKind,
    named: Named,
    from_available: bool,
    to_available: bool,
) -> Direction {
    match (kind, named) {
        (MoveKind::Adopt | MoveKind::Abandon, _) => Direction::Finish,
        (_, Named::Neither) => Direction::Refuse,
        (_, Named::From) if from_available => Direction::Undo,
        (MoveKind::Rename | MoveKind::Create, Named::From) | (_, Named::To) if to_available => {
            Direction::Finish
        }
        _ => Direction::Wait,
    }
}

/// The interrupted move of the global data directory `global`, read from disk, or `None` where no
/// move is recorded. Blocking.
pub fn assess(global: &Path) -> Result<Option<Assessment>, ExternalDirError> {
    let Some(record) = MoveRecord::load(global)? else {
        return Ok(None);
    };
    let setting = Setting::load(global)?;
    let named = named(global, setting.as_ref(), &record);
    let from_available = root_available(global, &record.from, setting.as_ref());
    let to_available = root_available(global, &record.to, setting.as_ref());
    let adopted_root_gone =
        record.kind == MoveKind::Adopt && is_gone(&root_path(global, &record.to));
    Ok(Some(Assessment {
        direction: direction(record.kind, named, from_available, to_available),
        record,
        named,
        from_available,
        to_available,
        adopted_root_gone,
    }))
}

/// Whether nothing is at `path` and its nearest existing ancestor is a directory. A path below a
/// file, which Windows answers as missing and Unix as no directory, is there, as is one that fails
/// to be examined for any other reason. Blocking.
fn is_gone(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        _ => return false,
    }
    for ancestor in path.ancestors().skip(1) {
        match std::fs::metadata(ancestor) {
            Ok(metadata) => return metadata.is_dir(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return false,
        }
    }
    false
}

/// The failure of recovery of the adopt `record` of `global`, whose root has gone: it names the
/// record, the root, and the forms of the command that replace the record.
fn adopted_root_gone(global: &Path, record: &MoveRecord) -> String {
    format!(
        "{} records an adopt of {}, which has gone, so the adopt can't finish: replace it with \
         `lore service set-external-directory <path> --adopt` or `--abandon`",
        global.join(MOVE_RECORD).display(),
        root_path(global, &record.to).display()
    )
}

/// Which root of `record` the setting `setting` of `global` names, the default location where
/// there is none.
fn named(global: &Path, setting: Option<&Setting>, record: &MoveRecord) -> Named {
    let named = root_path(global, setting.map_or("", |setting| setting.path.as_str()));
    if named == root_path(global, &record.from) {
        Named::From
    } else if named == root_path(global, &record.to) {
        Named::To
    } else {
        Named::Neither
    }
}

/// The phases of a move of `kind`, in order.
pub fn phases(kind: MoveKind) -> &'static [MovePhase] {
    use MovePhase::Copy;
    use MovePhase::Identify;
    use MovePhase::Mark;
    use MovePhase::Place;
    use MovePhase::Redirect;
    use MovePhase::SetAside;
    use MovePhase::Switch;
    use MovePhase::Unmark;
    match kind {
        MoveKind::Rename => &[Mark, Identify, Place, Switch, Redirect],
        MoveKind::Copy => &[
            Mark, Identify, Copy, Place, Switch, Unmark, SetAside, Redirect,
        ],
        MoveKind::Create | MoveKind::Abandon => &[Place, Switch, Redirect],
        MoveKind::Adopt => &[Switch, Redirect],
    }
}

/// Runs the step `record`'s phase names and the steps after it, writing each phase into the
/// record in `global` before its step runs, then removes the record. Answers what the default
/// location holds once the redirect matches the setting.
pub async fn finish(global: &Path, record: &mut MoveRecord) -> Result<Redirect, ExternalDirError> {
    finish_with(global, record, Renames::RetriedWhileInUse).await
}

/// How a move's `copy` step runs: the progress that counts the copy and stops it, and what it
/// tells the name of each external directory as the copy reaches it.
#[lore_macro::test_pub]
#[derive(Clone)]
pub(super) struct CopyRun {
    /// Counts the copy, and stops it once stopped.
    pub(super) progress: Arc<Progress>,
    /// Told the name of each external directory as the copy reaches it.
    pub(super) on_copying: Arc<dyn Fn(&str) + Send + Sync>,
}

tokio::task_local! {
    /// How the `copy` step of a move run under [`copying_as`] runs.
    static COPY_RUN: Option<CopyRun>;
}

/// Runs `future`, whose `copy` step runs as `run` says. A `copy` step run with no `run`, or
/// outside it, is stopped by no one and reports nothing. A task-local, so that the futures of the
/// steps every other move runs hold nothing of it.
pub(super) fn copying_as<F: Future>(
    run: Option<CopyRun>,
    future: F,
) -> impl Future<Output = F::Output> {
    COPY_RUN.scope(run, future)
}

/// How a step renames a root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Renames {
    /// Reissued while files in it are in use, for about fifteen seconds.
    RetriedWhileInUse,
    /// Tried once, as a recovery that must not hold up a service start does.
    TriedOnce,
}

/// [`finish`], with a root renamed as `renames` says.
async fn finish_with(
    global: &Path,
    record: &mut MoveRecord,
    renames: Renames,
) -> Result<Redirect, ExternalDirError> {
    let redirect = phases_from(global, record, None, renames).await?;
    forget(global).await?;
    Ok(redirect)
}

/// Runs the step `record`'s phase names and the steps after it, as [`finish`] does, up to the
/// phase `before`, which is written into the record and not run, and keeps the record. Answers
/// what the default location holds where the `redirect` phase ran.
pub async fn run_phases(
    global: &Path,
    record: &mut MoveRecord,
    before: Option<MovePhase>,
) -> Result<Redirect, ExternalDirError> {
    phases_from(global, record, before, Renames::RetriedWhileInUse).await
}

/// [`run_phases`], with a root renamed as `renames` says.
async fn phases_from(
    global: &Path,
    record: &mut MoveRecord,
    before: Option<MovePhase>,
    renames: Renames,
) -> Result<Redirect, ExternalDirError> {
    let all = phases(record.kind);
    let Some(start) = all.iter().position(|phase| *phase == record.phase) else {
        return Err(ExternalDirError::internal(format!(
            "the external root move record names a phase its kind has not: {:?}",
            record.phase
        )));
    };
    let mut redirect = Redirect::Matched;
    for phase in &all[start..] {
        if record.phase != *phase {
            record.phase = *phase;
            record.save(global).await?;
        }
        if before == Some(*phase) {
            break;
        }
        stop_at!(Phase(*phase));
        if let Some(found) = run_phase(global, record, *phase, renames).await? {
            redirect = found;
        }
    }
    Ok(redirect)
}

/// What [`recover`] did with the move recorded in a global data directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recovered {
    /// No move is recorded.
    Nothing,
    /// The move finished, and the default location holds this.
    Finished(Redirect),
    /// The move was undone, and the default location holds this.
    Undone(Redirect),
    /// The move waits, and its record stays, for this reason.
    Waiting(Waiting),
}

/// Why an interrupted move waits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Waiting {
    /// Neither root is available as the table needs.
    Unavailable,
    /// The lock of this shared store is held.
    StoreHeld(PathBuf),
}

/// How [`recover`] takes the store locks.
pub enum Locking<'a> {
    /// As [`super::locks::lock_stores`] takes them, naming each held store to the function and starting over,
    /// with a root's rename reissued while files in it are in use, as a move does.
    Restarting(&'a mut (dyn FnMut(&Path) + Send)),
    /// Each lock polled for up to this long, a zero one tried once, with a root's rename tried
    /// once: a held store leaves the move waiting, as recovery in the service does.
    Polling(Duration),
}

/// Finishes or undoes the move recorded in `global`, as [`direction`] picks, under the locks of
/// the stores [`store_root`] names, taken as `joining` and `locking` say. Fails, changing nothing,
/// where the table refuses, and where `cancel` completes while [`Locking::Restarting`] waits for a
/// held lock, as [`lock_stores_until`] takes them; keeps the record where a step fails.
pub async fn recover(
    global: &Path,
    joining: Joining,
    locking: Locking<'_>,
    cancel: impl Future<Output = ()>,
) -> Result<Recovered, LocksError> {
    let assessed = global.to_path_buf();
    let Some(mut assessment) = lore_base::lore_spawn_blocking!(move || assess(&assessed))
        .await
        .internal("reading the external root move record")?
        .forward::<LocksError>("reading the external root move record")?
    else {
        return Ok(Recovered::Nothing);
    };
    if assessment.adopted_root_gone {
        return Err(LocksError::internal(adopted_root_gone(
            global,
            &assessment.record,
        )));
    }
    let record = &mut assessment.record;
    let direction = assessment.direction;
    match direction {
        Direction::Refuse => {
            return Err(LocksError::internal(format!(
                "a move of the external root from {} to {} is interrupted, and external-root \
                 names neither root",
                root_path(global, &record.from).display(),
                root_path(global, &record.to).display()
            )));
        }
        Direction::Wait => return Ok(Recovered::Waiting(Waiting::Unavailable)),
        Direction::Finish | Direction::Undo => {}
    }
    let stores = match store_root(global, record, assessment.to_available) {
        Some(root) => store_set(&root).await?,
        None => Vec::new(),
    };
    let renames = match locking {
        Locking::Restarting(_) => Renames::RetriedWhileInUse,
        Locking::Polling(_) => Renames::TriedOnce,
    };
    let _locks = match locking {
        Locking::Restarting(on_held) => {
            match lock_stores_until(&stores, joining, on_held, cancel).await? {
                Some(locks) => Ok(locks),
                None => {
                    return Err(LocksError::internal(
                        "the recovery of an interrupted move of the external root was cancelled \
                     while it waited for a shared store, and the move stays recorded",
                    ));
                }
            }
        }
        Locking::Polling(poll) => match try_lock_stores(&stores, joining, poll).await {
            Ok(Taken::Locked(locks)) => Ok(locks),
            Ok(Taken::Held(store)) => return Ok(Recovered::Waiting(Waiting::StoreHeld(store))),
            Err(err) => Err(err),
        },
    }?;
    let recovered = match direction {
        Direction::Undo => undo(global, record).await.map(Recovered::Undone),
        _ => finish_with(global, record, renames)
            .await
            .map(Recovered::Finished),
    };
    recovered.forward::<LocksError>("recovering a move of the external root")
}

/// How long recovery in the background waits between two attempts.
pub const RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// What recovery at a service start leaves for later.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtStart {
    /// No move is recorded.
    NoMove,
    /// The move finished or was undone, and this process holds shared access to the root.
    Recovered,
    /// The move waits for a held store, for exclusive access another process holds, or for a
    /// step that failed, such as a rename refused while files are in use, and this process holds
    /// exclusive access to the root for the next attempt.
    Retry,
    /// The move waits for a root that isn't available, or its recovery refuses, until the next
    /// service start.
    NextStart,
}

/// Recovery in the service, for the global data directory `global`: exclusive access taken into
/// this process's entry without waiting, each store lock polled for up to `poll`, joining this
/// process's holds, one attempt at each rename, and exclusive access released into shared access
/// once the move is finished or undone. A service start passes a zero `poll`, which makes these
/// its quick steps. Failures are logged, never returned.
pub async fn recover_at_start(global: &Path, poll: Duration) -> AtStart {
    let (at, report) = attempt_at_start(global, poll).await;
    if let Some(report) = &report {
        report.log();
    }
    at
}

/// What an attempt of recovery in the service logs: a warning or an informational message.
#[derive(Debug, PartialEq, Eq)]
struct Report {
    warning: bool,
    message: String,
}

impl Report {
    fn warn(message: String) -> Option<Report> {
        Some(Report {
            warning: true,
            message,
        })
    }

    fn info(message: String) -> Option<Report> {
        Some(Report {
            warning: false,
            message,
        })
    }

    fn log(&self) {
        if self.warning {
            crate::lore_warn!("{}", self.message);
        } else {
            crate::lore_info!("{}", self.message);
        }
    }
}

/// [`recover_at_start`], answering what it logs rather than logging it.
async fn attempt_at_start(global: &Path, poll: Duration) -> (AtStart, Option<Report>) {
    let examined = global.to_path_buf();
    let read = lore_base::lore_spawn_blocking!(
        move || MoveRecord::load(&examined).map(|record| record.is_some())
    )
    .await;
    match read {
        Ok(Ok(false)) => return (AtStart::NoMove, None),
        Ok(Ok(true)) => {}
        Ok(Err(err)) => {
            let report = Report::warn(format!(
                "Not recovering the move of the external root: {err}"
            ));
            return (AtStart::NextStart, report);
        }
        Err(err) => {
            let report = Report::warn(format!(
                "Not recovering the move of the external root: {err}"
            ));
            return (AtStart::Retry, report);
        }
    }
    match access_blocking(global, super::access::try_exclusive).await {
        Ok(true) => {}
        Ok(false) => {
            let report = Report::info(
                "The move of the external root waits for exclusive access to it, which another \
                 holder has"
                    .to_string(),
            );
            return (AtStart::Retry, report);
        }
        Err(err) => {
            let report = Report::warn(format!(
                "Not recovering the move of the external root yet: {err}"
            ));
            return (AtStart::Retry, report);
        }
    }
    match recover(
        global,
        Joining::OwnProcess,
        Locking::Polling(poll),
        std::future::pending(),
    )
    .await
    {
        Ok(Recovered::Nothing) => {
            release_into_shared(global).await;
            (AtStart::NoMove, None)
        }
        Ok(Recovered::Finished(redirect) | Recovered::Undone(redirect)) => {
            let report = match redirect {
                Redirect::Kept(occupant) => Report::warn(format!(
                    "The default location of the external root holds what the move didn't make, \
                     and is kept: {occupant:?}"
                )),
                Redirect::Matched => None,
            };
            release_into_shared(global).await;
            (AtStart::Recovered, report)
        }
        Ok(Recovered::Waiting(Waiting::StoreHeld(store))) => (
            AtStart::Retry,
            Report::info(format!(
                "The move of the external root waits for the shared store {}",
                store.display()
            )),
        ),
        Ok(Recovered::Waiting(Waiting::Unavailable)) => {
            let _ = access_blocking(global, super::access::release).await;
            let report = Report::warn(
                "The move of the external root waits for a root that isn't available, until the \
                 next service start"
                    .to_string(),
            );
            (AtStart::NextStart, report)
        }
        Err(err) => {
            let report = Report::warn(format!(
                "Recovering the move of the external root failed: {err}"
            ));
            let assessed = global.to_path_buf();
            let refused = lore_base::lore_spawn_blocking!(move || assess(&assessed))
                .await
                .ok()
                .and_then(Result::ok)
                .flatten()
                .is_some_and(|assessment| waits_for_a_start(&assessment));
            if refused {
                let _ = access_blocking(global, super::access::release).await;
                return (AtStart::NextStart, report);
            }
            (AtStart::Retry, report)
        }
    }
}

/// Retries [`recover_at_start`] every [`RETRY_INTERVAL`] while a move is recorded that the quick
/// steps at the start left waiting for anything but a root, logging what an attempt reports only
/// where it differs from what the attempt before reported, and runs `mount` once an attempt
/// recovers it. Then deletes the marker of each still-marked leftover whose path exists again and
/// removes the contents of each leftover, dropping its entry once removed, under shared access.
/// For a task of the service, once it listens.
pub async fn recover_in_background(global: PathBuf, mount: impl Future<Output = ()>) {
    let assessed = global.clone();
    let mut at = match lore_base::lore_spawn_blocking!(move || assess(&assessed)).await {
        Ok(Ok(Some(assessment)))
            if assessment.direction != Direction::Wait && !waits_for_a_start(&assessment) =>
        {
            AtStart::Retry
        }
        _ => AtStart::NoMove,
    };
    let mut reported = None;
    while at == AtStart::Retry {
        tokio::time::sleep(RETRY_INTERVAL).await;
        let report;
        (at, report) = attempt_at_start(&global, LOCK_POLL).await;
        if report.is_some() {
            if report != reported
                && let Some(report) = &report
            {
                report.log();
            }
            reported = report;
        }
        if at == AtStart::Recovered {
            mount.await;
            break;
        }
    }
    if let Err(err) = remove_leftovers(&global).await {
        crate::lore_warn!("Removing what is left of earlier external roots failed: {err}");
    }
}

/// Whether recovery in the service leaves the move `assessment` reads for the next start, with no
/// retry: one the table refuses, and an adopt whose root has gone.
fn waits_for_a_start(assessment: &Assessment) -> bool {
    assessment.direction == Direction::Refuse || assessment.adopted_root_gone
}

/// Deletes the marker of each still-marked leftover of `global` whose path exists again, and
/// removes the contents of each leftover, under shared access, which a pending move refuses and
/// which is taken only where a leftover is listed.
async fn remove_leftovers(global: &Path) -> Result<(), ExternalDirError> {
    let listed = global.to_path_buf();
    let taken = lore_base::lore_spawn_blocking!(move || {
        let leftovers = Leftovers::load(&listed)?;
        if leftovers.paths.is_empty() && leftovers.marked.is_empty() {
            return Ok(false);
        }
        super::access::require_shared(&listed).map(|()| true)
    })
    .await
    .internal("reading the external root leftovers")??;
    if !taken {
        return Ok(());
    }
    unmark_returned_leftovers(global).await?;
    for entry in Leftovers::load(global)?.paths {
        remove_leftover(global, &entry).await?;
    }
    Ok(())
}

/// Releases this process's exclusive access to the root of `global` into shared access, logging a
/// refusal, which leaves the root's virtual instances unmounted.
async fn release_into_shared(global: &Path) {
    if let Err(err) = access_blocking(global, super::access::release_exclusive).await {
        crate::lore_warn!("Not mounting the virtual instances of the external root: {err}");
    }
}

/// Runs the blocking access call `call` for `global` on the blocking pool.
async fn access_blocking<T: Send + 'static>(
    global: &Path,
    call: fn(&Path) -> Result<T, ExternalDirError>,
) -> Result<T, ExternalDirError> {
    let global = global.to_path_buf();
    lore_base::lore_spawn_blocking!(move || call(&global))
        .await
        .internal("taking access to the external root")?
}

/// What a service start found of the external root of its global data directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtStartFound {
    /// No setting and no move record: a root that never moved, with nothing to recover or remove.
    Unmoved,
    /// A setting and no move record: leftovers can remain to remove.
    Configured,
    /// A move record, which [`recover_at_start`] recovers.
    Recorded,
}

/// What a service start examines before it mounts: one read of the setting and one look for the
/// move record of `global`. Reports a directory with entries at the default location while the
/// setting names another root, as [`report_default_location`] does. Blocking.
pub fn examine_at_start(global: &Path) -> AtStartFound {
    let setting = Setting::load(global).ok().flatten();
    if let Some(setting) = &setting {
        report_default_location(global, setting);
    }
    if std::fs::symlink_metadata(global.join(MOVE_RECORD)).is_ok() {
        AtStartFound::Recorded
    } else if setting.is_some() {
        AtStartFound::Configured
    } else {
        AtStartFound::Unmoved
    }
}

/// Logs a directory with entries at the default location of `global` while `setting` names
/// another root, which no redirect replaces, and answers whether there is one. Blocking.
pub fn report_default_location(global: &Path, setting: &Setting) -> bool {
    let default = global.join(super::EXTERNAL);
    if root_path(global, &setting.path) == default {
        return false;
    }
    let holds_entries = std::fs::symlink_metadata(&default)
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
        && std::fs::read_dir(&default).is_ok_and(|mut entries| entries.next().is_some());
    if holds_entries {
        crate::lore_warn!(
            "{} holds entries while external-root names another root, so no redirect replaces it",
            default.display()
        );
    }
    holds_entries
}

/// The root whose virtual instances' stores the final switch of `record` in `global` locks: the
/// root the move renames, at the target once `to_available` says it moved there, the adopted root
/// for `adopt`, and none for `create` and `abandon`.
pub fn store_root(global: &Path, record: &MoveRecord, to_available: bool) -> Option<PathBuf> {
    match record.kind {
        MoveKind::Rename | MoveKind::Copy if to_available => Some(root_path(global, &record.to)),
        MoveKind::Rename | MoveKind::Copy => Some(root_path(global, &record.from)),
        MoveKind::Adopt => Some(root_path(global, &record.to)),
        MoveKind::Create | MoveKind::Abandon => None,
    }
}

/// Undoes the move `record` of `global`, as the module's documentation describes. Answers what
/// the default location holds once the redirect matches the setting.
pub async fn undo(global: &Path, record: &MoveRecord) -> Result<Redirect, ExternalDirError> {
    match record.kind {
        MoveKind::Rename | MoveKind::Copy | MoveKind::Create => {}
        MoveKind::Adopt | MoveKind::Abandon => {
            return Err(ExternalDirError::internal(
                "an --adopt or --abandon of the external root is never undone",
            ));
        }
    }
    let setting = Setting::load(global)?;
    let target = root_path(global, &record.to);
    match record.kind {
        MoveKind::Create => set_aside_if_made(global, &placing_path(&target), None).await?,
        MoveKind::Copy => set_aside_if_made(global, &copying_path(&target), None).await?,
        _ => {}
    }
    let placed = match record.kind {
        MoveKind::Create => Placed::AnyMarker,
        _ => Placed::MarkerOf(setting.as_ref()),
    };
    set_aside_if_made(global, &target, Some(placed)).await?;
    stop_at!(Undo(UndoStep::Redirect));
    let redirect = match_redirect(global, setting.as_ref(), Some(record)).await?;
    stop_at!(Undo(UndoStep::Forget));
    forget(global).await?;
    if setting.is_none() {
        remove_unconfigured_leftovers(global).await?;
    }
    Ok(redirect)
}

/// Removes the leftovers of `global`, which has no setting: what an undone `create` set aside,
/// which holds nothing once its marker is gone. A service starting with no setting and no move
/// record looks for no leftovers, so they are removed here, at once.
async fn remove_unconfigured_leftovers(global: &Path) -> Result<(), ExternalDirError> {
    for entry in Leftovers::load(global)?.paths {
        remove_leftover(global, &entry).await?;
    }
    Ok(())
}

/// The marker that tells a target the move placed.
#[derive(Clone, Copy)]
enum Placed<'a> {
    /// Any marker, for a root the move created.
    AnyMarker,
    /// A marker of this setting, for a root the move renamed.
    MarkerOf(Option<&'a Setting>),
}

/// Unmarks, lists and sets aside `dir`, as the undo steps of the module's documentation describe,
/// where the move made it: with `placed`, where it holds that marker, and with `None`, where it
/// exists. Either way also where the leftover list names a free aside name for it. A redirect at
/// `dir` holds no marker, so the marker of the root it leads to stays.
async fn set_aside_if_made(
    global: &Path,
    dir: &Path,
    placed: Option<Placed<'_>>,
) -> Result<(), ExternalDirError> {
    if !exists(dir).await {
        return Ok(());
    }
    let listed = listed_aside(global, dir).await?;
    let marker = placed_marker(dir).await?;
    let made = listed.is_some()
        || match placed {
            None => true,
            Some(Placed::AnyMarker) => marker.is_some(),
            Some(Placed::MarkerOf(setting)) => {
                marker.zip(setting).is_some_and(|(marker, setting)| {
                    marker.id == setting.id && marker.generation == setting.generation
                })
            }
        };
    if !made {
        return Ok(());
    }
    stop_at!(Undo(UndoStep::Unmark));
    if marker.is_some() {
        remove_marker(dir).await?;
    }
    stop_at!(Undo(UndoStep::List));
    let aside = if let Some(aside) = listed {
        aside
    } else {
        let aside = aside_path(dir);
        let entry = leftover_entry(global, &aside);
        update_leftovers(global, |list| {
            list.paths.push(entry);
            true
        })
        .await?;
        aside
    };
    stop_at!(Undo(UndoStep::SetAside));
    let (from, to) = (dir.to_path_buf(), aside);
    blocking(
        move || lore_io::rename::rename_no_replace(&from, &to, true),
        "setting aside what a move of the external root made",
    )
    .await
}

/// Runs the step of `phase` for `record` in `global`, after its check. Answers what the default
/// location holds, for the `redirect` phase. The steps only a move by copy runs are boxed, as a
/// cold path, so that the futures of every other move hold none of them.
async fn run_phase(
    global: &Path,
    record: &MoveRecord,
    phase: MovePhase,
    renames: Renames,
) -> Result<Option<Redirect>, ExternalDirError> {
    match phase {
        MovePhase::Mark => mark(&root_path(global, &record.from)).await?,
        MovePhase::Identify => identify(global, record).await?,
        MovePhase::Copy => Box::pin(copy_root(global, record)).await?,
        MovePhase::Place => place(global, record, renames).await?,
        MovePhase::Switch => switch(global, record).await?,
        MovePhase::Unmark => Box::pin(unmark_old_root(global, record)).await?,
        MovePhase::SetAside => Box::pin(set_aside_old_root(global, record, renames)).await?,
        MovePhase::Redirect => {
            let setting = Setting::load(global)?;
            return Ok(Some(
                match_redirect(global, setting.as_ref(), Some(record)).await?,
            ));
        }
    }
    Ok(None)
}

/// Checks that the user owns the current root `root`, sets its mode to `0o700` on Unix, and
/// writes a marker of a new root ID and generation 1 into it, unless it holds a marker.
async fn mark(root: &Path) -> Result<(), ExternalDirError> {
    if load_marker(root).await?.is_some() {
        return Ok(());
    }
    let private = root.to_path_buf();
    blocking(
        move || {
            require_owned(&private)?;
            make_private(&private)
        },
        "giving the external root access for its owner only",
    )
    .await?;
    Marker {
        id: RootId::generate(),
        generation: NonZeroU64::MIN,
    }
    .save(root)
    .await
}

/// Writes the setting of `global` from the marker of `record`'s current root, unless the setting
/// holds the marker's root ID.
async fn identify(global: &Path, record: &MoveRecord) -> Result<(), ExternalDirError> {
    let root = root_path(global, &record.from);
    let marker = required_marker(&root).await?;
    if Setting::load(global)?.is_some_and(|setting| setting.id == marker.id) {
        return Ok(());
    }
    Setting {
        path: record.from.clone(),
        id: marker.id,
        generation: marker.generation,
    }
    .save(global)
    .await
}

/// Places the new root of `record` at its target in `global`, unless the target holds the marker
/// the module's documentation names.
async fn place(
    global: &Path,
    record: &MoveRecord,
    renames: Renames,
) -> Result<(), ExternalDirError> {
    let target = root_path(global, &record.to);
    let setting = Setting::load(global)?;
    let marker = placed_marker(&target).await?;
    match record.kind {
        MoveKind::Rename => {
            let setting = setting.ok_or_else(no_setting)?;
            if marker.is_some_and(|marker| {
                marker.id == setting.id && marker.generation == setting.generation
            }) {
                return Ok(());
            }
            clear_default_location(global, record, &setting, &target).await?;
            let from = root_path(global, &record.from);
            let (private, parent) = (from.clone(), target.parent().map(Path::to_path_buf));
            blocking(
                move || -> Result<(), ExternalDirError> {
                    make_private(&private)?;
                    if let Some(parent) = parent {
                        lore_io::create_dir_all_private(&parent)
                            .internal("making the parents of the target")?;
                    }
                    Ok(())
                },
                "preparing the rename of the external root",
            )
            .await?;
            rename_root(&from, &target, renames).await
        }
        MoveKind::Create => {
            if marker.is_some() {
                return Ok(());
            }
            place_new_root(
                &target,
                Marker {
                    id: RootId::generate(),
                    generation: NonZeroU64::MIN,
                },
            )
            .await
        }
        MoveKind::Abandon => {
            let setting = setting.ok_or_else(no_setting)?;
            let generation = setting.generation.checked_add(1).ok_or_else(|| {
                ExternalDirError::internal("the external root generation can't rise further")
            })?;
            if marker
                .is_some_and(|marker| marker.id == setting.id && marker.generation == generation)
            {
                return Ok(());
            }
            clear_default_location(global, record, &setting, &target).await?;
            place_new_root(
                &target,
                Marker {
                    id: setting.id,
                    generation,
                },
            )
            .await
        }
        MoveKind::Copy => {
            let setting = setting.ok_or_else(no_setting)?;
            if marker.is_some_and(|marker| {
                marker.id == setting.id && marker.generation == setting.generation
            }) {
                return Ok(());
            }
            Box::pin(place_copy(global, record, &setting, &target, renames)).await
        }
        MoveKind::Adopt => Err(ExternalDirError::internal(format!(
            "a move of kind {:?} has no place phase here",
            record.kind
        ))),
    }
}

/// Removes the command's redirect at the default location of `global`, `target`, where `record`
/// places its root there, so that the root can take its place. Refuses what the move didn't make.
async fn clear_default_location(
    global: &Path,
    record: &MoveRecord,
    setting: &Setting,
    target: &Path,
) -> Result<(), ExternalDirError> {
    if record.to.is_empty()
        && redirect_to(global, None, Some(setting), Some(record)).await? != Redirect::Matched
    {
        return Err(ExternalDirError::internal(format!(
            "the default location {} holds what the move didn't make, so the external root can't \
             take its place",
            target.display()
        )));
    }
    Ok(())
}

/// Places the copy of `record`'s root in `global` at `target`, under `setting`, as the `place`
/// step of a move by copy does: to the default location, it first removes the command's redirect
/// there.
async fn place_copy(
    global: &Path,
    record: &MoveRecord,
    setting: &Setting,
    target: &Path,
    renames: Renames,
) -> Result<(), ExternalDirError> {
    clear_default_location(global, record, setting, target).await?;
    rename_root(&copying_path(target), target, renames).await
}

/// Copies the current root of `record` in `global` into the temporary directory beside its
/// target, as the `copy` step of the module's documentation describes, and as [`copying_as`]
/// says: the copy stops once its progress is stopped, and it tells each external directory it
/// reaches. Gives the current root access for its owner only first, as a rename does, so the
/// copy takes that mode; warns once of the attributes the system refused to copy, and logs once
/// the entries that kept the target's group.
async fn copy_root(global: &Path, record: &MoveRecord) -> Result<(), ExternalDirError> {
    let setting = Setting::load(global)?.ok_or_else(no_setting)?;
    let target = root_path(global, &record.to);
    let copied = copying_path(&target);
    let holds_setting = |marker: Option<Marker>| {
        marker.is_some_and(|marker| {
            marker.id == setting.id && marker.generation == setting.generation
        })
    };
    if holds_setting(placed_marker(&target).await?) || holds_setting(placed_marker(&copied).await?)
    {
        return Ok(());
    }
    crate::util::fs::unlink_recursive(&copied)
        .await
        .internal_with(|| format!("removing {}", copied.display()))?;
    let made = copied.clone();
    blocking(
        move || lore_io::create_dir_all_private(&made),
        "making the directory the external root is copied into",
    )
    .await?;
    let from = root_path(global, &record.from);
    let private = from.clone();
    blocking(
        move || make_private(&private),
        "giving the external root access for its owner only before its copy",
    )
    .await?;
    let copy = COPY_RUN
        .try_with(Clone::clone)
        .ok()
        .flatten()
        .unwrap_or_else(|| CopyRun {
            progress: Arc::new(Progress::default()),
            on_copying: Arc::new(|_| {}),
        });
    let faithful = copy_faithfully(&from, &copied, &copy).await?;
    if !faithful.relabeled.is_empty() {
        let kept: Vec<String> = faithful
            .relabeled
            .iter()
            .map(|(name, entries)| format!("{name} on {entries} entries"))
            .collect();
        crate::lore_warn!(
            "The copy of the external root keeps the values its file system gives these \
             attributes, which the system refused to copy: {}",
            kept.join(", ")
        );
    }
    if faithful.kept_groups > 0 {
        crate::lore_info!(
            "{} entries of the copy of the external root keep the group of their new location,              as this user isn't a member of the group they had",
            faithful.kept_groups
        );
    }
    Marker {
        id: setting.id,
        generation: setting.generation,
    }
    .save(&copied)
    .await
}

/// Copies the root `from` into the directory `copied` with [`crate::fs::walk::copy_faithfully`],
/// leaving out its marker and the contents of each `write-staging/`, on the blocking pool, and
/// tells `copy` the name of each external directory as the copy reaches it.
#[lore_macro::test_pub]
async fn copy_faithfully(
    from: &Path,
    copied: &Path,
    copy: &CopyRun,
) -> Result<crate::fs::walk::FaithfulCopy, ExternalDirError> {
    let (reached, mut reports) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (source, destination, progress) = (
        from.to_path_buf(),
        copied.to_path_buf(),
        Arc::clone(&copy.progress),
    );
    let copying = lore_base::lore_spawn_blocking!(move || {
        crate::fs::walk::copy_faithfully(
            &source,
            &destination,
            &|_path, relative, kind| copied_visit(relative, kind, &reached),
            &progress,
        )
    });
    let mut copying = std::pin::pin!(copying);
    let copied_root = loop {
        tokio::select! {
            done = &mut copying => break done,
            Some(name) = reports.recv() => (copy.on_copying)(&name),
        }
    };
    while let Ok(name) = reports.try_recv() {
        (copy.on_copying)(&name);
    }
    Ok(copied_root
        .internal("copying the external root")?
        .internal_with(|| {
            format!(
                "copying the external root {} to {}",
                from.display(),
                copied.display()
            )
        })?)
}

/// What a copy of an external root does with the entry at `relative` in it, of `kind`: leaves out
/// the root's marker and the contents of each `write-staging/`, and sends the name of each
/// external directory on `reached` as it reaches it.
#[lore_macro::test_pub]
fn copied_visit(
    relative: &Path,
    kind: Kind,
    reached: &tokio::sync::mpsc::UnboundedSender<String>,
) -> Visit {
    let mut components = relative.components();
    if let (Some(name), None) = (components.next(), components.next()) {
        if name.as_os_str() == MARKER {
            return Visit::Skip;
        }
        if kind == Kind::Directory {
            let _ = reached.send(name.as_os_str().to_string_lossy().into_owned());
        }
    }
    if super::check::in_write_staging(relative) {
        return Visit::Skip;
    }
    if kind == Kind::Directory {
        Visit::EachEntry
    } else {
        Visit::Clone
    }
}

/// Deletes the marker of the old root of `record` in `global`, durably, as the `unmark` step of
/// the module's documentation describes.
async fn unmark_old_root(global: &Path, record: &MoveRecord) -> Result<(), ExternalDirError> {
    let from = root_path(global, &record.from);
    if is_real_directory(&from).await? {
        return remove_marker(&from).await;
    }
    if !exists(&from).await {
        list_leftover(global, &from, true).await?;
    }
    Ok(())
}

/// Sets what is left of the old root of `record` in `global` aside, as the `set-aside` step of
/// the module's documentation describes, renaming it as `renames` says.
async fn set_aside_old_root(
    global: &Path,
    record: &MoveRecord,
    renames: Renames,
) -> Result<(), ExternalDirError> {
    let from = root_path(global, &record.from);
    if !is_real_directory(&from).await? {
        return Ok(());
    }
    let aside = if let Some(aside) = listed_aside(global, &from).await? {
        aside
    } else {
        let aside = aside_path(&from);
        let entry = leftover_entry(global, &aside);
        update_leftovers(global, |list| {
            list.paths.push(entry);
            true
        })
        .await?;
        aside
    };
    rename_root(&from, &aside, renames).await
}

/// Renames the root at `from` to `target`, durably, reissuing the rename as `renames` says while
/// files in it are in use and `target` is free.
async fn rename_root(from: &Path, target: &Path, renames: Renames) -> Result<(), ExternalDirError> {
    retry_while_in_use(
        "rename the external root",
        |error: &std::io::Error| {
            let retried = renames == Renames::RetriedWhileInUse;
            let retryable = rename_is_retryable(error, target);
            async move { retried && retryable.await }
        },
        || {
            let (from, to) = (from.to_path_buf(), target.to_path_buf());
            async move {
                lore_base::lore_spawn_blocking!(move || {
                    lore_io::rename::rename_no_replace(&from, &to, true)
                })
                .await
                .map_err(std::io::Error::other)?
            }
        },
    )
    .await
    .map_err(|err| {
        ExternalDirError::internal(format!(
            "renaming the external root {} to {}: {err}",
            from.display(),
            target.display()
        ))
    })
}

/// Makes `target`'s missing parents, then `target` at its temporary name beside it, holding
/// `marker`, and renames it to `target`, each durably. A directory left at the temporary name by
/// an earlier attempt is removed first.
async fn place_new_root(target: &Path, marker: Marker) -> Result<(), ExternalDirError> {
    let placing = placing_path(target);
    crate::util::fs::unlink_recursive(&placing)
        .await
        .internal_with(|| format!("removing {}", placing.display()))?;
    let made = placing.clone();
    blocking(
        move || lore_io::create_dir_all_private(&made),
        "making a new external root",
    )
    .await?;
    marker.save(&placing).await?;
    let placed = target.to_path_buf();
    blocking(
        move || lore_io::rename::rename_no_replace(&placing, &placed, true),
        "placing a new external root",
    )
    .await
}

/// Writes the setting of `global` from the marker at `record`'s target, unless it holds the
/// target with that marker's root ID and generation. First takes the target off the leftover
/// list, where a takeover or an earlier move listed it.
async fn switch(global: &Path, record: &MoveRecord) -> Result<(), ExternalDirError> {
    let target = root_path(global, &record.to);
    let marker = required_marker(&target).await?;
    let entry = leftover_entry(global, &target);
    update_leftovers(global, |list| take_off(list, &entry)).await?;
    let switched = Setting {
        path: record.to.clone(),
        id: marker.id,
        generation: marker.generation,
    };
    if Setting::load(global)?.as_ref() == Some(&switched) {
        return Ok(());
    }
    switched.save(global).await
}

/// Makes the default location of `global` match `setting`, as the `redirect` phase does.
async fn match_redirect(
    global: &Path,
    setting: Option<&Setting>,
    record: Option<&MoveRecord>,
) -> Result<Redirect, ExternalDirError> {
    let root = setting.map_or("", |setting| setting.path.as_str());
    if root.is_empty() {
        return Ok(Redirect::Matched);
    }
    redirect_to(global, Some(Path::new(root)), setting, record).await
}

/// Removes the move record of `global`, durably.
async fn forget(global: &Path) -> Result<(), ExternalDirError> {
    remove_durably(
        &global.join(MOVE_RECORD),
        "removing the external root move record",
    )
    .await
}

/// Lists the leftover `path` of `global` in the leftover list: in `marked` where it was listed
/// while it couldn't be reached and keeps its marker, else in `paths`. Listing it twice lists it
/// once.
pub async fn list_leftover(
    global: &Path,
    path: &Path,
    marked: bool,
) -> Result<(), ExternalDirError> {
    let entry = leftover_entry(global, path);
    update_leftovers(global, |list| {
        let listed = if marked {
            &mut list.marked
        } else {
            &mut list.paths
        };
        if listed.contains(&entry) {
            return false;
        }
        listed.push(entry);
        true
    })
    .await
}

/// Deletes the marker of each leftover of `global` listed in `marked` whose path exists again, and
/// lists it in `paths`, after reading the setting: a marked leftover that is the root the setting
/// names is taken off the list with its marker kept. A marked leftover whose path is absent stays
/// listed.
pub async fn unmark_returned_leftovers(global: &Path) -> Result<(), ExternalDirError> {
    let marked = Leftovers::load(global)?.marked;
    let configured = configured_root(global)?;
    for entry in marked {
        let path = leftover_path(global, &entry);
        if configured.as_ref() == Some(&path) {
            update_leftovers(global, |list| take_off(list, &entry)).await?;
            continue;
        }
        if !exists(&path).await {
            continue;
        }
        remove_marker(&path).await?;
        update_leftovers(global, |list| {
            let Some(at) = list.marked.iter().position(|listed| *listed == entry) else {
                return false;
            };
            let entry = list.marked.remove(at);
            if !list.paths.contains(&entry) {
                list.paths.push(entry);
            }
            true
        })
        .await?;
    }
    Ok(())
}

/// The root the setting of `global` names, if there is a setting.
fn configured_root(global: &Path) -> Result<Option<PathBuf>, ExternalDirError> {
    Ok(Setting::load(global)?.map(|setting| root_path(global, &setting.path)))
}

/// Takes `entry` off both lists of `list`, and answers whether it was on one.
fn take_off(list: &mut Leftovers, entry: &str) -> bool {
    let listed = list.paths.len() + list.marked.len();
    list.paths.retain(|path| path != entry);
    list.marked.retain(|path| path != entry);
    list.paths.len() + list.marked.len() != listed
}

/// Removes the leftover `entry` of `global`, listed in `paths`: its marker first, durably, then its
/// contents, then its entry. A leftover that is the root the setting names is taken off the list
/// and kept.
pub async fn remove_leftover(global: &Path, entry: &str) -> Result<(), ExternalDirError> {
    let path = leftover_path(global, entry);
    let configured = configured_root(global)?.as_ref() == Some(&path);
    if !configured && exists(&path).await {
        remove_marker(&path).await?;
        stop_at!(LeftoverUnmarked);
        let (_, removed) = lore_io::IoDriver::global()
            .remove_dir_all_in_background(&path, None, true)
            .await;
        removed.internal_with(|| format!("removing the leftover {}", path.display()))?;
    }
    update_leftovers(global, |list| {
        let listed = list.paths.len();
        list.paths.retain(|listed| listed != entry);
        list.paths.len() != listed
    })
    .await
}

/// Reads the leftover list of `global` while holding its lock, and writes it back where `change`
/// answers that it changed it.
async fn update_leftovers(
    global: &Path,
    change: impl FnOnce(&mut Leftovers) -> bool,
) -> Result<(), ExternalDirError> {
    let (mut list, _lock) = config::load_with_lock::<Leftovers>(global.join(LEFTOVERS))
        .await
        .forward::<ExternalDirError>("reading the external root leftovers")?;
    if change(&mut list) {
        list.save(global).await?;
    }
    Ok(())
}

/// The aside name the leftover list of `global` names for `dir` while nothing is at it.
async fn listed_aside(global: &Path, dir: &Path) -> Result<Option<PathBuf>, ExternalDirError> {
    let Some(prefix) = dir.file_name() else {
        return Ok(None);
    };
    let mut prefix = prefix.to_owned();
    prefix.push(ASIDE_SUFFIX);
    for entry in Leftovers::load(global)?.paths {
        let aside = leftover_path(global, &entry);
        let beside = aside.parent() == dir.parent();
        let named = aside
            .file_name()
            .and_then(|name| name.to_str())
            .zip(prefix.to_str())
            .is_some_and(|(name, prefix)| name.starts_with(prefix));
        if beside && named && !exists(&aside).await {
            return Ok(Some(aside));
        }
    }
    Ok(None)
}

/// A new aside name beside `dir`: its name, [`ASIDE_SUFFIX`] and 16 random hex digits.
fn aside_path(dir: &Path) -> PathBuf {
    let mut name = dir.file_name().unwrap_or_default().to_owned();
    name.push(format!("{ASIDE_SUFFIX}{:016x}", rand::random::<u64>()));
    dir.with_file_name(name)
}

/// The temporary name beside `target` that a new root is made at.
fn placing_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_owned();
    name.push(PLACING_SUFFIX);
    target.with_file_name(name)
}

/// The temporary name beside `target` that a move by copy copies the root into.
#[lore_macro::test_pub]
pub(super) fn copying_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_owned();
    name.push(COPYING_SUFFIX);
    target.with_file_name(name)
}

/// The leftover list's entry for `path`: its name alone where it lies in `global`, else the path.
fn leftover_entry(global: &Path, path: &Path) -> String {
    match path.strip_prefix(global) {
        Ok(name) if name.components().count() == 1 => name.to_string_lossy().into_owned(),
        _ => path.to_string_lossy().into_owned(),
    }
}

/// The path of the leftover list's `entry` in `global`.
pub fn leftover_path(global: &Path, entry: &str) -> PathBuf {
    let path = Path::new(entry);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        global.join(path)
    }
}

/// The marker of the root at `root`, or `None` where it holds none or is missing.
async fn load_marker(root: &Path) -> Result<Option<Marker>, ExternalDirError> {
    let root = root.to_path_buf();
    lore_base::lore_spawn_blocking!(move || Marker::load(&root))
        .await
        .internal("reading an external root marker")?
}

/// The marker of the root placed at `target`, read only where `target` is a directory and not a
/// redirect, which leads elsewhere.
async fn placed_marker(target: &Path) -> Result<Option<Marker>, ExternalDirError> {
    if !is_real_directory(target).await? {
        return Ok(None);
    }
    load_marker(target).await
}

/// Whether `path` is a directory and not a redirect, which leads elsewhere.
async fn is_real_directory(path: &Path) -> Result<bool, ExternalDirError> {
    let examined = path.to_path_buf();
    Ok(lore_base::lore_spawn_blocking!(move || {
        std::fs::symlink_metadata(&examined)
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
    })
    .await
    .internal("examining a root of a move of the external root")?)
}

/// The marker of the root at `root`, which must hold one.
async fn required_marker(root: &Path) -> Result<Marker, ExternalDirError> {
    load_marker(root).await?.ok_or_else(|| {
        ExternalDirError::internal(format!("{} holds no external root marker", root.display()))
    })
}

/// Deletes the marker of the root at `root`, durably, where it holds one.
async fn remove_marker(root: &Path) -> Result<(), ExternalDirError> {
    remove_durably(&root.join(MARKER), "removing an external root marker").await
}

/// Removes the file at `path`, where there is one, and flushes its directory.
async fn remove_durably(path: &Path, context: &'static str) -> Result<(), ExternalDirError> {
    crate::util::fs::unlink(path).await.internal(context)?;
    let Some(parent) = path.parent().map(Path::to_path_buf) else {
        return Ok(());
    };
    blocking(move || lore_io::sync_dir(&parent), context).await
}

/// Whether anything is at `path`, read without following a redirect.
async fn exists(path: &Path) -> bool {
    lore_io::IoDriver::global().symlink_stat(path).await.is_ok()
}

/// Fails unless the user owns the directory at `root`, on Unix. Blocking.
pub fn require_owned(root: &Path) -> Result<(), ExternalDirError> {
    #[cfg(target_family = "unix")]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(root)
            .internal_with(|| format!("examining the external root {}", root.display()))?;
        // SAFETY: geteuid has no preconditions.
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(ExternalDirError::internal(format!(
                "the external root {} belongs to another user",
                root.display()
            )));
        }
    }
    #[cfg(not(target_family = "unix"))]
    let _ = root;
    Ok(())
}

/// Gives the directory at `root` access for its owner only, on Unix. Blocking.
fn make_private(root: &Path) -> Result<(), ExternalDirError> {
    #[cfg(target_family = "unix")]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
            .internal_with(|| format!("giving {} access for its owner only", root.display()))?;
    }
    #[cfg(not(target_family = "unix"))]
    let _ = root;
    Ok(())
}

/// Runs the blocking `work` on the blocking pool, failing as internal with `context`.
async fn blocking<T: Send + 'static, E: std::error::Error + Send + Sync + 'static>(
    work: impl FnOnce() -> Result<T, E> + Send + 'static,
    context: &'static str,
) -> Result<T, ExternalDirError> {
    match lore_base::lore_spawn_blocking!(work).await {
        Ok(Ok(done)) => Ok(done),
        Ok(Err(err)) => Err(ExternalDirError::internal_with_context(err, context)),
        Err(err) => Err(ExternalDirError::internal_with_context(err, context)),
    }
}

/// The error of a recovery that needs a setting where there is none.
fn no_setting() -> ExternalDirError {
    ExternalDirError::internal("external-root is missing")
}
