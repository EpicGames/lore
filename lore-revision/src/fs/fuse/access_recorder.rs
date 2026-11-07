// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Records the first access to each path a FUSE mount serves from its revision, for a later
//! `lore clone --prefetch <file>`. Reads record a file; a listing built to answer LOOKUP or
//! READDIR/READDIRPLUS records a directory. Paths under `write/`, which the revision does not
//! hold, are not recorded. Enabled per mount by `LORE_FUSE_RECORD_DIR`.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use lore_base::lore_spawn;
use tokio::io::AsyncWriteExt as _;
use tokio::io::BufWriter;
use tokio::sync::mpsc;

use crate::lore_warn;

/// The directory each mount's recording file is written under; unset, recording is off.
#[lore_macro::test_pub]
const RECORD_DIR_ENV: &str = "LORE_FUSE_RECORD_DIR";

/// Upper bound on how long a burst of first accesses waits before its lines reach disk.
const FLUSH_INTERVAL: Duration = Duration::from_millis(250);

/// Sends first-access paths to one writer task appending them, one per line, to a file opened
/// once at construction. The file is flushed on this interval and once more when the last
/// sender drops, so a reader sees every line once the mount unmounts.
#[derive(Clone)]
pub struct AccessRecorder {
    paths: mpsc::UnboundedSender<String>,
}

impl AccessRecorder {
    /// A recorder for this mount under `RECORD_DIR_ENV`, named for `instance_id` and the
    /// current time; `None` when the variable is unset or the file cannot be created. Read the
    /// variable once, where the mount is constructed.
    pub fn from_env(instance_id: &str) -> Option<Self> {
        let dir = std::env::var_os(RECORD_DIR_ENV)?;
        match Self::start(&record_file_path(Path::new(&dir), instance_id)) {
            Ok(recorder) => Some(recorder),
            Err(error) => {
                lore_warn!("FUSE access recording disabled: {error}");
                None
            }
        }
    }

    /// A recorder appending to a freshly created `path`.
    pub fn start(path: &Path) -> std::io::Result<Self> {
        let file = std::fs::File::create(path)?;
        let (paths, receiver) = mpsc::unbounded_channel();
        let _writer = lore_spawn!(write_loop(tokio::fs::File::from_std(file), receiver));
        Ok(Self { paths })
    }

    /// Sends `path` to the writer. Dropped silently if the writer has already stopped.
    #[lore_macro::test_pub]
    pub(crate) fn record(&self, path: String) {
        let _ = self.paths.send(path);
    }
}

/// The file this mount's recording is written to: `<dir>/<instance_id>-<unix seconds>.txt`.
fn record_file_path(dir: &Path, instance_id: &str) -> PathBuf {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since_epoch| since_epoch.as_secs());
    dir.join(format!("{instance_id}-{seconds}.txt"))
}

/// Appends every path the channel delivers to `file`, one per line, flushing on
/// [`FLUSH_INTERVAL`] and once more before returning, so the sender side's drop guarantees a
/// final flush. Lines collect in memory; a flush writes them off the runtime's workers.
async fn write_loop(file: tokio::fs::File, mut paths: mpsc::UnboundedReceiver<String>) {
    let mut writer = BufWriter::new(file);
    let mut flush = tokio::time::interval(FLUSH_INTERVAL);
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            received = paths.recv() => match received {
                Some(path) => {
                    let _ = writer.write_all(path.as_bytes()).await;
                    let _ = writer.write_all(b"\n").await;
                }
                None => break,
            },
            _ = flush.tick() => {
                let _ = writer.flush().await;
            }
        }
    }
    let _ = writer.flush().await;
}
