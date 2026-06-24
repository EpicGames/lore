// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use lore_base::directories::project_directory;
use lore_base::fs::lock::FSLock;
use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;
use tokio::fs::OpenOptions;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::task::spawn_blocking;

use crate::util;
use crate::util::url::normalize_remote_url;

#[error_set]
pub enum GlobalConfigError {}

fn make_path_if_nonexistent(path: &PathBuf) -> Result<(), GlobalConfigError> {
    if !path.exists() {
        std::fs::create_dir_all(path)
            .internal_with(|| format!("creating global config dir {}", path.display()))?;
    }
    Ok(())
}

const LORE_GLOBAL_PATH_VAR: &str = "LORE_GLOBAL_PATH";

pub fn get_global_config_dir() -> Result<PathBuf, GlobalConfigError> {
    let path = if let Ok(override_dir) = std::env::var(LORE_GLOBAL_PATH_VAR) {
        PathBuf::from(override_dir).join("config")
    } else {
        project_directory()
            .ok_or_else(|| GlobalConfigError::internal("project directory not found"))?
            .config_local_dir()
            .to_path_buf()
    };
    make_path_if_nonexistent(&path)?;
    Ok(path)
}

pub fn get_global_data_dir() -> Result<PathBuf, GlobalConfigError> {
    let path = if let Ok(override_dir) = std::env::var(LORE_GLOBAL_PATH_VAR) {
        PathBuf::from(override_dir).join("data")
    } else {
        project_directory()
            .ok_or_else(|| GlobalConfigError::internal("project directory not found"))?
            .data_local_dir()
            .to_path_buf()
    };
    make_path_if_nonexistent(&path)?;
    Ok(path)
}

pub const CONFIG: &str = "config.toml";

fn global_config_toml_path() -> Result<PathBuf, GlobalConfigError> {
    get_global_config_dir().map(|path| path.join(CONFIG))
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DefaultSharedStoreConfigValue {
    pub path_to_store: String,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
#[serde(default)]
pub struct GlobalConfig {
    #[serde(alias = "default_global_stores")]
    default_shared_stores: BTreeMap<String, DefaultSharedStoreConfigValue>,
    #[serde(alias = "use_global_store_automatically")]
    pub use_shared_store_automatically: Option<bool>,
}

impl GlobalConfig {
    pub fn all_default_shared_stores(
        &self,
    ) -> impl Iterator<Item = (&String, &DefaultSharedStoreConfigValue)> {
        self.default_shared_stores.iter()
    }
    pub fn default_shared_store_directory_for_remote(
        &self,
        remote_url: &str,
    ) -> Result<PathBuf, GlobalConfigError> {
        let normalized = normalize_remote_url(remote_url);
        if let Some(config) = self.default_shared_stores.get(normalized) {
            Ok(util::path::make_absolute(&config.path_to_store)
                .map_err(|_err| GlobalConfigError::internal("bad path"))?)
        } else {
            Self::suggested_path_for_remote_url(remote_url)
        }
    }
    pub fn set_default_path_for_remote_url(
        &mut self,
        remote_url: &str,
        default: impl AsRef<Path>,
    ) -> Result<(), GlobalConfigError> {
        let normalized_url = normalize_remote_url(remote_url).to_owned();
        self.default_shared_stores.insert(
            normalized_url,
            DefaultSharedStoreConfigValue {
                path_to_store: default
                    .as_ref()
                    .to_str()
                    .ok_or(GlobalConfigError::internal("bad path"))?
                    .to_owned(),
            },
        );
        Ok(())
    }
    pub fn use_shared_store_automatically(&self) -> bool {
        self.use_shared_store_automatically.unwrap_or(false)
    }
    pub fn suggested_path_for_remote_url(remote_url: &str) -> Result<PathBuf, GlobalConfigError> {
        let data_dir = get_global_data_dir()?;
        let normalized = normalize_remote_url(remote_url);
        let new_path = data_dir.join(Self::escape_url_as_dirname(normalized));
        if new_path.exists() {
            return Ok(new_path);
        }
        // Fall back to legacy path that included the protocol prefix (e.g. "urcs___host")
        // so existing shared stores created before protocol stripping are still found.
        let legacy_path = data_dir.join(Self::escape_url_as_dirname(
            remote_url.trim_end_matches('/'),
        ));
        if legacy_path.exists() {
            return Ok(legacy_path);
        }
        // Neither exists — use the new normalized form for new stores.
        Ok(new_path)
    }

    /// The per-remote subdirectory name within a shared store base path. A base
    /// path holds one such directory per remote URL so a single base can back
    /// the stores of multiple endpoints at once.
    pub fn shared_store_subdir_for_remote(remote_url: &str) -> String {
        Self::escape_url_as_dirname(normalize_remote_url(remote_url))
    }

    fn escape_url_as_dirname(url: &str) -> String {
        url.chars()
            .map(|c| match c {
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
                c if c.is_ascii_control() => '_',
                c => c,
            })
            .collect()
    }

    pub async fn load() -> Result<Self, GlobalConfigError> {
        let path = global_config_toml_path()?;
        load_config(&path)
            .await
            .forward::<GlobalConfigError>("Loading global config")
    }

    pub async fn load_locked() -> Result<(Self, FSLock), GlobalConfigError> {
        let path = global_config_toml_path()?;
        let (mut config, lock) = load_config_with_lock::<Self>(&path)
            .await
            .forward::<GlobalConfigError>("Loading global config")?;
        // Normalize stored keys to strip legacy protocol prefixes (e.g. "urc://host" -> "host").
        let old = std::mem::take(&mut config.default_shared_stores);
        for (key, value) in old {
            let normalized = normalize_remote_url(&key).to_owned();
            config
                .default_shared_stores
                .entry(normalized)
                .or_insert(value);
        }
        Ok((config, lock))
    }

    pub async fn save(&self, lock: FSLock) -> Result<(), GlobalConfigError> {
        let path = global_config_toml_path()?;
        let result = save_config(self, &path).await;
        drop(lock);
        result.forward::<GlobalConfigError>("saving global config")
    }
}

#[error_set]
pub enum LoadConfigError {}

pub async fn load_config_with_lock<ConfigType: Default + Serialize + for<'a> Deserialize<'a>>(
    path: impl AsRef<Path> + Copy,
) -> Result<(ConfigType, FSLock), LoadConfigError> {
    let path_buf = path.as_ref().to_owned();
    let lock = spawn_blocking(|| {
        FSLock::acquire_file_lock(path_buf)
            .map_err(|err| LoadConfigError::internal(format!("Failed acquiring lock {err}")))
    })
    .await
    .internal("Failed to acquire file lock")??;
    let config = load_config(path).await?;
    Ok((config, lock))
}

pub async fn load_config<ConfigType: Default + Serialize + for<'a> Deserialize<'a>>(
    path: impl AsRef<Path>,
) -> Result<ConfigType, LoadConfigError> {
    if let Ok(mut config_file) = OpenOptions::new().create(false).read(true).open(path).await {
        let mut config = String::default();
        config_file.read_to_string(&mut config).await.ok();
        toml::from_str(config.as_str())
            .map_err(|_err| LoadConfigError::internal("failed to parse config"))
    } else {
        Ok(ConfigType::default())
    }
}

#[error_set]
pub enum SaveConfigError {}

pub async fn save_config<ConfigType: Serialize>(
    config: &ConfigType,
    path: impl AsRef<Path>,
) -> Result<(), SaveConfigError> {
    let mut config_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .await
        .internal("opening config file for save")?;
    let config_string = toml::to_string_pretty(&config).internal("formatting config as TOML")?;
    config_file
        .write_all(config_string.as_bytes())
        .await
        .internal("writing config file")?;
    config_file.flush().await.internal("flushing config file")?;
    Ok(())
}
