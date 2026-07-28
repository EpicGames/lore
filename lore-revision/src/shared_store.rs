// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::path::PathBuf;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::error::LoreErrorExt;
use crate::errors::*;
use crate::event::EventError;
use crate::event::LoreEvent;
use crate::global;
use crate::global::GlobalConfig;
use crate::global::save_config;
use crate::interface::LoreArray;
use crate::interface::LoreError;
use crate::interface::LoreString;
use crate::lore::RepositoryId;
use crate::lore::execution_context;
use crate::lore_warn;
use crate::protocol;
use crate::repository::RepositoryConfig;
use crate::repository::SharedStoreToUseConfig;
use crate::repository::StoreConfig;
use crate::store::immutable;
use crate::store::immutable::ImmutableStoreCreateOptions;
use crate::store::immutable::ImmutableStoreSettings;
use crate::store::mutable;
use crate::util;
use crate::util::url::normalize_remote_url;

#[error_set]
pub enum SharedStoreError {
    SharedStoreNotFound,
    AddressNotFound,
    Disconnected,
    InvalidPath,
    Maintenance,
    NoRemote,
    NotAuthenticated,
    NotAuthorized,
    NotFound,
    NotSupported,
    Oversized,
    PayloadNotFound,
    SlowDown,
}

impl EventError for SharedStoreError {
    fn translated(&self) -> LoreError {
        match self {
            SharedStoreError::InvalidPath(_) => LoreError::InvalidArguments,
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct SharedStoreConfig {
    pub remote_url: Option<String>,
    pub store_config: Option<StoreConfig>,
}

// Name of the shared store directory created inside the user provided directory
pub const SHARED_STORE_DIR: &str = "shared_store";
// Inside SHARED_STORE_DIR
pub const SHARED_STORE_CONFIG: &str = "shared_store.toml";

pub fn find_existing_shared_store_in_dir(
    containing_dir_path: impl AsRef<Path>,
) -> Result<PathBuf, SharedStoreError> {
    // Try new name first
    let shared_store_path = containing_dir_path.as_ref().join(SHARED_STORE_DIR);
    if shared_store_path.exists() {
        return Ok(shared_store_path);
    }

    // Fall back to old name for backwards compatibility
    let global_store_path = containing_dir_path.as_ref().join("global_store");
    if global_store_path.exists() {
        return Ok(global_store_path);
    }

    // Neither exists, return error with new name
    Err(SharedStoreNotFound {
        path: shared_store_path.display().to_string(),
    }
    .into())
}

/// Data for an event reporting that a shared store was created.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreSharedStoreCreateEventData {
    /// Filesystem path of the created shared store.
    pub path: LoreString,
}

pub async fn create_shared_store(
    path: Option<PathBuf>,
    remote_url: String,
    make_default: bool,
) -> Result<(), SharedStoreError> {
    let execution = execution_context();
    let global_cli_args = execution.globals();

    if global_cli_args.offline == 0 {
        let identity = global_cli_args.identity().unwrap_or_default();
        protocol::connect(&remote_url, identity, RepositoryId::default())
            .await
            .internal(&format!("Failed to connect to remote URL {remote_url}"))?;
    }

    let directory_containing_shared_store = if let Some(path) = path {
        path.join(GlobalConfig::shared_store_subdir_for_remote(&remote_url))
    } else {
        GlobalConfig::suggested_path_for_remote_url(&remote_url)
            .internal("failed to make default shared store path")?
    };
    let shared_store_path = directory_containing_shared_store.join(SHARED_STORE_DIR);

    if shared_store_path.exists() {
        if global_cli_args.force() {
            tokio::fs::remove_dir_all(&shared_store_path)
                .await
                .internal_with(|| {
                    format!("removing shared store at {}", shared_store_path.display())
                })?;
        } else {
            return SharedStoreError::internal(format!(
                "Found existing shared store at {}",
                shared_store_path.display()
            ))
            .emit();
        }
    }

    create_shared_store_at(&shared_store_path, Some(remote_url.clone())).await?;

    if make_default {
        let (mut global_config, lock) = GlobalConfig::load_locked()
            .await
            .internal("loading global config")?;
        global_config
            .set_default_path_for_remote_url(
                &remote_url,
                directory_containing_shared_store
                    .to_str()
                    .ok_or_else(|| SharedStoreError::internal("bad path"))?,
            )
            .internal("setting default shared store path")?;
        global_config
            .save(lock)
            .await
            .internal("saving global config")?;
    }

    Ok(())
}

/// Create the immutable and mutable stores and write the shared store config at
/// `shared_store_path`. Callers must ensure the path does not already contain a
/// store. Shared by the explicit `shared-store create` command and the
/// create-on-clone path in [`ensure_shared_store_for_repo`].
async fn create_shared_store_at(
    shared_store_path: &Path,
    remote_url: Option<String>,
) -> Result<(), SharedStoreError> {
    let shared_store_config = SharedStoreConfig {
        remote_url,
        store_config: Some(StoreConfig::global_default()),
    };

    let options = shared_store_config
        .store_config
        .as_ref()
        .map_or(ImmutableStoreCreateOptions::none(), |config| {
            config.to_options()
        });

    let immutable_store = immutable::create(
        Some(shared_store_path),
        options,
        false,
        ImmutableStoreSettings {
            allow_partial_fragment: true, /* Client store can have partial fragments */
            protect_local_fragment: true, /* Protect local fragments from eviction */
            verify_write: shared_store_config
                .store_config
                .as_ref()
                .and_then(|config| config.verify_write)
                .unwrap_or_default(),
            ..Default::default()
        },
    )
    .await
    .forward::<SharedStoreError>("creating immutable store")?;

    lore_storage::maintenance::spawn_gc(&immutable_store, &options);

    mutable::create(
        Some(shared_store_path),
        mutable::MutableStoreSettings::default(),
        immutable_store,
    )
    .await
    .forward::<SharedStoreError>("creating mutable store")?;

    LoreEvent::SharedStoreCreate(LoreSharedStoreCreateEventData {
        path: LoreString::from_str(&shared_store_path.display().to_string()),
    })
    .send();

    save_config(
        &shared_store_config,
        &shared_store_path.join(SHARED_STORE_CONFIG),
    )
    .await
    .internal("saving shared store config")?;

    Ok(())
}

fn remote_urls_equivalent(
    shared_store_url: &Option<String>,
    repository_url: &Option<String>,
) -> Result<bool, SharedStoreError> {
    if let Some(shared_store_url) = shared_store_url {
        if let Some(repository_url) = repository_url {
            Ok(normalize_remote_url(shared_store_url) == normalize_remote_url(repository_url))
        } else {
            Err(SharedStoreError::internal("No repository remote URL"))
        }
    } else {
        Err(SharedStoreError::internal("No shared store remote URL"))
    }
}

/// Resolve the directory that should contain the shared store for a repo: an
/// explicitly configured path, or the default location derived from the remote
/// URL. The directory is not required to exist.
async fn resolve_shared_store_dir(
    shared_store_to_use_config: &SharedStoreToUseConfig,
    remote_url: &Option<String>,
) -> Result<PathBuf, SharedStoreError> {
    if let Some(path) = &shared_store_to_use_config.shared_store_path {
        let base = util::path::make_absolute(path)
            .forward::<SharedStoreError>("resolving shared store path")?;
        Ok(base.join(GlobalConfig::shared_store_subdir_for_remote(
            remote_url
                .as_ref()
                .ok_or(SharedStoreError::internal("no remote url"))?,
        )))
    } else {
        let global_config = GlobalConfig::load()
            .await
            .internal("loading global config")?;
        Ok(global_config
            .default_shared_store_directory_for_remote(
                remote_url
                    .as_ref()
                    .ok_or(SharedStoreError::internal("no remote url"))?,
            )
            .internal("getting shared store path")?)
    }
}

/// Migrate a legacy shared store that sits directly in `base` (the pre-per-URL
/// layout, `base/shared_store`) to `target_store_path` when it records
/// `repository_url`, so an existing store is reused instead of orphaned. Returns
/// whether a store was moved.
async fn migrate_legacy_store_in_base(
    base: &Path,
    target_store_path: &Path,
    repository_url: &Option<String>,
) -> Result<bool, SharedStoreError> {
    let Ok(legacy_store_path) = find_existing_shared_store_in_dir(base) else {
        return Ok(false);
    };
    let legacy_config = load_shared_store_config(&legacy_store_path)
        .await
        .unwrap_or_default();
    if !remote_urls_equivalent(&legacy_config.remote_url, repository_url).unwrap_or(false) {
        return Ok(false);
    }

    if let Some(parent) = target_store_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .internal_with(|| format!("creating shared store directory {}", parent.display()))?;
    }
    #[allow(clippy::disallowed_methods)]
    // Authorized shared-store writer (global data dir, not repo tree).
    tokio::fs::rename(&legacy_store_path, target_store_path)
        .await
        .internal_with(|| {
            format!(
                "migrating legacy shared store {} to {}",
                legacy_store_path.display(),
                target_store_path.display()
            )
        })?;
    Ok(true)
}

/// Ensure the shared store for a repo exists, creating it when missing. This
/// lets a clone (or `repository create`) for an endpoint that has no store yet
/// succeed instead of failing with `SharedStoreNotFound`.
///
/// The store lives in a per-remote subdirectory of the base path — the default
/// data directory, or an explicit `shared_store_path` — so one base can back
/// the stores of multiple endpoints. Default-base stores are registered as the
/// remote's default location so `shared-store info` lists them; explicit-path
/// stores are pinned by the repo config instead and are not promoted globally.
pub async fn ensure_shared_store_for_repo(
    config: &RepositoryConfig,
) -> Result<(), SharedStoreError> {
    let Some(shared_store_to_use_config) = config.shared_store_to_use.as_ref() else {
        return Ok(());
    };
    if !shared_store_to_use_config.use_shared_store.unwrap_or(false) {
        return Ok(());
    }

    let directory_with_shared_store =
        resolve_shared_store_dir(shared_store_to_use_config, &config.remote_url).await?;
    if find_existing_shared_store_in_dir(&directory_with_shared_store).is_ok() {
        return Ok(());
    }

    let shared_store_path = directory_with_shared_store.join(SHARED_STORE_DIR);
    let migrated = match directory_with_shared_store.parent() {
        Some(base) => {
            migrate_legacy_store_in_base(base, &shared_store_path, &config.remote_url).await?
        }
        None => false,
    };
    if !migrated {
        lore_warn!(
            "Creating new shared store for remote {} in shared store at {}",
            config.remote_url.as_deref().unwrap_or_default(),
            shared_store_path.display()
        );
        create_shared_store_at(&shared_store_path, config.remote_url.clone()).await?;
    }

    if shared_store_to_use_config.shared_store_path.is_none()
        && let (Some(remote_url), Some(directory)) = (
            config.remote_url.as_ref(),
            directory_with_shared_store.to_str(),
        )
    {
        let (mut global_config, lock) = GlobalConfig::load_locked()
            .await
            .internal("loading global config")?;
        global_config
            .set_default_path_for_remote_url(remote_url, directory)
            .internal("setting default shared store path")?;
        global_config
            .save(lock)
            .await
            .internal("saving global config")?;
    }

    Ok(())
}

/// Given a `RepositoryConfig` either
/// - return `Ok(Some(path))` where path is the path of a shared store to use instead of .urc/immutable
/// - return `Ok(None)` indicating to use the local .urc/immutable store
/// - return `Err()`
pub async fn get_shared_store_path_for_repo(
    config: &RepositoryConfig,
) -> Result<Option<PathBuf>, SharedStoreError> {
    Ok(
        if let Some(shared_store_to_use_config) = config.shared_store_to_use.as_ref()
            && shared_store_to_use_config.use_shared_store.unwrap_or(false)
        {
            let directory_with_shared_store =
                resolve_shared_store_dir(shared_store_to_use_config, &config.remote_url).await?;
            let shared_store_path = find_existing_shared_store_in_dir(directory_with_shared_store)?;
            let shared_store_config =
                load_shared_store_config(&shared_store_path)
                    .await
                    .map_err(|_err| SharedStoreNotFound {
                        path: shared_store_path.display().to_string(),
                    })?;
            if !remote_urls_equivalent(&shared_store_config.remote_url, &config.remote_url)? {
                return Err(SharedStoreError::internal(format!(
                    "Loading the shared store for a repo with remote url \"{}\" but the shared store had the remote url \"{}\"",
                    config.remote_url.clone().unwrap_or_default(),
                    shared_store_config.remote_url.unwrap_or_default(),
                )));
            }
            Some(shared_store_path)
        } else {
            None
        },
    )
}

async fn load_shared_store_config(
    shared_store_path: impl AsRef<Path>,
) -> Result<SharedStoreConfig, SharedStoreError> {
    let shared_store_path = shared_store_path.as_ref();
    let config_path = shared_store_path.join(SHARED_STORE_CONFIG);
    let legacy_config_path = shared_store_path.join("global.toml");
    if config_path.exists() {
        global::load_config::<SharedStoreConfig>(config_path)
            .await
            .forward::<SharedStoreError>("Loading shared store config")
    } else if legacy_config_path.exists() {
        // If a config is found at the old location, save it to the correct location and delete the
        // original
        let legacy_config = global::load_config::<SharedStoreConfig>(&legacy_config_path)
            .await
            .forward::<SharedStoreError>("Loading legacy shared store config");
        if let Ok(legacy_config) = legacy_config.as_ref() {
            save_config(legacy_config, config_path)
                .await
                .forward::<SharedStoreError>("migrating global config")?;
            util::fs::unlink(&legacy_config_path)
                .await
                .internal("removing redundant legacy global config")?;
        }
        legacy_config
    } else {
        Err(SharedStoreNotFound {
            path: shared_store_path.display().to_string(),
        }
        .into())
    }
}

/// Data for an event describing the configured shared stores.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreSharedStoreInfoEventData {
    /// Nonzero when a shared store is used automatically for the repository.
    pub use_automatically: u8,
    /// Remote URLs of the shared stores.
    pub remote_urls: LoreArray<LoreString>,
    /// Filesystem paths of the shared stores.
    pub paths: LoreArray<LoreString>,
    /// Per-store flag, nonzero when the store exists on disk.
    pub exists: LoreArray<u8>,
}
