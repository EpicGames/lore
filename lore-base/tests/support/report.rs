// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! What a process logs when it reports its runtime, shared by the test binaries that build one.
use lore_base::log::LoreLogLevel;
use parking_lot::Mutex;

static LOGGED: Mutex<Vec<(LoreLogLevel, String)>> = Mutex::new(Vec::new());

fn record(level: LoreLogLevel, _location: &str, message: &str) {
    LOGGED.lock().push((level, message.to_string()));
}

/// The messages [`lore_base::runtime::report_serving_runtime`] logs, with their levels.
pub fn reported_runtime() -> Vec<(LoreLogLevel, String)> {
    lore_base::log::set_log_callback(Some(record));
    lore_base::runtime::report_serving_runtime();
    lore_base::log::set_log_callback(None);
    std::mem::take(&mut *LOGGED.lock())
}

/// The core runtime's timer as the report names it: the info line it logs.
pub fn reported_timer_line(timer: lore_base::runtime::CoreTimer) -> (LoreLogLevel, String) {
    (LoreLogLevel::Info, format!("Core runtime timer: {timer:?}"))
}
