// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! This process's shared or exclusive access to the external root of each global data directory
//! it serves, one entry per canonical path, over the OS lock on `external.lock`.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;

use lore_base::fs::lock::FSLock;
use lore_error_set::prelude::*;
use parking_lot::Mutex;

use super::EXTERNAL;
use super::ExternalDirError;
use super::canonical_global;
use super::resolve::RootState;
use super::resolve::resolve_in;
use crate::global::get_global_data_dir;

/// The lock this process holds on one external root: shared for the life of the process, or
/// exclusive while a move of the root waits for recovery.
struct Access {
    exclusive: bool,
    _lock: FSLock,
}

/// The access this process holds, by the canonical path of each global data directory.
static ACCESS: LazyLock<Mutex<HashMap<Arc<Path>, Access>>> = LazyLock::new(Default::default);

/// The external root of the caller's global data directory, resolved as [`resolve_shared_in`]
/// resolves it. Blocking.
pub fn resolve_shared() -> Result<RootState, ExternalDirError> {
    Ok(resolve_in(require_shared_for_caller()?))
}

/// The external root of the global data directory `global`, resolved once this process holds
/// shared access to it, so no move of the root comes between. Blocking.
pub fn resolve_shared_in(global: PathBuf) -> Result<RootState, ExternalDirError> {
    require_shared(&global)?;
    Ok(resolve_in(global))
}

/// Takes shared access to the external root of the caller's global data directory, as
/// [`require_shared`] does, and answers that directory. Blocking.
pub fn require_shared_for_caller() -> Result<PathBuf, ExternalDirError> {
    let global =
        get_global_data_dir().forward::<ExternalDirError>("finding the global data directory")?;
    require_shared(&global)?;
    Ok(global)
}

/// Takes shared access to the external root of `global` for the life of the process, without
/// waiting. Fails while a move holds exclusive access, here or in another process. Blocking.
pub fn require_shared(global: &Path) -> Result<(), ExternalDirError> {
    let key = canonical_global(global)?;
    let mut access = ACCESS.lock();
    match access.get(&key) {
        Some(held) if held.exclusive => Err(refused()),
        Some(_) => Ok(()),
        None => take_shared(&mut access, key),
    }
}

/// Takes exclusive access to the external root of `global` for recovery, without waiting, giving
/// up this process's shared access for the attempt, and answers whether it holds it. Blocking.
pub fn try_exclusive(global: &Path) -> Result<bool, ExternalDirError> {
    let key = canonical_global(global)?;
    let mut access = ACCESS.lock();
    if access.get(&key).is_some_and(|held| held.exclusive) {
        return Ok(true);
    }
    access.remove(&key);
    let Some(lock) = FSLock::try_acquire_file_lock(key.join(EXTERNAL))
        .internal("taking exclusive access to the external root")?
    else {
        return Ok(false);
    };
    access.insert(
        key,
        Access {
            exclusive: true,
            _lock: lock,
        },
    );
    Ok(true)
}

/// Takes exclusive access to the external root of `global` for a move, without waiting: a lock the
/// caller holds until it drops, outside this process's entries. `None` while another holder has
/// access. Blocking.
pub fn try_exclusive_for_move(global: &Path) -> Result<Option<FSLock>, ExternalDirError> {
    Ok(
        FSLock::try_acquire_file_lock(canonical_global(global)?.join(EXTERNAL))
            .internal("taking exclusive access to the external root")?,
    )
}

/// Releases exclusive access to the external root of `global` into shared access, without
/// waiting. Fails, holding none, where another process took exclusive access since. Blocking.
pub fn release_exclusive(global: &Path) -> Result<(), ExternalDirError> {
    let key = canonical_global(global)?;
    let mut access = ACCESS.lock();
    match access.get(&key) {
        Some(held) if !held.exclusive => Ok(()),
        Some(_) => {
            access.remove(&key);
            take_shared(&mut access, key)
        }
        None => take_shared(&mut access, key),
    }
}

/// Releases the access this process holds to the external root of `global`, whether shared or
/// exclusive. Blocking.
pub fn release(global: &Path) -> Result<(), ExternalDirError> {
    let key = canonical_global(global)?;
    ACCESS.lock().remove(&key);
    Ok(())
}

/// Takes the shared OS lock of the global data directory whose canonical path is `key`, which
/// `access` holds no entry for, and records it there.
fn take_shared(
    access: &mut HashMap<Arc<Path>, Access>,
    key: Arc<Path>,
) -> Result<(), ExternalDirError> {
    let lock = FSLock::try_acquire_shared_file_lock(key.join(EXTERNAL))
        .internal("taking shared access to the external root")?
        .ok_or_else(refused)?;
    access.insert(
        key,
        Access {
            exclusive: false,
            _lock: lock,
        },
    );
    Ok(())
}

/// The error of a call refused shared access.
fn refused() -> ExternalDirError {
    ExternalDirError::internal(
        "the external root can't be used: a move of it holds exclusive access, or waits for \
         recovery",
    )
}
