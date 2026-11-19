// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `--adopt` and `--abandon`: the two changes of the setting that move nothing, each recorded and
//! recovered as a move, and each able to take over a move that waits for a root.

use std::path::Path;
use std::path::PathBuf;

use lore_base::fs::lock::FSLock;
use lore_error_set::prelude::*;

use super::check::check_new_root;
use super::check::refuse_linked_root;
use super::locks::Joining;
use super::locks::StoreLocks;
use super::locks::lock_stores_until;
use super::locks::store_set;
use super::moving::MoveError;
use super::moving::Moved;
use super::moving::blocking;
use super::moving::path_text;
use super::moving::refuse_live_mounts;
use super::record::MoveKind;
use super::record::MovePhase;
use super::record::MoveRecord;
use super::recovery;
use super::recovery::Locking;
use super::recovery::Recovered;
use super::recovery::Waiting;
use super::refused;
use super::resolve::Availability;
use super::resolve::checked_marker;
use super::resolve::resolve_in;
use super::resolve::root_available;
use super::resolve::root_path;
use super::setting::Setting;

/// Points the setting of `global` at the existing directory `target`, which holds the marker of the
/// setting's root ID and generation and, on Unix, is the user's alone, and makes the redirect
/// match. With no setting, the marker's root ID and generation are taken, and the setting written
/// again. The adopted root is taken off the leftover list. A move that waits for a root is taken
/// over: the move's other roots, and the temporary directory of a move by copy, are listed as
/// leftovers first. So is an adopt whose root has gone, which made no root and gives up none. Runs
/// as [`super::moving::move_root`] runs, with no service and exclusive access held.
pub async fn adopt(
    global: &Path,
    target: &Path,
    _access: &FSLock,
    termination: impl Future<Output = ()>,
    mut on_held: impl FnMut(&Path) + Send,
) -> Result<Moved, MoveError> {
    let mut termination = std::pin::pin!(termination);
    let (recovered, waiting) = recover_or_wait(global, &mut on_held, &mut termination).await?;
    let linked = target.to_path_buf();
    lore_base::lore_spawn_blocking!(move || refuse_linked_root(&linked))
        .await
        .internal("checking the target of an adopt")?
        .forward::<MoveError>("checking the target of an adopt")?;
    let setting = blocking(global, |global| Setting::load(&global)).await?;
    let (adopted, record_path) = (target.to_path_buf(), path_text(global, target)?);
    let checked = blocking(global, move |global| {
        Ok(checked_marker(&global, &adopted, setting.as_ref()))
    })
    .await?;
    if let Err(reason) = checked {
        return Err(refused(format!(
            "{} can't be adopted: {}",
            target.display(),
            reason.describe()
        )));
    }
    if waiting.is_none() && current_root(global).await? == Some(target.to_path_buf()) {
        return Ok(nothing_moved(recovered));
    }
    refuse_nested(target).await?;
    let from = blocking(global, |global| Setting::load(&global))
        .await?
        .map_or_default(|setting| setting.path);
    let stores = store_set(target)
        .await
        .forward::<MoveError>("reading the stores of the adopted root")?;
    let mut record = MoveRecord {
        from,
        to: record_path,
        kind: MoveKind::Adopt,
        phase: MovePhase::Switch,
    };
    let locks = lock_for_takeover(global, &record, &stores, termination, &mut on_held).await?;
    if let Some(waiting) = &waiting {
        let given_up: Vec<PathBuf> = [&waiting.from, &waiting.to]
            .into_iter()
            .map(|root| root_path(global, root))
            .chain(copying_of(global, waiting))
            .filter(|root| waiting.kind != MoveKind::Adopt && root != target)
            .collect();
        give_up(global, &given_up).await?;
    }
    let redirect = finish_switch(global, &mut record, locks).await?;
    Ok(Moved {
        kind: Some(MoveKind::Adopt),
        redirect,
        recovered,
    })
}

/// Gives up the unavailable current root of `global` for a new, empty root at `target` with the
/// next generation, so the old root stays unavailable when it returns, and makes the redirect
/// match. Refuses while the current root is available, and with no setting. A move that waits for
/// a root, or an adopt whose root has gone, is taken over, unless the root it leaves is available:
/// what the move made at its target, and the temporary directory of a move by copy, is listed as a
/// leftover first, which for an adopt is nothing.
/// Runs as [`super::moving::move_root`] runs, with no service and exclusive access held.
pub async fn abandon(
    global: &Path,
    target: &Path,
    _access: &FSLock,
    termination: impl Future<Output = ()>,
    mut on_held: impl FnMut(&Path) + Send,
) -> Result<Moved, MoveError> {
    let mut termination = std::pin::pin!(termination);
    let (recovered, waiting) = recover_or_wait(global, &mut on_held, &mut termination).await?;
    let Some(setting) = blocking(global, |global| Setting::load(&global)).await? else {
        return Err(refused(
            "external-root is missing, so there is no root ID to keep: adopt the root first"
                .to_string(),
        ));
    };
    let setting_path = setting.path.clone();
    let (current_available, from_available) = {
        let (from, current) = (
            waiting.as_ref().map(|waiting| waiting.from.clone()),
            setting.clone(),
        );
        blocking(global, move |global| {
            Ok((
                root_available(&global, &setting_path, Some(&current)),
                from.is_some_and(|from| root_available(&global, &from, Some(&current))),
            ))
        })
        .await?
    };
    if current_available || from_available {
        return Err(refused(
            "the external root is available, so it isn't abandoned: adopt it instead".to_string(),
        ));
    }
    let (new_root_of, checked_target) = (global.to_path_buf(), target.to_path_buf());
    lore_base::lore_spawn_blocking!(move || check_new_root(&new_root_of, &checked_target))
        .await
        .internal("checking the target of an abandon")?
        .forward::<MoveError>("checking the target of an abandon")?;
    let mut record = MoveRecord {
        from: setting.path,
        to: path_text(global, target)?,
        kind: MoveKind::Abandon,
        phase: MovePhase::Place,
    };
    let locks = lock_for_takeover(global, &record, &[], termination, &mut on_held).await?;
    if let Some(waiting) = &waiting {
        let made: Vec<PathBuf> = if waiting.kind == MoveKind::Adopt {
            Vec::new()
        } else {
            std::iter::once(root_path(global, &waiting.to))
                .chain(copying_of(global, waiting))
                .collect()
        };
        give_up(global, &made).await?;
    }
    let redirect = finish_switch(global, &mut record, locks).await?;
    Ok(Moved {
        kind: Some(MoveKind::Abandon),
        redirect,
        recovered,
    })
}

/// Recovers an interrupted move of `global`, until `termination` while it takes the locks,
/// answering what recovery did and, where the move waits for a root, its record, which a takeover
/// replaces. An adopt whose root has gone, which recovery fails on, reads as one that waits.
async fn recover_or_wait(
    global: &Path,
    on_held: &mut (impl FnMut(&Path) + Send),
    termination: impl Future<Output = ()>,
) -> Result<(Recovered, Option<MoveRecord>), MoveError> {
    if let Some(assessment) = blocking(global, |global| recovery::assess(&global)).await?
        && assessment.adopted_root_gone
    {
        return Ok((
            Recovered::Waiting(Waiting::Unavailable),
            Some(assessment.record),
        ));
    }
    let recovered = recovery::recover(
        global,
        Joining::Never,
        Locking::Restarting(on_held),
        termination,
    )
    .await
    .forward::<MoveError>("recovering an interrupted move of the external root")?;
    let waiting = match &recovered {
        Recovered::Waiting(Waiting::Unavailable) => {
            blocking(global, |global| MoveRecord::load(&global)).await?
        }
        _ => None,
    };
    Ok((recovered, waiting))
}

/// The temporary directory a waiting move by copy, `waiting`, copies into, which a takeover gives
/// up with the move's roots.
fn copying_of(global: &Path, waiting: &MoveRecord) -> Option<PathBuf> {
    (waiting.kind == MoveKind::Copy)
        .then(|| recovery::copying_path(&root_path(global, &waiting.to)))
}

/// The root of `global` a resolution finds available, if any.
async fn current_root(global: &Path) -> Result<Option<PathBuf>, MoveError> {
    blocking(global, |global| {
        Ok(match resolve_in(global).availability {
            Availability::Available(root) => Some(root),
            _ => None,
        })
    })
    .await
}

/// Lists the roots `given_up` as leftovers of `global` that keep their marker until their removal
/// begins, leaving out a redirect, which is no root.
async fn give_up(global: &Path, given_up: &[PathBuf]) -> Result<(), MoveError> {
    for path in given_up {
        let examined = path.clone();
        let redirect = blocking(global, move |_global| {
            Ok(std::fs::symlink_metadata(&examined)
                .is_ok_and(|metadata| metadata.file_type().is_symlink()))
        })
        .await?;
        if !redirect {
            recovery::list_leftover(global, path, true)
                .await
                .forward::<MoveError>("listing what a takeover gives up")?;
        }
    }
    #[cfg(feature = "test-util")]
    if !crate::test_util::move_step(crate::test_util::MoveStep::TakeoverListed).await {
        return Err(MoveError::internal(
            "a test failed the takeover once it listed what it gives up",
        ));
    }
    Ok(())
}

/// Refuses the live mounts of the root `record` switches to in `global`, then takes the locks of
/// `stores`, free locks first, before the takeover writes anything: `termination` while a held
/// lock is waited for fails the takeover with nothing changed.
async fn lock_for_takeover(
    global: &Path,
    record: &MoveRecord,
    stores: &[PathBuf],
    termination: impl Future<Output = ()>,
    on_held: &mut (impl FnMut(&Path) + Send),
) -> Result<StoreLocks, MoveError> {
    if let Some(root) = super::recovery::store_root(global, record, true) {
        refuse_live_mounts(&root).await?;
    }
    lock_stores_until(stores, Joining::Never, on_held, termination)
        .await
        .forward::<MoveError>("taking the stores of the external root")?
        .ok_or_else(|| {
            MoveError::internal(
                "the takeover of the external root was cancelled while it waited for a shared \
                 store, with nothing changed",
            )
        })
}

/// Writes `record` into `global`, once the takeover has listed what it gives up, and runs its
/// final switch under `locks`, then deletes the marker of each listed leftover whose path exists.
/// An adopt or an abandon is finished, never undone: once the record is written, a failure keeps
/// it for recovery.
async fn finish_switch(
    global: &Path,
    record: &mut MoveRecord,
    locks: StoreLocks,
) -> Result<super::redirect::Redirect, MoveError> {
    record
        .save(global)
        .await
        .forward::<MoveError>("writing the external root move record")?;
    let redirect = recovery::finish(global, record)
        .await
        .forward::<MoveError>("running the final switch of a takeover of the external root")?;
    drop(locks);
    recovery::unmark_returned_leftovers(global)
        .await
        .forward::<MoveError>("deleting the markers of the leftovers")?;
    Ok(redirect)
}

/// The answer of a takeover that changes nothing.
fn nothing_moved(recovered: Recovered) -> Moved {
    Moved {
        kind: None,
        redirect: super::redirect::Redirect::Matched,
        recovered,
    }
}

/// Refuses a target inside a repository instance's working tree or a live mount.
async fn refuse_nested(target: &Path) -> Result<(), MoveError> {
    let checked = target.to_path_buf();
    lore_base::lore_spawn_blocking!(move || crate::repository::refuse_instance_nested_at(&checked))
        .await
        .internal("checking the target of a takeover")?
        .map_err(MoveError::from)
}
