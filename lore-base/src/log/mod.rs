// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod rotate;

use core::fmt;
use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering;

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

impl LoreLogLevel {
    const fn discriminant(self) -> u32 {
        match self {
            LoreLogLevel::None => 0,
            LoreLogLevel::Trace => 1,
            LoreLogLevel::Debug => 2,
            LoreLogLevel::Info => 3,
            LoreLogLevel::Warn => 4,
            LoreLogLevel::Error => 5,
        }
    }

    const fn from_discriminant(value: u32) -> Self {
        match value {
            1 => LoreLogLevel::Trace,
            2 => LoreLogLevel::Debug,
            3 => LoreLogLevel::Info,
            4 => LoreLogLevel::Warn,
            5 => LoreLogLevel::Error,
            _ => LoreLogLevel::None,
        }
    }
}

impl fmt::Display for LoreLogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

pub type LoreLogCallback = fn(level: LoreLogLevel, location: &str, message: &str);

static LOG_CALLBACK: parking_lot::RwLock<Option<LoreLogCallback>> = parking_lot::RwLock::new(None);

/// The level every log macro compares against, held as an atomic rather than
/// behind the lock the callback is. Relaxed is the whole ordering this needs.
/// A level changing while a message is being decided settles only whether that
/// message is kept, and nothing is published or consumed alongside it.
static LOG_LEVEL: AtomicU32 = AtomicU32::new(LoreLogLevel::None.discriminant());

pub fn set_log_callback(callback: Option<LoreLogCallback>) {
    *LOG_CALLBACK.write() = callback;
}

pub fn set_log_level(level: LoreLogLevel) {
    LOG_LEVEL.store(level.discriminant(), Ordering::Relaxed);
}

/// Inlined so the gate in every log macro is a load and a compare at the call
/// site rather than a cross-crate call, which needs LTO to disappear.
#[inline]
pub fn log_level() -> LoreLogLevel {
    LoreLogLevel::from_discriminant(LOG_LEVEL.load(Ordering::Relaxed))
}

/// Dispatches a log event to the registered callback, if any.
/// Called by the log macros — not intended for direct use.
#[doc(hidden)]
pub fn dispatch_log(level: LoreLogLevel, location: &str, message: &str) {
    let callback = *LOG_CALLBACK.read();
    if let Some(callback) = callback {
        callback(level, location, message);
    }
}

// -- lore-prefixed macros --

#[macro_export]
macro_rules! lore_trace {
    ($fmt:expr $(, $arg:expr)* $(,)?) => {{
        #[cfg(not(feature = "trace_log"))]
        if false {
            let _ = core::format_args!($fmt $(, $arg)*);
        }
        #[cfg(feature = "trace_log")]
        if true {
            $crate::lore_log_event!(
                $crate::log::LoreLogLevel::Trace,
                $fmt $(, $arg)*
            )
        }
    }};
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
