// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod external_dir;

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

/// Settings for the Lore service process, under `[service]`.
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
#[serde(default)]
pub struct ServiceConfig {
    /// Executable started as the service, and the one a service is expected to
    /// run from.
    ///
    /// Naming it here is what makes the choice deliberate rather than a race
    /// between whichever client happens to start a service first. Clients of
    /// different versions can share a machine, so the version that serves them
    /// is a decision to be made once and written down, not an accident of
    /// ordering. Unset resolves the executable from the running program.
    pub executable: Option<String>,
    /// Whether commands are carried out by the service rather than in the
    /// process that was run.
    ///
    /// This is what turns the service on for a machine and leaves it on, which
    /// is most of the point of having one. Unset is off.
    pub use_automatically: Option<bool>,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
#[serde(default)]
pub struct GlobalConfig {
    #[serde(alias = "default_global_stores")]
    default_shared_stores: BTreeMap<String, DefaultSharedStoreConfigValue>,
    #[serde(alias = "use_global_store_automatically")]
    pub use_shared_store_automatically: Option<bool>,
    pub service: ServiceConfig,
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

    /// The executable named under `[service]`, if one is named. A blank value
    /// reads as unset, so that clearing the field is a way to stop pinning one,
    /// and surrounding space is not part of a path.
    pub fn service_executable(&self) -> Option<&str> {
        self.service
            .executable
            .as_deref()
            .map(str::trim)
            .filter(|executable| !executable.is_empty())
    }

    /// Whether commands are carried out by the service. Unset is off.
    pub fn use_service_automatically(&self) -> bool {
        self.service.use_automatically.unwrap_or(false)
    }
}

impl SaveableConfig for GlobalConfig {
    type ErrorType = GlobalConfigError;

    fn file_location() -> Result<PathBuf, Self::ErrorType> {
        get_global_config_dir().map(|path| path.join(CONFIG))
    }

    fn modify_on_load(mut self) -> Result<Self, Self::ErrorType> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every field written and read back through TOML, so that a field added
    /// to the config is known to survive being saved and loaded rather than
    /// only being readable from a file someone wrote by hand.
    #[test]
    fn a_fully_populated_config_survives_a_round_trip() {
        let mut config = GlobalConfig {
            use_shared_store_automatically: Some(true),
            ..GlobalConfig::default()
        };
        config
            .set_default_path_for_remote_url("lore://example", "/srv/shared")
            .expect("the shared store path must be settable");
        config.service.executable = Some("/opt/lore/1.9/bin/lore".to_string());

        let written = toml::to_string(&config).expect("the config must be writable as TOML");
        let read: GlobalConfig = toml::from_str(&written).expect("and readable back");

        assert_eq!(read.service_executable(), Some("/opt/lore/1.9/bin/lore"));
        assert!(read.use_shared_store_automatically());
        assert_eq!(read.all_default_shared_stores().count(), 1);
    }

    #[test]
    fn no_executable_is_named_by_default() {
        assert_eq!(GlobalConfig::default().service_executable(), None);
    }

    /// Blanking the field is how a pin is removed, so it reads as unset rather
    /// than as an executable with no name.
    #[test]
    fn an_empty_executable_reads_as_unset() {
        let config: GlobalConfig =
            toml::from_str("[service]\nexecutable = \"\"\n").expect("readable");
        assert_eq!(config.service_executable(), None);
    }
}
