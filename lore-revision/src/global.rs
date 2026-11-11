// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod external_dir;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use lore_base::directories::project_directory;
use lore_base::env::AUTH_PATH_VAR;
use lore_base::env::AUTH_STORE_VAR;
use lore_base::env::AuthStore;
use lore_base::env::GLOBAL_PATH_VAR;
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

pub fn get_global_config_dir() -> Result<PathBuf, GlobalConfigError> {
    let path = global_config_dir_path()
        .ok_or_else(|| GlobalConfigError::internal("project directory not found"))?;
    make_path_if_nonexistent(&path)?;
    Ok(path)
}

/// The directory of the global configuration, without creating it, or `None` where there is no
/// per-user directory to put it in.
fn global_config_dir_path() -> Option<PathBuf> {
    match lore_base::env::var(GLOBAL_PATH_VAR) {
        Some(override_dir) => Some(PathBuf::from(override_dir).join("config")),
        None => Some(project_directory()?.config_local_dir().to_path_buf()),
    }
}

pub fn get_global_data_dir() -> Result<PathBuf, GlobalConfigError> {
    let path = if let Some(override_dir) = lore_base::env::var(GLOBAL_PATH_VAR) {
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

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct DefaultSharedStoreConfigValue {
    pub path_to_store: String,
    /// Keys this version doesn't know, with their values, written back on save.
    #[serde(flatten)]
    unknown_keys: toml::Table,
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
    /// Seconds the service keeps a connection to a remote open once no command
    /// is using it, so that the next command reuses it rather than connecting
    /// again. Unset is [`DEFAULT_IDLE_CONNECTION_TIMEOUT`].
    pub idle_connection_timeout: Option<u64>,
    /// Keys this version doesn't know, with their values, written back on save.
    #[serde(flatten)]
    unknown_keys: toml::Table,
}

/// Settings for authentication, under `[auth]`.
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
#[serde(default)]
pub struct AuthConfig {
    /// Where the token store's encryption key is kept: `secure` (the default) for the operating
    /// system's secure store, `fallback` for a file beside the token store, readable by its owner
    /// alone. `fallback` suits machines where nothing can answer a secure-store prompt, such as
    /// build agents and automation, and binaries rebuilt with a new signature each time, which the
    /// secure store treats as new applications.
    pub store: Option<AuthStore>,
    /// Directory holding the token store, in place of the per-user default. A relative path is
    /// relative to the directory of the configuration file.
    pub path: Option<String>,
    /// Keys this version doesn't know, with their values, written back on save.
    #[serde(flatten)]
    unknown_keys: toml::Table,
}

/// The global configuration's `[auth]` settings, as the token store reads them.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct AuthSettings {
    /// Where the token store's encryption key is kept, where the configuration names a store.
    pub store: Option<AuthStore>,
    /// The token store directory, absolute when the configuration file's path is.
    pub path: Option<PathBuf>,
}

impl AuthSettings {
    /// The settings `config` gives, read from a configuration file in `config_dir`. A `path` is
    /// read without surrounding whitespace, and a blank one is unset.
    pub fn from_config(config: AuthConfig, config_dir: &Path) -> Self {
        let path = config
            .path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(|path| config_dir.join(path));
        Self {
            store: config.store,
            path,
        }
    }

    /// The environment variables that carry these settings to the token store, with their values:
    /// `LORE_AUTH_STORE` for either store, so that a call relayed to the service carries the
    /// choice, and `LORE_AUTH_PATH`.
    pub fn variables(&self) -> Vec<(&'static str, OsString)> {
        let store = self
            .store
            .map(|store| (AUTH_STORE_VAR, OsString::from(store.as_var())));
        let path = self
            .path
            .as_ref()
            .map(|path| (AUTH_PATH_VAR, path.clone().into_os_string()));
        store.into_iter().chain(path).collect()
    }
}

/// The `[auth]` settings of the global configuration at `file`. A missing file has none; one that
/// can't be read, isn't TOML, or holds an `[auth]` table that doesn't parse is an error. Other
/// tables aren't read. Reads synchronously, for a caller that runs before the runtime exists.
pub fn load_auth_settings_from(file: &Path) -> Result<AuthSettings, GlobalConfigError> {
    /// The global configuration's `[auth]` table alone.
    #[derive(Serialize, Deserialize, Default)]
    #[serde(default)]
    struct AuthTable {
        auth: AuthConfig,
    }
    let config: AuthTable = crate::util::config::load_blocking(file)
        .forward::<GlobalConfigError>("Loading the global configuration's [auth] table")?;
    let config_dir = file.parent().unwrap_or(Path::new(""));
    Ok(AuthSettings::from_config(config.auth, config_dir))
}

/// Refuses a value of `LORE_AUTH_STORE` that names neither store, which the token store would read
/// as the secure store. A blank or unset value is no choice.
#[lore_macro::test_pub]
fn check_auth_store_value(value: Option<&str>) -> Result<(), GlobalConfigError> {
    match value {
        Some(value) if !value.is_empty() && AuthStore::from_var(value).is_none() => {
            Err(GlobalConfigError::internal(format!(
                "{AUTH_STORE_VAR} is {value:?}, which names neither store: it takes `secure` or `fallback`"
            )))
        }
        _ => Ok(()),
    }
}

/// [`load_auth_settings_from`] the global configuration file. Where there is no per-user directory
/// to hold one, there are no settings, and nothing is created. A `LORE_AUTH_STORE` that names
/// neither store is an error, since the token store would read it as the secure store; a blank one
/// is unset.
pub fn load_auth_settings() -> Result<AuthSettings, GlobalConfigError> {
    let store = std::env::var(AUTH_STORE_VAR).ok();
    check_auth_store_value(store.as_deref())?;
    let mut settings = match global_config_dir_path() {
        Some(directory) => load_auth_settings_from(&directory.join(CONFIG))?,
        None => AuthSettings::default(),
    };
    // A relative `LORE_GLOBAL_PATH` gives a relative path, which a service resolves against its own
    // working directory.
    settings.path = settings
        .path
        .map(|path| std::path::absolute(&path).unwrap_or(path));
    let default = project_directory().map(|directory| directory.config_local_dir().to_path_buf());
    check_fallback_has_a_path(
        &settings,
        store.as_deref(),
        std::env::var(AUTH_PATH_VAR).ok().as_deref(),
        default.as_deref(),
    )?;
    Ok(settings)
}

/// Refuses `settings` choosing the fallback store with no token store directory of its own, from
/// `[auth] path` or the `LORE_AUTH_PATH` value `path`, where the `LORE_AUTH_STORE` value `store`
/// leaves the choice to them. The default directory `default`, which a `path` naming it doesn't
/// escape, holds the logins of programs using the secure store, which a new fallback key would
/// discard.
#[lore_macro::test_pub]
fn check_fallback_has_a_path(
    settings: &AuthSettings,
    store: Option<&str>,
    path: Option<&str>,
    default: Option<&Path>,
) -> Result<(), GlobalConfigError> {
    let unset = |value: Option<&str>| value.is_none_or(str::is_empty);
    let own_path = settings
        .path
        .as_deref()
        .is_some_and(|path| default.is_none_or(|default| path != default));
    if settings.store == Some(AuthStore::Fallback) && unset(store) && !own_path && unset(path) {
        return Err(GlobalConfigError::internal(
            "`[auth] store = \"fallback\"` needs a `path` of its own: the default token store holds the logins of programs using the secure store",
        ));
    }
    Ok(())
}

/// How long the service keeps an unused connection open when
/// `[service] idle_connection_timeout` is unset.
pub const DEFAULT_IDLE_CONNECTION_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// The user-level `config.toml`, which keeps the keys it doesn't know at every level.
///
/// A key renamed later can't read its old name through `#[serde(alias)]`: an older Lore keeps
/// the new key and writes the old one beside it, and an alias refuses both being present.
#[lore_macro::test_pub]
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
#[serde(default)]
pub struct GlobalConfig {
    #[serde(alias = "default_global_stores")]
    default_shared_stores: BTreeMap<String, DefaultSharedStoreConfigValue>,
    #[serde(alias = "use_global_store_automatically")]
    pub use_shared_store_automatically: Option<bool>,
    pub service: ServiceConfig,
    pub auth: AuthConfig,
    /// Keys this version doesn't know, with their values, written back on save.
    #[serde(flatten)]
    unknown_keys: toml::Table,
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
    /// Sets the default shared store directory for `remote_url`, keeping the entry's other keys.
    pub fn set_default_path_for_remote_url(
        &mut self,
        remote_url: &str,
        default: impl AsRef<Path>,
    ) -> Result<(), GlobalConfigError> {
        let path_to_store = default
            .as_ref()
            .to_str()
            .ok_or_else(|| GlobalConfigError::internal("bad path"))?
            .to_owned();
        self.default_shared_stores
            .entry(normalize_remote_url(remote_url).to_owned())
            .or_default()
            .path_to_store = path_to_store;
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

    /// How long the service keeps a connection to a remote open once no command
    /// is using it.
    pub fn service_idle_connection_timeout(&self) -> Duration {
        self.service
            .idle_connection_timeout
            .map_or(DEFAULT_IDLE_CONNECTION_TIMEOUT, Duration::from_secs)
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
