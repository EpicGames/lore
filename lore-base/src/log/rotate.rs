// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::PathBuf;
use std::time::SystemTime;

use chrono::Local;
use chrono::TimeZone;

use crate::fs::lock::FSLock;

/// A daily-rotating log file writer that is safe for multi-process use.
///
/// Log lines are appended to `{prefix}.log` in the configured directory. When the date
/// changes, the current file is renamed to `{prefix}.log.{YYYY-MM-DD}` and a fresh
/// `{prefix}.log` is created. Rotation is serialized across processes using an `FSLock`
/// on the log directory, so concurrent writers never race on the rename.
///
/// Old rotated files beyond `max_files` are deleted during rotation (oldest first).
pub struct RotatingLogFile {
    dir: PathBuf,
    log_path: PathBuf,
    prefix: String,
    max_files: usize,
    file: Option<File>,
    /// The date label for the current log file, used as suffix when rotating.
    current_date: String,
    /// Unix timestamp (seconds) of the next local midnight. Compared against
    /// `SystemTime::now()` on each write — a single syscall with no allocation.
    next_rotation_ts: i64,
}

impl RotatingLogFile {
    /// Creates a new rotating log file writer.
    ///
    /// - `dir`: directory where log files are stored (created if it doesn't exist)
    /// - `prefix`: filename prefix, e.g. `"lore"` produces `lore.log`
    /// - `max_files`: maximum number of rotated files to keep (excluding the active file)
    pub fn new(dir: PathBuf, prefix: String, max_files: usize) -> io::Result<Self> {
        fs::create_dir_all(&dir)?;
        let log_path = dir.join(format!("{prefix}.log"));
        let current_date = today_label();
        let next_rotation_ts = next_midnight_ts();
        let mut writer = Self {
            dir,
            log_path,
            prefix,
            max_files,
            file: None,
            current_date,
            next_rotation_ts,
        };
        // Rotate stale log files from a previous day before opening.
        writer.rotate_if_stale()?;
        writer.reopen()?;
        Ok(writer)
    }

    fn reopen(&mut self) -> io::Result<()> {
        self.file = Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.log_path)?,
        );
        self.current_date = today_label();
        self.next_rotation_ts = next_midnight_ts();
        Ok(())
    }

    /// Rotates a stale log file from a previous day on startup. If the active log file
    /// exists and was last modified before today, it is rotated under the `FSLock` before
    /// any new writes occur.
    fn rotate_if_stale(&mut self) -> io::Result<()> {
        if let Ok(meta) = fs::metadata(&self.log_path)
            && let Ok(modified) = meta.modified()
        {
            let modified_label = date_label_from_system_time(modified);
            if modified_label == self.current_date {
                // File was last written today — no rotation needed.
                return Ok(());
            }
            // File is from a previous day — rotate it.
            self.current_date = modified_label;
            self.rotate()?;
        }
        Ok(())
    }

    /// Rotates the log file under an exclusive filesystem lock.
    ///
    /// After acquiring the lock, re-checks whether rotation is still needed by
    /// looking for a rotated file with the current date suffix. If one exists, another
    /// process already rotated — just reopen.
    fn rotate(&mut self) -> io::Result<()> {
        // Acquire the lock before dropping the file handle. If locking fails, the
        // existing handle remains valid and subsequent writes continue working.
        let _lock = FSLock::acquire_directory_lock_blocking(&self.dir)?;

        // Drop the file handle before renaming — required on Windows where open
        // handles prevent rename/delete operations.
        self.file.take();

        // Re-check under lock: if a rotated file for the current date already exists,
        // another process beat us to the rotation — just reopen the fresh active file.
        let already_rotated = self
            .dir
            .join(format!("{}.log.{}", self.prefix, self.current_date));
        if already_rotated.exists() {
            return self.reopen();
        }

        // We are the rotator. Rename the current file with the old date as suffix.
        let _ = fs::rename(&self.log_path, already_rotated);

        self.cleanup_old_files();
        self.reopen()
    }

    /// Removes the oldest rotated files until at most `max_files` remain.
    fn cleanup_old_files(&self) {
        let pattern = format!("{}.log.", self.prefix);
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };

        let mut rotated: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&pattern))
            })
            .collect();

        // Date suffixes sort chronologically.
        rotated.sort();

        let excess = rotated.len().saturating_sub(self.max_files);
        for path in rotated.drain(..excess) {
            let _ = fs::remove_file(path);
        }
    }
}

impl Write for RotatingLogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        if now >= self.next_rotation_ts {
            self.rotate()?;
        }
        if let Some(file) = self.file.as_mut() {
            file.write(buf)
        } else {
            Err(io::Error::other("log file not available"))
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = self.file.as_mut() {
            file.flush()
        } else {
            Ok(())
        }
    }
}

/// Returns the current local date formatted as `YYYY-MM-DD`.
fn today_label() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

/// Returns the local date label for an arbitrary `SystemTime`.
fn date_label_from_system_time(time: SystemTime) -> String {
    let datetime: chrono::DateTime<Local> = time.into();
    datetime.format("%Y-%m-%d").to_string()
}

/// Returns the unix timestamp (seconds) of the start of the next local day.
/// Handles DST transitions gracefully: if midnight doesn't exist (spring-forward),
/// falls back to the first valid instant of the next day (typically 01:00).
fn next_midnight_ts() -> i64 {
    let tomorrow = Local::now().date_naive() + chrono::Duration::days(1);
    let midnight = tomorrow
        .and_hms_opt(0, 0, 0)
        .expect("00:00:00 is valid HMS");
    match Local.from_local_datetime(&midnight) {
        chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _) => dt.timestamp(),
        chrono::LocalResult::None => {
            // Midnight doesn't exist in this timezone (DST spring-forward).
            // Use 01:00 as fallback — the earliest time that is guaranteed to exist
            // after a DST skip.
            let fallback = tomorrow
                .and_hms_opt(1, 0, 0)
                .expect("01:00:00 is valid HMS");
            if let Some(dt) = Local.from_local_datetime(&fallback).earliest() {
                dt.timestamp()
            } else {
                // Last resort: use raw 86400-second offset from today's midnight.
                let today = Local::now()
                    .date_naive()
                    .and_hms_opt(0, 0, 0)
                    .expect("valid HMS");
                Local
                    .from_local_datetime(&today)
                    .earliest()
                    .map_or(0, |dt| dt.timestamp() + 86400)
            }
        }
    }
}
