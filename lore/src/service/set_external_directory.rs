// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore service set-external-directory`: the checks before the stop, the dry run's report, the
//! stop of the service, the move, an adopt or an abandon, and the service's start once it ends.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use futures::FutureExt;
use lore_base::error::InvalidArguments;
use lore_base::error::MountsBusy;
use lore_error_set::prelude::*;
use lore_revision::event::EventError;
use lore_revision::fs::walk::Progress;
use lore_revision::fs::walk::TreeSize;
use lore_revision::global::external_dir::check::check_new_root;
use lore_revision::global::external_dir::check::check_target;
use lore_revision::global::external_dir::check::measure_root;
use lore_revision::global::external_dir::check::refuse_linked_root;
use lore_revision::global::external_dir::check::require_copy_space;
use lore_revision::global::external_dir::moving::Moved;
use lore_revision::global::external_dir::moving::move_root;
use lore_revision::global::external_dir::record::MoveKind;
use lore_revision::global::external_dir::record::MoveRecord;
use lore_revision::global::external_dir::recovery::require_owned;
use lore_revision::global::external_dir::redirect::Redirect;
use lore_revision::global::external_dir::refused;
use lore_revision::global::external_dir::resolve::Availability;
use lore_revision::global::external_dir::resolve::checked_marker;
use lore_revision::global::external_dir::resolve::resolve_in;
use lore_revision::global::external_dir::resolve::root_available;
use lore_revision::global::external_dir::resolve::root_path;
use lore_revision::global::external_dir::setting::Setting;
use lore_revision::global::external_dir::takeover::abandon;
use lore_revision::global::external_dir::takeover::adopt;
use lore_revision::global::get_global_data_dir;
use lore_revision::instance::InstanceId;
use lore_revision::lore_info;
use lore_revision::lore_warn;

use crate::remote::service_process::service_runs_in_this_process;
use crate::service::external_root::FoundService;
use crate::service::external_root::Restart;
use crate::service::external_root::WAIT_BOUND;
use crate::service::external_root::find_service;
use crate::service::external_root::refuse_reported_service;
use crate::service::external_root::service_of_this_program;
use crate::service::external_root::socket_held;
use crate::service::external_root::stop_for_move;

#[error_set]
pub enum SetExternalDirectoryError {
    InvalidArguments,
    MountsBusy,
}

impl EventError for SetExternalDirectoryError {}

/// What the command does with its target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Form {
    /// Moves every external directory to the target.
    Move,
    /// Points the setting at the target, which holds the current root, and moves nothing.
    Adopt,
    /// Gives up the unavailable current root for a new, empty one at the target.
    Abandon,
}

/// What the checks before the stop found.
struct Checked {
    /// How the root moves, or `None` where the target is the current root.
    kind: Option<MoveKind>,
    /// The current root, where it is available.
    root: Option<PathBuf>,
    /// The move recorded as interrupted, if any.
    interrupted: Option<MoveRecord>,
}

/// Runs the command for `form` with `path`, relative to `working_directory` where it isn't
/// absolute, or the default location where empty. Runs every check that needs nothing stopped,
/// and with `dry_run` reports what it would do and stops there. Otherwise stops the service, as
/// `lore service stop` does with `force`, runs the move until `termination`, which undoes a move
/// before its final switch, and starts the service again. `termination` before the stop ends
/// the command, and while the service starts again ends the wait for it to answer.
pub(crate) async fn set_external_directory(
    path: &str,
    form: Form,
    dry_run: bool,
    force: bool,
    working_directory: Option<PathBuf>,
    termination: impl Future<Output = ()>,
) -> Result<(), SetExternalDirectoryError> {
    if service_runs_in_this_process() {
        return Err(service_of_this_program().into());
    }
    let global = get_global_data_dir()
        .forward_any::<SetExternalDirectoryError>("finding the global data directory")?;
    let target = target_path(&global, path, working_directory.as_deref())?;
    let mut termination = std::pin::pin!(termination.fuse());
    let progress = Arc::new(Progress::default());
    let checking = {
        let (global, target, progress) = (global.clone(), target.clone(), Arc::clone(&progress));
        lore_base::lore_spawn_blocking!(move || {
            check_before_stop(&global, &target, form, dry_run, &progress)
        })
    };
    let checked = tokio::select! {
        checked = checking => checked.internal("checking the target of the external root")??,
        () = &mut termination => {
            progress.stop();
            return Err(cancelled());
        }
    };
    if form == Form::Move && checked.kind.is_none() && checked.interrupted.is_none() {
        lore_info!("{} is the external root already", target.display());
        return Ok(());
    }
    let found = tokio::select! {
        found = find_service() => {
            found.forward::<SetExternalDirectoryError>("reading the Lore service's status")?
        }
        () = &mut termination => return Err(cancelled()),
    };
    if let FoundService::Reporting(status) = &found {
        let canonical =
            lore_revision::global::external_dir::canonical_global(&global)
                .forward::<SetExternalDirectoryError>("finding the global data directory")?;
        refuse_reported_service(status, &canonical, &target)
            .forward::<SetExternalDirectoryError>("checking the running Lore service")?;
    }
    #[cfg(target_family = "windows")]
    tokio::select! {
        probed = probe_serving_executable(match &found {
            FoundService::Reporting(status) => Some(Path::new(status.binary_path.as_str())),
            _ => None,
        }) => probed?,
        () = &mut termination => return Err(cancelled()),
    }
    if matches!(found, FoundService::Moving) {
        return Err(socket_held().into());
    }
    if dry_run {
        report_dry_run(&global, &target, form, &checked, &found);
        return Ok(());
    }

    let stopped = stop_for_move(&global, &target, found, force, WAIT_BOUND, &mut termination)
        .await
        .forward::<SetExternalDirectoryError>("stopping the Lore service for the move")?;
    let on_held = |store: &Path| {
        lore_info!(
            "Waiting for the shared store {} to be free, which a program holds open",
            store.display()
        );
    };
    let on_copying = |name: &str| lore_info!("Copying the external directory {name}");
    let moved = match form {
        Form::Move => {
            move_root(
                &global,
                &target,
                stopped.access(),
                &mut termination,
                on_held,
                on_copying,
            )
            .await
        }
        Form::Adopt => {
            adopt(
                &global,
                &target,
                stopped.access(),
                &mut termination,
                on_held,
            )
            .await
        }
        Form::Abandon => {
            abandon(
                &global,
                &target,
                stopped.access(),
                &mut termination,
                on_held,
            )
            .await
        }
    };
    let restart = stopped.end(&mut termination).await;
    let moved = moved.forward::<SetExternalDirectoryError>("moving the external root")?;
    report_moved(&target, &moved, checked.root.as_deref());
    match restart {
        Restart::Started => lore_info!("The Lore service is running again"),
        Restart::Unawaited => lore_info!("The Lore service is starting again"),
        Restart::NoneStopped | Restart::Failed => {}
    }
    Ok(())
}

/// The failure of a command cancelled before it stopped the service, which changed nothing.
fn cancelled() -> SetExternalDirectoryError {
    SetExternalDirectoryError::internal(
        "cancelled before the Lore service was stopped, with nothing changed",
    )
}

/// The absolute path of the target `path` names, relative to `working_directory`, or the default
/// location of `global` where it is empty, with no `.` or `..` component left.
fn target_path(
    global: &Path,
    path: &str,
    working_directory: Option<&Path>,
) -> Result<PathBuf, SetExternalDirectoryError> {
    if path.is_empty() {
        return Ok(global.join(lore_revision::global::external_dir::EXTERNAL));
    }
    let absolute =
        lore_revision::util::path::make_absolute_from(path, working_directory).map_err(|err| {
            refused::<SetExternalDirectoryError>(format!(
                "the target {path} can't be resolved: {err}"
            ))
        })?;
    Ok(without_steps(&absolute))
}

/// The absolute `path` with each `.` component dropped and each `..` taking off the component
/// before it, as the path reads, not as links resolve.
fn without_steps(path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    resolved
}

/// The checks on the target of `form` that need nothing stopped, for `global`. A dry run also
/// measures the current root, until `progress` is stopped. Blocking.
fn check_before_stop(
    global: &Path,
    target: &Path,
    form: Form,
    dry_run: bool,
    progress: &Progress,
) -> Result<Checked, SetExternalDirectoryError> {
    let interrupted = MoveRecord::load(global)
        .forward::<SetExternalDirectoryError>("reading the external root move record")?;
    let setting = Setting::load(global)
        .forward::<SetExternalDirectoryError>("reading the external root setting")?;
    let state = resolve_in(global.to_path_buf());
    let root = match &state.availability {
        Availability::Available(root) => Some(root.clone()),
        _ => None,
    };
    let mut measured = None;
    let kind = match form {
        Form::Move if interrupted.is_some() => None,
        Form::Move => {
            let kind = check_target(&state, target)
                .forward::<SetExternalDirectoryError>("checking the target")?;
            if kind == Some(MoveKind::Copy) {
                measured = Some(
                    require_copy_space(&state, target, progress)
                        .forward::<SetExternalDirectoryError>("checking the target")?,
                );
            }
            if matches!(kind, Some(MoveKind::Rename | MoveKind::Copy))
                && setting.is_none()
                && let Some(root) = &root
            {
                require_owned(root)
                    .forward::<SetExternalDirectoryError>("checking the current root")?;
            }
            kind
        }
        Form::Adopt => {
            refuse_linked_root(target)
                .forward::<SetExternalDirectoryError>("checking the target")?;
            if let Err(reason) = checked_marker(global, target, setting.as_ref()) {
                return Err(refused(format!(
                    "{} can't be adopted: {}",
                    target.display(),
                    reason.describe()
                )));
            }
            Some(MoveKind::Adopt)
        }
        Form::Abandon => {
            let Some(setting) = &setting else {
                return Err(refused(
                    "external-root is missing, so there is no root ID to keep: adopt the root \
                     first"
                        .to_string(),
                ));
            };
            if root_available(global, &setting.path, Some(setting)) {
                return Err(refused(
                    "the external root is available, so it isn't abandoned: adopt it instead"
                        .to_string(),
                ));
            }
            check_new_root(global, target)
                .forward::<SetExternalDirectoryError>("checking the target")?;
            Some(MoveKind::Abandon)
        }
    };
    if dry_run && let Some(root) = &root {
        report_root(root, progress, measured);
    }
    Ok(Checked {
        kind,
        root,
        interrupted,
    })
}

/// Reports how many external directories the root at `root` holds and what a copy of them
/// would take: `measured`, where the checks measured it, and otherwise measured until `progress`
/// is stopped. Blocking.
fn report_root(root: &Path, progress: &Progress, measured: Option<TreeSize>) {
    let count = std::fs::read_dir(root).map_or(0, |entries| {
        entries
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .and_then(InstanceId::from_hex)
                    .is_some()
            })
            .count()
    });
    let measured = measured.map_or_else(
        || {
            let block = lore_io::clone::space_of(root).map_or(4096, |space| space.block);
            measure_root(root, block, progress)
        },
        Ok,
    );
    match measured {
        Ok(size) => lore_info!(
            "The external root {} holds {count} external directories, {} bytes",
            root.display(),
            size.size
        ),
        Err(_) if progress.is_stopped() => {}
        Err(err) => lore_warn!(
            "Unable to measure the external root {}: {err}",
            root.display()
        ),
    }
}

/// Reports what the command would do for `form` with `target` in `global`, as `checked` and
/// `found`, which is no other move, say.
fn report_dry_run(
    global: &Path,
    target: &Path,
    form: Form,
    checked: &Checked,
    found: &FoundService,
) {
    if let Some(record) = &checked.interrupted {
        lore_info!(
            "A move of the external root from {} to {} is interrupted, which the command finishes \
             or undoes first",
            root_path(global, &record.from).display(),
            root_path(global, &record.to).display()
        );
    }
    match found {
        FoundService::Reporting(status) if status.process_id != 0 => lore_info!(
            "Would stop the Lore service (process {}) and start it again from {}",
            status.process_id,
            status.binary_path.as_str()
        ),
        FoundService::Reporting(_) => {
            lore_info!("Would stop the Lore service, which reports no process, and start none");
        }
        FoundService::OtherBuild => {
            lore_info!("Would stop the Lore service, of another build, and start none");
        }
        FoundService::None => lore_info!("No Lore service is running"),
        FoundService::Moving => {}
    }
    match (form, checked.kind) {
        (Form::Adopt, _) => lore_info!("Would adopt {} as the external root", target.display()),
        (Form::Abandon, _) => lore_info!(
            "Would abandon the external root for a new, empty one at {}",
            target.display()
        ),
        (Form::Move, Some(MoveKind::Create)) => {
            lore_info!("Would create the external root at {}", target.display());
        }
        (Form::Move, Some(MoveKind::Copy)) => lore_info!(
            "Would move the external root to {} by copying it, after which each FUSE instance \
             reads and hashes the files in its overlay again",
            target.display()
        ),
        (Form::Move, Some(_)) => lore_info!(
            "Would move the external root to {} by renaming it",
            target.display()
        ),
        (Form::Move, None) => {}
    }
}

/// Reports what the move to `target` did, from the root `from`.
fn report_moved(target: &Path, moved: &Moved, from: Option<&Path>) {
    let from = from.map_or_else(String::new, |from| format!(" from {}", from.display()));
    match moved.kind {
        Some(MoveKind::Rename) => {
            lore_info!(
                "Moved the external root{from} to {} by rename",
                target.display()
            );
        }
        Some(MoveKind::Create) => lore_info!("Created the external root at {}", target.display()),
        Some(MoveKind::Adopt) => lore_info!("Adopted {} as the external root", target.display()),
        Some(MoveKind::Abandon) => lore_info!(
            "Abandoned the external root{from} for a new, empty one at {}",
            target.display()
        ),
        Some(MoveKind::Copy) => lore_info!(
            "Moved the external root{from} to {} by copying it",
            target.display()
        ),
        None => lore_info!("{} is the external root", target.display()),
    }
    if let Redirect::Kept(occupant) = &moved.redirect {
        lore_warn!(
            "The default location of the external root holds what the move didn't make, which is \
             kept: {occupant:?}"
        );
    }
}

/// Refuses, on Windows, unless the executable that serves after the move, the configured one
/// unless it is `restarts`, the binary of the service the move starts again, runs the command's
/// subcommand.
#[cfg(target_family = "windows")]
async fn probe_serving_executable(
    restarts: Option<&Path>,
) -> Result<(), SetExternalDirectoryError> {
    let (configured, _) = crate::remote::service_process::service_executable().await;
    let (configured, restarts) = (configured.ok(), restarts.map(Path::to_path_buf));
    lore_base::lore_spawn_blocking!(move || {
        lore_revision::global::external_dir::check::probe_service_executable(
            configured.as_deref(),
            restarts.as_deref(),
            std::time::Duration::from_secs(10),
        )
    })
    .await
    .internal("probing the configured Lore service executable")?
    .forward::<SetExternalDirectoryError>("probing the configured Lore service executable")
}
