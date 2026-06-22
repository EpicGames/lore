// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::error::Error;
use std::fs;
use std::io;

use lore::interface::LoreLogLevel;
use serde::Deserialize;

use crate::eprintln;

static CLI_CONFIG: std::sync::OnceLock<CliConfig> = std::sync::OnceLock::new();

const CONFIG: &str = "cli.toml";
#[cfg(target_family = "unix")]
const DEFAULT_PAGER: &str = "less -R";
#[cfg(target_family = "windows")]
const DEFAULT_PAGER: &str = "more.com";
#[cfg(not(any(target_family = "windows", target_family = "unix")))]
const DEFAULT_PAGER: &str = "";

#[derive(Debug)]
pub struct CliConfig {
    /// Enable machine-readable json output in CLI
    pub json: bool,
    /// Output log level
    pub log_level: LoreLogLevel,
    /// Which paginator to use
    pub pager: String,
    /// Debug logging
    pub debug: bool,
    /// Disable interactive prompts
    pub non_interactive: bool,
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            json: false,
            log_level: LoreLogLevel::default(),
            pager: DEFAULT_PAGER.to_string(),
            debug: false,
            non_interactive: false,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct CliUserConfig {
    /// Which paginator to use
    pub pager: String,
}

impl Default for CliUserConfig {
    fn default() -> Self {
        Self {
            pager: DEFAULT_PAGER.to_string(),
        }
    }
}

#[allow(clippy::fn_params_excessive_bools)]
pub fn setup_config(
    json: bool,
    log_level: LoreLogLevel,
    no_pager: bool,
    debug: bool,
    non_interactive: bool,
) {
    let mut config = CliConfig {
        json,
        log_level,
        pager: DEFAULT_PAGER.to_string(),
        debug,
        non_interactive,
    };

    if let Ok(user_config) = load_user_config_file() {
        config.pager = user_config.pager;
    }

    if no_pager || json {
        config.pager = String::new();
    }

    CLI_CONFIG.set(config).unwrap_or_else(|_cli_config| {
        eprintln!("Error while initializing cli config: config has already been initialized");
    });
}

fn load_user_config_file() -> Result<CliUserConfig, Box<dyn Error>> {
    let Some(path) = lore::interface::user_directory().map(|path| path.join(CONFIG)) else {
        return Err(io::Error::other("Failed to get user config path").into());
    };

    match path.try_exists() {
        Ok(false) => {
            let config = CliUserConfig::default();
            Ok(config)
        }
        Ok(true) => {
            let config_string = fs::read_to_string(path)?;
            let config = toml::from_str(&config_string)?;
            Ok(config)
        }
        Err(e) => Err(e)?,
    }
}

pub fn config() -> &'static CliConfig {
    CLI_CONFIG.get_or_init(CliConfig::default)
}
