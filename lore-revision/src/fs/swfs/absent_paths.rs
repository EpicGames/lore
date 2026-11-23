// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The paths an SWFS instance's current revision holds that its working tree does not
//! (ADR-00034), kept in the mutable store so that a later mount serves the tree the same.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use lore_base::types::Hash;
use lore_error_set::prelude::*;
use lore_storage::FlushOptions;
use lore_storage::KeyType;
use parking_lot::RwLock;

use crate::errors::UnhandledError;
use crate::fs::swfs::paths::self_and_ancestors;
use crate::immutable;
use crate::lore::Address;
use crate::lore::Context;
use crate::lore_warn;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::util::path::RelativePath;

const ABSENT_KEY: &str = "swfs-absent-paths";

/// The saves of each instance's set in flight in the process, by the key the set is stored under.
/// A save is counted from when it is queued, so a mount of the instance made before it has
/// written, as when the service remounts the instance straight after an operation, waits for it
/// rather than loading the set it replaces ([`AbsentPaths::load`]).
static SAVES_IN_FLIGHT: LazyLock<
    parking_lot::Mutex<HashMap<Hash, tokio::sync::watch::Sender<usize>>>,
> = LazyLock::new(Default::default);

/// A save of a set counted in flight, from when it is queued until it is dropped: once written, or
/// when the task saving it is cancelled, so a mount never waits on a save that will not end.
struct SaveInFlight(tokio::sync::watch::Sender<usize>);

impl SaveInFlight {
    fn begin(key: Hash) -> Self {
        let saves = saves_in_flight(key);
        saves.send_modify(|in_flight| *in_flight += 1);
        Self(saves)
    }
}

impl Drop for SaveInFlight {
    fn drop(&mut self) {
        self.0.send_modify(|in_flight| *in_flight -= 1);
    }
}

/// The count of `key`'s saves in flight.
fn saves_in_flight(key: Hash) -> tokio::sync::watch::Sender<usize> {
    SAVES_IN_FLIGHT
        .lock()
        .entry(key)
        .or_insert_with(|| tokio::sync::watch::channel(0).0)
        .clone()
}

/// Lowercase paths left out of what the mount serves: what an operation that moved the instance
/// to its revision did not write. A path below an absent directory is absent with it.
#[derive(Debug)]
pub struct AbsentPaths {
    repository: Arc<RepositoryContext>,
    paths: RwLock<HashSet<String>>,
    /// Held across a save, so saves do not interleave their writes.
    saving: tokio::sync::Mutex<()>,
    /// Set while a save is queued and has not begun, so changes made before then share it.
    save_queued: AtomicBool,
}

impl AbsentPaths {
    /// The paths an earlier mount of the instance saved, or none, once every save of them queued
    /// in the process has written. A set that cannot be read is logged and taken as empty: the
    /// tree then shows the paths it held.
    ///
    /// A queued save takes the repository's write lock, so a caller must not hold it: a mount
    /// is made as an instance is created, before any save of it, at the service's start, before
    /// any operation, or as an instance moves, outside the lock.
    pub async fn load(repository: Arc<RepositoryContext>) -> Arc<Self> {
        let mut saves = saves_in_flight(absent_key(&repository)).subscribe();
        // The sender is held in the map for the process's life, so the wait ends only at zero.
        let _ = saves.wait_for(|in_flight| *in_flight == 0).await;
        let paths = match load_paths(&repository).await {
            Ok(paths) => paths,
            Err(error) => {
                lore_warn!("The paths absent from the SWFS instance could not be read: {error}");
                HashSet::new()
            }
        };
        Arc::new(Self {
            repository,
            paths: RwLock::new(paths),
            saving: tokio::sync::Mutex::new(()),
            save_queued: AtomicBool::new(false),
        })
    }

    /// Whether `path` or a directory above it is absent.
    pub fn contains(&self, path: &RelativePath) -> bool {
        let paths = self.paths.read();
        if paths.is_empty() {
            return false;
        }
        self_and_ancestors(&path.as_str().to_lowercase()).any(|path| paths.contains(path))
    }

    /// Records `absent` as absent and `present`, with the directories leading to each, as no
    /// longer so, and queues a save if that changed anything.
    pub fn update(self: &Arc<Self>, absent: &[RelativePath], present: &[RelativePath]) {
        let changed = {
            let mut paths = self.paths.write();
            let mut changed = false;
            for path in present {
                for path in self_and_ancestors(&path.as_str().to_lowercase()) {
                    changed |= paths.remove(path);
                }
            }
            for path in absent {
                changed |= paths.insert(path.as_str().to_lowercase());
            }
            changed
        };
        if changed {
            self.schedule_save();
        }
    }

    /// Queues a save to run on the runtime: the caller is an operation's finalize, whose command
    /// holds the repository's write lock that the save takes. Counted in flight until it has
    /// written, so a mount loading the set waits for it.
    fn schedule_save(self: &Arc<Self>) {
        if self.save_queued.swap(true, Ordering::AcqRel) {
            return;
        }
        let in_flight = SaveInFlight::begin(absent_key(&self.repository));
        let this = self.clone();
        lore_base::lore_spawn_guarded!(async move {
            let _in_flight = in_flight;
            this.save_queued.store(false, Ordering::Release);
            if let Err(error) = this.save().await {
                lore_warn!("The paths absent from the SWFS instance could not be saved: {error}");
            }
        });
    }

    async fn save(&self) -> Result<(), UnhandledError> {
        let Some(path) = self.repository.path().map(std::path::Path::to_path_buf) else {
            return Ok(());
        };
        let _saving = self.saving.lock().await;
        let acquired;
        let token = if let Some(token) = self.repository.try_write_token() {
            token
        } else {
            acquired = RepositoryWriteToken::acquire(&path).await;
            &acquired
        };

        let mut lines: Vec<String> = self.paths.read().iter().cloned().collect();
        let hash = if lines.is_empty() {
            Hash::default()
        } else {
            lines.sort_unstable();
            immutable::write(
                self.repository.clone(),
                Context::default(),
                Bytes::from(lines.join("\n")),
                immutable::write_options_from_repository(self.repository.clone()),
            )
            .await
            .forward_any::<UnhandledError>("Writing the paths absent from the SWFS instance")?
            .hash
        };

        let handle = self.repository.write_mutable_store(token);
        handle
            .store(
                self.repository.root_id(),
                absent_key(&self.repository),
                hash,
                KeyType::Untyped,
            )
            .await
            .forward_any::<UnhandledError>("Storing the paths absent from the SWFS instance")?;
        handle
            .flush(FlushOptions::default())
            .await
            .forward_any::<UnhandledError>("Flushing the paths absent from the SWFS instance")?;
        self.repository
            .immutable_store()
            .flush(FlushOptions::default())
            .await
            .forward_any::<UnhandledError>("Flushing the paths absent from the SWFS instance")?;
        Ok(())
    }
}

fn absent_key(repository: &RepositoryContext) -> Hash {
    crate::hash::hash_function_args_slice(
        repository.salt(),
        ABSENT_KEY,
        repository.instance_id.data(),
        &[],
    )
}

async fn load_paths(
    repository: &Arc<RepositoryContext>,
) -> Result<HashSet<String>, UnhandledError> {
    let hash = repository
        .read_mutable_store()
        .load(
            repository.root_id(),
            absent_key(repository),
            KeyType::Untyped,
        )
        .await
        .ok()
        .unwrap_or_default();
    if hash.is_zero() {
        return Ok(HashSet::new());
    }
    let data = immutable::read(
        repository.clone(),
        Address::zero_context_hash(hash),
        None,
        immutable::read_options_from_repository(repository),
    )
    .await
    .forward_any::<UnhandledError>("Reading the paths absent from the SWFS instance")?;
    let text = std::str::from_utf8(&data)
        .internal("The paths absent from the SWFS instance are not UTF-8")?;
    Ok(text
        .split('\n')
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}
