// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

use chrono::SecondsFormat;
use chrono::Utc;
use lore_base::directories::project_directory;
use lore_base::log::LoreLogLevel;
use lore_base::log::rotate::RotatingLogFile;
use lore_error_set::prelude::*;
use lore_revision::event::LoreEvent;
use lore_revision::event::LoreLogEventData;
use lore_revision::interface::LoreString;
use lore_revision::lore::try_execution_context;

#[repr(C)]
#[derive(Debug, Clone, Default, PartialEq)]
/// Configuration controlling Lore's file and event logging.
pub struct LoreLogConfig {
    /// Enable logging to a file (disabled by default)
    pub file: u8,
    /// Enable daily rolling logfile
    pub file_rolling: u8,
    /// Path to the log file
    pub file_path: LoreString,
    /// Prefix for log files
    pub file_prefix: LoreString,
    /// Minimum log level
    pub level: LoreLogLevel,
    /// Log categories bitflags (local, remote, transport)
    pub categories: u32,
    /// Maximum log file size
    pub file_max_size: u32,
    /// Maximum log file count
    pub file_max_count: u32,
}

const LORE_LOGFILE_PREFIX: &str = "lore";
const LORE_LOGFILE_MAX_COUNT: usize = 8;

#[error_set]
pub enum LogError {}

static LOG_CONFIG: OnceLock<parking_lot::RwLock<LoreLogConfig>> = OnceLock::new();

/// Initialize lore-log with the lore dispatch callback.
/// Must be called before any logging occurs.
pub fn initialize() {
    lore_base::log::set_log_callback(Some(lore_log_dispatch));
    lore_base::log::set_log_level(LoreLogLevel::Debug);
}

pub fn log_level() -> LoreLogLevel {
    if let Some(config) = LOG_CONFIG.get() {
        config.read().level
    } else {
        LoreLogLevel::Info
    }
}

pub fn configure(config: &LoreLogConfig) {
    initialize();

    let mut config = config.clone();

    if config.level == LoreLogLevel::None {
        config.level = LoreLogLevel::Info;
    }

    lore_base::log::set_log_level(config.level);

    if config.file == 0 {
        return;
    }

    let prefix = if !config.file_prefix.is_empty() {
        config.file_prefix.to_string()
    } else {
        LORE_LOGFILE_PREFIX.to_owned()
    };

    let log_dir = if !config.file_path.is_empty() {
        PathBuf::from(config.file_path.as_str())
    } else {
        PathBuf::from(get_default_logs_path())
    };

    let max_count = if config.file_max_count > 0 {
        config.file_max_count as usize
    } else {
        LORE_LOGFILE_MAX_COUNT
    };

    let Ok(log_file) = RotatingLogFile::new(log_dir, prefix, max_count) else {
        return;
    };

    LogFileWriter::init(log_file);

    *LOG_CONFIG.get_or_init(parking_lot::RwLock::default).write() = config;
}

static LOG_FILE_WRITER: OnceLock<std::sync::mpsc::Sender<String>> = OnceLock::new();

/// Writes formatted log lines to a rotating log file on a background thread.
struct LogFileWriter;

impl LogFileWriter {
    fn init(file: RotatingLogFile) {
        let (sender, receiver) = std::sync::mpsc::channel();

        let _ = std::thread::Builder::new()
            .name("log-writer".into())
            .spawn(move || {
                let mut file = file;
                while let Ok(line) = receiver.recv() {
                    let _ = writeln!(&mut file, "{line}");
                }
            });

        let _ = LOG_FILE_WRITER.set(sender);
    }
}

/// Log dispatch callback for the lore crate.
/// Writes to the log file (if configured) and dispatches through the execution context.
fn lore_log_dispatch(level: LoreLogLevel, location: &str, message: &str) {
    let execution = try_execution_context();

    // Write to log file if configured
    if let Some(sender) = LOG_FILE_WRITER.get()
        && level >= LoreLogLevel::Debug
    {
        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let correlation_id = execution
            .as_ref()
            .map_or("-", |e| e.dispatcher.correlation_id.as_str());
        let _ = sender.send(format!(
            "[{timestamp}] [{correlation_id}] [{level}] [{location}] {message}"
        ));
    }

    // Dispatch through execution context event dispatcher
    if let Some(execution) = execution {
        execution.dispatcher.send(LoreEvent::Log(LoreLogEventData {
            level,
            category: 0,
            timestamp: lore_revision::util::time::timestamp(),
            location: LoreString::from(location),
            message: LoreString::from(message),
        }));
    }
}

pub fn get_logs_path() -> String {
    if let Some(config) = LOG_CONFIG.get() {
        let config = config.read();
        if !config.file_path.is_empty() {
            let log_path = Path::new(config.file_path.as_str());
            if let Ok(log_path) = log_path.canonicalize() {
                return log_path.display().to_string();
            } else {
                return log_path.display().to_string();
            }
        }
    }

    get_default_logs_path()
}

fn get_default_logs_path() -> String {
    if let Some(project_dir) = project_directory()
        && let Some(path_string) = project_dir.data_local_dir().join("logs").to_str()
    {
        path_string.to_owned()
    } else {
        String::new()
    }
}
