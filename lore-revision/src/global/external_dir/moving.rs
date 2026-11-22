// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A move of the external root by rename or by copy, or the creation of a first root, once the
//! service is stopped: recovery of an interrupted move, the checks after the stop, the record, the
//! copy and the final switch under the store locks.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use lore_base::error::MountsBusy;
use lore_base::fs::lock::FSLock;
use lore_error_set::prelude::*;

use super::EXTERNAL;
use super::check::check_target;
use super::locks::Joining;
use super::locks::lock_stores;
use super::locks::store_set;
use super::mounts::live_mounts_in;
use super::record::MoveKind;
use super::record::MovePhase;
use super::record::MoveRecord;
use super::recovery;
use super::recovery::CopyRun;
use super::recovery::Locking;
use super::recovery::Recovered;
use super::redirect::Redirect;
use super::refused;
use super::resolve::Availability;
use super::resolve::resolve_in;
use super::setting::Setting;
use crate::errors::InvalidArguments;
use crate::instance::list_instances;
use crate::shared_store::SHARED_STORE_DIR;
use crate::shared_store::registry::SharedStoreRegistryEntry;

#[error_set]
pub enum MoveError {
    InvalidArguments,
    MountsBusy,
}

/// What a move did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Moved {
    /// How the root moved, or `None` where the target is the current root and nothing moved.
    pub kind: Option<MoveKind>,
    /// What the default location holds once the redirect matches the setting.
    pub redirect: Redirect,
    /// What recovery did with a move interrupted before this one.
    pub recovered: Recovered,
}

/// Moves the external root of the global data directory `global` to the absolute path `target`,
/// by rename on one volume and by copy across volumes, or creates it there where no default root
/// exists, once the checks before the stop, [`super::check::require_copy_space`] among them for a
/// copy, have passed, with no service running and the exclusive access to the root that
/// [`super::access::try_exclusive_for_move`] takes held:
///
/// 1. recovers an interrupted move, refusing where it waits;
/// 2. checks the target again, refuses a target inside a registered virtual instance's path, read
///    under the store locks, which names each held store to `on_held` before a copy starts, and a
///    live mount of a virtual instance in the root with `MountsBusy`;
/// 3. writes the move record and runs the phases before the final switch, the copy among them,
///    telling `on_copying` the name of each external directory as the copy reaches it;
/// 4. checks the mount table again, and takes the store locks, naming each held store to
///    `on_held`;
/// 5. runs the final switch.
///
/// A failure after the record is written, a live mount found in step 4, and `termination`
/// completing during step 3, which stops the copy, or while the locks are taken, take the move as
/// recovery would, undoing it before the final switch. `termination` completing while the
/// registrations are read cancels the move before it changes anything, and completing after the
/// locks are taken takes effect once the move has finished. `termination` completing while the
/// recovery in step 1, or the recovery of a failed final switch, waits for a store fails the
/// command, keeping the move's record.
pub async fn move_root(
    global: &Path,
    target: &Path,
    _access: &FSLock,
    termination: impl Future<Output = ()>,
    mut on_held: impl FnMut(&Path) + Send,
    on_copying: impl Fn(&str) + Send + Sync + 'static,
) -> Result<Moved, MoveError> {
    let mut termination = std::pin::pin!(termination);
    let recovered = recovery::recover(
        global,
        Joining::Never,
        Locking::Restarting(&mut on_held),
        &mut termination,
    )
    .await
    .forward::<MoveError>("recovering an interrupted move of the external root")?;
    if let Recovered::Waiting(waiting) = &recovered {
        return Err(MoveError::internal(format!(
            "a move of the external root is interrupted, and its recovery waits: {waiting:?}"
        )));
    }
    let (state, kind) = {
        let global = global.to_path_buf();
        let checked = target.to_path_buf();
        lore_base::lore_spawn_blocking!(move || {
            let state = resolve_in(global);
            check_target(&state, &checked).map(|kind| (state, kind))
        })
        .await
        .internal("checking the target of a move of the external root")?
        .forward::<MoveError>("checking the target of a move of the external root")?
    };
    let Some(kind) = kind else {
        return Ok(Moved {
            kind: None,
            redirect: Redirect::Matched,
            recovered,
        });
    };
    let Availability::Available(root) = state.availability else {
        return Err(MoveError::internal("the external root isn't available"));
    };
    let stores = match kind {
        MoveKind::Rename | MoveKind::Copy => store_set(&root)
            .await
            .forward::<MoveError>("reading the stores of the external root")?,
        _ => Vec::new(),
    };
    tokio::select! {
        refused = refuse_registered_target(&root, &stores, target, &mut on_held) => refused?,
        () = &mut termination => {
            return Err(MoveError::internal("the move of the external root was cancelled"));
        }
    }
    refuse_live_mounts(&root).await?;

    let setting = blocking(global, |global| Setting::load(&global)).await?;
    let first_move = setting.is_none();
    let mut record = MoveRecord {
        from: setting.map_or_default(|setting| setting.path),
        to: path_text(global, target)?,
        kind,
        phase: match kind {
            MoveKind::Rename | MoveKind::Copy if first_move => MovePhase::Mark,
            MoveKind::Copy => MovePhase::Copy,
            _ => MovePhase::Place,
        },
    };
    record
        .save(global)
        .await
        .forward::<MoveError>("writing the external root move record")?;
    let copy = (kind == MoveKind::Copy).then(|| CopyRun {
        progress: Arc::new(copy_progress(global)),
        on_copying: Arc::new(on_copying),
    });
    let progress = copy.as_ref().map(|copy| Arc::clone(&copy.progress));
    let before_switch = async {
        let phases = recovery::copying_as(
            copy,
            recovery::run_phases(global, &mut record, Some(MovePhase::Place)),
        );
        let mut phases = std::pin::pin!(phases);
        let ran = tokio::select! {
            ran = &mut phases => ran,
            () = &mut termination => {
                if let Some(progress) = &progress {
                    progress.stop();
                }
                let _ = phases.await;
                return Ok(None);
            }
        };
        ran.forward::<MoveError>("running the first phases of a move of the external root")?;
        refuse_live_mounts(&root).await?;
        tokio::select! {
            locks = lock_stores(&stores, Joining::Never, &mut on_held) => Ok(Some(
                locks.forward::<MoveError>("taking the stores of the external root")?,
            )),
            () = &mut termination => Ok(None),
        }
    };
    let locks = match before_switch.await {
        Ok(Some(locks)) => locks,
        Ok(None) => {
            undo(global).await?;
            return Err(MoveError::internal(
                "the move of the external root was cancelled, and is undone",
            ));
        }
        Err(err) => {
            undo(global).await?;
            return Err(err);
        }
    };
    let finished = recovery::finish(global, &mut record).await;
    drop(locks);
    match finished {
        Ok(redirect) => Ok(Moved {
            kind: Some(kind),
            redirect,
            recovered,
        }),
        Err(err) => {
            #[cfg(feature = "test-util")]
            crate::test_util::move_step(crate::test_util::MoveStep::SwitchFailed).await;
            match recovery::recover(
                global,
                Joining::Never,
                Locking::Restarting(&mut on_held),
                &mut termination,
            )
            .await
            {
                Ok(_) => Err(err.forward::<MoveError>(
                    "running the final switch of a move of the external root",
                )),
                Err(recovery) => Err(err.forward_with::<MoveError, _>(|| {
                    format!(
                        "running the final switch of a move of the external root, whose recovery \
                         then failed: {recovery}"
                    )
                })),
            }
        }
    }
}

/// The progress of a move's copy, which holds the copy once it has copied an entry until it is
/// stopped where a test overrides the moves of `global` with
/// [`crate::test_util::MoveOverride::CopyHeld`].
#[cfg_attr(not(feature = "test-util"), allow(unused_variables))]
fn copy_progress(global: &Path) -> crate::fs::walk::Progress {
    #[cfg(feature = "test-util")]
    if crate::test_util::move_override(global) == Some(crate::test_util::MoveOverride::CopyHeld) {
        return crate::fs::walk::Progress::holding();
    }
    crate::fs::walk::Progress::default()
}

/// Undoes the move recorded in `global`, which hasn't reached its final switch.
async fn undo(global: &Path) -> Result<(), MoveError> {
    let Some(record) = blocking(global, |global| super::record::MoveRecord::load(&global)).await?
    else {
        return Ok(());
    };
    recovery::undo(global, &record)
        .await
        .forward::<MoveError>("undoing a move of the external root")?;
    Ok(())
}

/// The path a move record names for the absolute path `target`: empty for the default location of
/// `global`. Refuses a relative or non-UTF-8 `target`.
pub(super) fn path_text(global: &Path, target: &Path) -> Result<String, MoveError> {
    if !target.is_absolute() {
        return Err(refused(format!(
            "the target {} isn't an absolute path",
            target.display()
        )));
    }
    if target == global.join(EXTERNAL) {
        return Ok(String::new());
    }
    target
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| refused(format!("the target {} isn't valid UTF-8", target.display())))
}

/// Refuses `target` where it lies inside the path of a virtual instance of `root` registered in
/// one of `stores`, read under the stores' locks, taken as [`lock_stores`] takes them.
async fn refuse_registered_target(
    root: &Path,
    stores: &[PathBuf],
    target: &Path,
    on_held: &mut (impl FnMut(&Path) + Send),
) -> Result<(), MoveError> {
    if stores.is_empty() {
        return Ok(());
    }
    let _locks = lock_stores(stores, Joining::Never, on_held)
        .await
        .forward::<MoveError>("taking the stores of the external root")?;
    for store in stores {
        let repository = registry_entry(store)?
            .create_null_repository_context()
            .await
            .forward_any::<MoveError>("opening a shared store of the external root")?;
        let instances = list_instances(&repository)
            .await
            .forward_any::<MoveError>("listing the instances of a shared store")?;
        for instance in instances {
            let in_root = root.join(instance.instance_id.text_encoding());
            if target.starts_with(&instance.path)
                && lore_io::IoDriver::global()
                    .symlink_stat(&in_root)
                    .await
                    .is_ok()
            {
                return Err(refused(format!(
                    "the target {} lies inside the virtual instance at {}",
                    target.display(),
                    instance.path
                )));
            }
        }
    }
    Ok(())
}

/// The registry's entry of the shared store at `store`, a directory named [`SHARED_STORE_DIR`].
fn registry_entry(store: &Path) -> Result<SharedStoreRegistryEntry, MoveError> {
    match (store.file_name(), store.parent().and_then(Path::to_str)) {
        (Some(name), Some(dir)) if name == SHARED_STORE_DIR => {
            Ok(SharedStoreRegistryEntry::in_directory(dir.to_owned()))
        }
        _ => Err(MoveError::internal(format!(
            "the shared store {} isn't a directory named {SHARED_STORE_DIR}",
            store.display()
        ))),
    }
}

/// Refuses with `MountsBusy` while the mount table lists a live mount of a virtual instance in
/// `root`, and fails where it can't be read.
pub(super) async fn refuse_live_mounts(root: &Path) -> Result<(), MoveError> {
    let live = blocking(root, |root| live_mounts_in(&root)).await?;
    if live.is_empty() {
        return Ok(());
    }
    let paths: Vec<String> = live.iter().map(|path| path.display().to_string()).collect();
    Err(MoveError::from(MountsBusy {
        paths: paths.join(", "),
    }))
}

/// Runs the blocking `work` on `path` on the blocking pool.
pub(super) async fn blocking<T: Send + 'static>(
    path: &Path,
    work: impl FnOnce(PathBuf) -> Result<T, super::ExternalDirError> + Send + 'static,
) -> Result<T, MoveError> {
    let path = path.to_path_buf();
    lore_base::lore_spawn_blocking!(move || work(path))
        .await
        .internal("running a step of a move of the external root")?
        .forward::<MoveError>("running a step of a move of the external root")
}
