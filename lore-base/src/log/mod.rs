// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod rotate;

use core::fmt;

use serde::Deserialize;
use serde::Serialize;

/// Severity level of a log message.
/// cbindgen:prefix-with-name
/// cbindgen:rename-all=ScreamingSnakeCase
#[repr(C)]
#[derive(Debug, Copy, Clone, Default, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LoreLogLevel {
    /// No logging.
    #[default]
    None = 0,
    /// Most detailed tracing messages.
    Trace = 1,
    /// Debugging messages.
    Debug = 2,
    /// Informational messages.
    Info = 3,
    /// Warnings about unexpected but recoverable situations.
    Warn = 4,
    /// Errors.
    Error = 5,
}

impl fmt::Display for LoreLogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

pub type LoreLogCallback = fn(level: LoreLogLevel, location: &str, message: &str);

static LOG_STATE: parking_lot::RwLock<LogState> = parking_lot::RwLock::new(LogState::new());

struct LogState {
    callback: Option<LoreLogCallback>,
    level: LoreLogLevel,
}

impl LogState {
    const fn new() -> Self {
        Self {
            callback: None,
            level: LoreLogLevel::None,
        }
    }
}

pub fn set_log_callback(callback: Option<LoreLogCallback>) {
    LOG_STATE.write().callback = callback;
}

pub fn set_log_level(level: LoreLogLevel) {
    LOG_STATE.write().level = level;
}

pub fn log_level() -> LoreLogLevel {
    LOG_STATE.read().level
}

/// Dispatches a log event to the registered callback, if any.
/// Called by the log macros — not intended for direct use.
#[doc(hidden)]
pub fn dispatch_log(level: LoreLogLevel, location: &str, message: &str) {
    let callback = LOG_STATE.read().callback;
    if let Some(callback) = callback {
        callback(level, location, message);
    }
}

// -- lore-prefixed macros --

#[macro_export]
macro_rules! lore_trace {
    ($fmt:expr $(, $arg:expr)* $(,)?) => {
        $crate::lore_log_event!(
            $crate::log::LoreLogLevel::Trace,
            $fmt $(, $arg)*
        )
    };
}

#[macro_export]
macro_rules! lore_debug {
    ($($args:tt)+) => {
        $crate::lore_log_event!(
            $crate::log::LoreLogLevel::Debug,
            $($args)*
        )
    };
}

#[macro_export]
macro_rules! lore_info {
    ($($args:tt)+) => {
        $crate::lore_log_event!(
            $crate::log::LoreLogLevel::Info,
            $($args)*
        )
    };
}

#[macro_export]
macro_rules! lore_warn {
    ($($args:tt)+) => {
        $crate::lore_log_event!(
            $crate::log::LoreLogLevel::Warn,
            $($args)*
        )
    };
}

#[macro_export]
macro_rules! lore_error {
    ($($args:tt)+) => {
        $crate::lore_log_event!(
            $crate::log::LoreLogLevel::Error,
            $($args)*
        )
    };
}

#[macro_export]
macro_rules! lore_log_event {
    ($level:expr, $($args:tt)+) => {
        if $level >= $crate::log::log_level() {
            let log_message = format!("{}", format_args!($($args)+));
            $crate::log::dispatch_log($level, module_path!(), &log_message);
        }
    };
}
