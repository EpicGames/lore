// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use lore_base::directories::project_directory;
use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::shared_store::suggested_shared_store_path_for_remote_url;
use crate::util;
use crate::util::config::SaveableConfig;
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
            suggested_shared_store_path_for_remote_url(remote_url)
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
}

impl SaveableConfig for GlobalConfig {
    type ErrorType = GlobalConfigError;

    fn file_location() -> Result<PathBuf, Self::ErrorType> {
        get_global_config_dir().map(|path| path.join(CONFIG))
    }

    async fn modify_on_load(mut self) -> Result<Self, Self::ErrorType> {
        let old = std::mem::take(&mut self.default_shared_stores);
        for (key, value) in old {
            let normalized = normalize_remote_url(&key).to_owned();
            self.default_shared_stores
                .entry(normalized)
                .or_insert(value);
        }
        Ok(self)
    }
}
