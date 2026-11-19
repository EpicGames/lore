// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The locks of a move's final switch: the shared stores that the virtual instances of one root
//! use, read from their configs, and the stores' locks, taken one at a time in path order with
//! bounded tries.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lore_base::error::MountsBusy;
use lore_base::fs::lock::LockWait;
use lore_error_set::prelude::*;
use lore_storage::local::store_lock::Attempt;
use lore_storage::local::store_lock::Intent;
use lore_storage::local::store_lock::StoreGuard;
use lore_storage::local::store_lock::StoreLock;

use crate::instance::InstanceId;
use crate::repository::CONFIG;
use crate::repository::DOT_LORE;
use crate::repository::RepositoryConfig;

#[error_set]
pub enum LocksError {
    MountsBusy,
}

/// How long one lock is polled before every lock is released and the order starts over.
pub const LOCK_POLL: Duration = Duration::from_secs(3);

/// How a holder treats a hold of the store that its own process has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Joining {
    /// Refuses it, as the command does, since a joined hold doesn't keep the process's own work
    /// out of the store.
    Never,
    /// Joins it, as recovery in the service does.
    OwnProcess,
}

/// What one pass over the store set found.
pub enum Taken {
    /// Every lock, held until this drops.
    Locked(StoreLocks),
    /// The store whose lock stayed held for the whole poll. Every lock is released.
    Held(PathBuf),
}

/// The locks of every store of a store set, held until this drops.
pub struct StoreLocks {
    _guards: Vec<StoreGuard>,
}

/// The shared stores that the virtual instances in `root` use, in path order, each once, read
/// from the `.lore/config` of each external directory. Skips entries whose name isn't an instance
/// ID, such as the marker and instance delete's rename-aside siblings, and entries that aren't
/// directories. An external directory with no config, or whose shared store no longer exists,
/// contributes no store. A config that can't be read, or that names no shared store, refuses.
pub async fn store_set(root: &Path) -> Result<Vec<PathBuf>, LocksError> {
    let listed = root.to_path_buf();
    let configs = lore_base::lore_spawn_blocking!(move || instance_configs(&listed))
        .await
        .internal("reading the virtual instances' configs")??;
    let mut stores = Vec::with_capacity(configs.len());
    for (config_path, config) in configs {
        match crate::shared_store::locate_shared_store_for_repo(&config).await {
            Ok(Some(store)) => stores.push(store),
            Ok(None) => {
                return Err(LocksError::internal(format!(
                    "the config {} names no shared store",
                    config_path.display()
                )));
            }
            Err(err) if err.is_shared_store_not_found() => {}
            Err(err) => {
                return Err(LocksError::internal_with_context(
                    err,
                    "finding the shared store a virtual instance's config names",
                ));
            }
        }
    }
    stores.sort_unstable();
    stores.dedup();
    Ok(stores)
}

/// The path and contents of each external directory's config in `root`, as [`store_set`] reads
/// them. Blocking.
fn instance_configs(root: &Path) -> Result<Vec<(PathBuf, RepositoryConfig)>, LocksError> {
    let entries = std::fs::read_dir(root)
        .internal_with(|| format!("listing the external root {}", root.display()))?;
    let mut configs = Vec::new();
    for entry in entries {
        let entry =
            entry.internal_with(|| format!("listing the external root {}", root.display()))?;
        let is_instance = entry
            .file_name()
            .to_str()
            .and_then(InstanceId::from_hex)
            .is_some();
        if !is_instance
            || !entry
                .file_type()
                .internal_with(|| format!("examining {}", entry.path().display()))?
                .is_dir()
        {
            continue;
        }
        let dot_lore = entry.path().join(DOT_LORE);
        let config_path = dot_lore.join(CONFIG);
        match std::fs::symlink_metadata(&config_path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                #[cfg(target_family = "windows")]
                refuse_a_dot_lore_that_is_no_directory(&err, &dot_lore)?;
                continue;
            }
            Err(err) => {
                return Err(LocksError::internal_with_context(
                    err,
                    &format!("reading the config {}", config_path.display()),
                ));
            }
            Ok(_) => {}
        }
        let config = crate::util::config::load_blocking::<RepositoryConfig>(&config_path)
            .forward::<LocksError>("reading a virtual instance's config")?;
        configs.push((config_path, config));
    }
    Ok(configs)
}

/// Refuses a `dot_lore` that isn't a directory, given the `NotFound` of its config, since Windows
/// reports a path below a file as `NotFound`. A config missing from a directory costs no look.
#[cfg(target_family = "windows")]
fn refuse_a_dot_lore_that_is_no_directory(
    not_found: &std::io::Error,
    dot_lore: &Path,
) -> Result<(), LocksError> {
    if not_found.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND as i32)
    {
        return Ok(());
    }
    match std::fs::metadata(dot_lore) {
        Ok(metadata) if !metadata.is_dir() => Err(LocksError::internal(format!(
            "{} isn't a directory",
            dot_lore.display()
        ))),
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(
            LocksError::internal_with_context(err, &format!("examining {}", dot_lore.display())),
        ),
        _ => Ok(()),
    }
}

/// Takes the locks of `stores`, in their order, until every one is held: each store's immutable
/// lock, then its mutable lock, nested in it, as a command claims them. Each lock is polled as
/// [`LockWait`] paces it for up to [`LOCK_POLL`]. When a poll runs out, every lock is released,
/// `on_held` is told the store, and the order starts over. A store this process holds refuses as
/// `joining` says. Cancelled by dropping the future, which releases every lock.
pub async fn lock_stores(
    stores: &[PathBuf],
    joining: Joining,
    mut on_held: impl FnMut(&Path),
) -> Result<StoreLocks, LocksError> {
    loop {
        match try_lock_stores(stores, joining, LOCK_POLL).await? {
            Taken::Locked(locks) => return Ok(locks),
            Taken::Held(store) => on_held(&store),
        }
    }
}

/// The locks of `stores`, taken as [`lock_stores`] takes them after one attempt at each, so that
/// free locks are taken whether or not `cancel` has completed. `None` where `cancel` completes
/// while a held lock is waited for.
pub async fn lock_stores_until(
    stores: &[PathBuf],
    joining: Joining,
    on_held: impl FnMut(&Path),
    cancel: impl Future<Output = ()>,
) -> Result<Option<StoreLocks>, LocksError> {
    if let Taken::Locked(locks) = try_lock_stores(stores, joining, Duration::ZERO).await? {
        return Ok(Some(locks));
    }
    tokio::select! {
        locks = lock_stores(stores, joining, on_held) => locks.map(Some),
        () = cancel => Ok(None),
    }
}

/// One pass of [`lock_stores`], with each lock polled for up to `poll`: a zero `poll` makes one
/// attempt at each. A store directory that no longer exists is skipped, since taking its lock
/// would make it again.
pub async fn try_lock_stores(
    stores: &[PathBuf],
    joining: Joining,
    poll: Duration,
) -> Result<Taken, LocksError> {
    let mut guards = Vec::with_capacity(2 * stores.len());
    for store in stores {
        let immutable_path = store.join("immutable");
        if !is_directory(&immutable_path).await {
            continue;
        }
        let immutable = StoreLock::new(&immutable_path, false)
            .internal_with(|| format!("opening the lock of {}", immutable_path.display()))?;
        let Some(guard) = poll_lock(&immutable, store, joining, poll).await? else {
            return Ok(Taken::Held(store.clone()));
        };
        guards.push(guard);
        let mutable_path = store.join("mutable");
        if !is_directory(&mutable_path).await {
            continue;
        }
        let mutable = StoreLock::new_within(&mutable_path, false, &immutable)
            .internal_with(|| format!("opening the lock of {}", mutable_path.display()))?;
        let Some(guard) = poll_lock(&mutable, store, joining, poll).await? else {
            return Ok(Taken::Held(store.clone()));
        };
        guards.push(guard);
    }
    Ok(Taken::Locked(StoreLocks { _guards: guards }))
}

/// Whether `path` is a directory.
async fn is_directory(path: &Path) -> bool {
    lore_io::IoDriver::global()
        .metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_dir())
}

/// Polls `lock`, of `store`, as [`LockWait`] paces it for up to `poll`, for a read, which advances
/// no epoch. `None` where it stays held for the whole poll. The lock holds no store state, so it
/// adopts the store's epoch at once, which leaves the stores this process opens under it to load
/// their own. With [`Joining::Never`], a hold of this process refuses with `MountsBusy` naming
/// `store`.
async fn poll_lock(
    lock: &Arc<StoreLock>,
    store: &Path,
    joining: Joining,
    poll: Duration,
) -> Result<Option<StoreGuard>, LocksError> {
    let deadline = tokio::time::Instant::now() + poll;
    let mut wait = LockWait::new();
    loop {
        let attempt = match joining {
            Joining::Never => lock.try_acquire_unjoined(Intent::Read).await,
            Joining::OwnProcess => lock
                .try_acquire(Intent::Read)
                .await
                .map(|guard| guard.map_or(Attempt::HeldElsewhere, Attempt::Acquired)),
        }
        .internal_with(|| format!("taking the lock of {}", store.display()))?;
        match attempt {
            Attempt::Acquired(mut guard) => {
                guard.note_refreshed();
                return Ok(Some(guard));
            }
            Attempt::HeldByThisProcess => {
                return Err(LocksError::from(MountsBusy {
                    paths: store.display().to_string(),
                }));
            }
            Attempt::HeldElsewhere if tokio::time::Instant::now() >= deadline => return Ok(None),
            Attempt::HeldElsewhere => wait.next(store).await,
        }
    }
}
