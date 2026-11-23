// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Captures what `tracing` writes on the current thread, so a test can assert on log lines.

use std::io;
use std::sync::Arc;
use std::sync::Mutex;

use tracing::subscriber::DefaultGuard;
use tracing_subscriber::fmt::format::FmtSpan;

/// The JSON lines an INFO-level subscriber wrote, span creations included.
#[derive(Clone, Default)]
pub(crate) struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl CapturedLogs {
    /// Starts capturing on the current thread until the returned guard drops. Events
    /// emitted on another thread, as by a request served through the full HTTP router,
    /// are not captured.
    pub(crate) fn start() -> (Self, DefaultGuard) {
        let logs = Self::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::INFO)
            .with_span_events(FmtSpan::NEW)
            .with_writer(move || writer.clone())
            .finish();
        (logs, tracing::subscriber::set_default(subscriber))
    }

    pub(crate) fn lines(&self) -> Vec<serde_json::Value> {
        let bytes = self.0.lock().unwrap().clone();
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).expect("log line is JSON"))
            .collect()
    }

    pub(crate) fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    /// The fields of the one INFO event whose message is `message`.
    pub(crate) fn event(&self, message: &str) -> serde_json::Value {
        let mut matching: Vec<_> = self
            .lines()
            .into_iter()
            .filter(|line| line["fields"]["message"] == message)
            .collect();
        assert_eq!(matching.len(), 1, "events named {message:?}: {matching:?}");
        let line = matching.remove(0);
        assert_eq!(line["level"], "INFO");
        line["fields"].clone()
    }
}

impl io::Write for CapturedLogs {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
