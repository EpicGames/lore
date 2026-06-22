// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::io::IsTerminal;
use std::io::Write;
use std::io::stdout;
use std::process::Child;
use std::process::ChildStdin;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Weak;

use parking_lot::Mutex;
use parking_lot::RwLock;

use crate::config::config;
use crate::eprintln;

static CURRENT_PAGER: RwLock<Weak<Mutex<PagerProcess>>> = RwLock::new(Weak::new());

#[derive(Clone)]
pub struct Pager {
    process: Option<Arc<Mutex<PagerProcess>>>,
}

struct PagerProcess {
    child: Child,
}

pub struct PagerWithBackup<W: Write> {
    pager: Pager,
    backup: W,
}

impl Default for Pager {
    fn default() -> Self {
        Self::new()
    }
}

impl Pager {
    pub fn new() -> Self {
        let pager = Self {
            process: PagerProcess::new().map(|pd| Arc::new(Mutex::new(pd))),
        };
        #[cfg(debug_assertions)]
        if CURRENT_PAGER.read().strong_count() > 0 {
            eprintln!("[ERROR] Pager created when pager already exists");
        }
        *CURRENT_PAGER.write() = pager
            .process
            .as_ref()
            .map(Arc::downgrade)
            .unwrap_or_default();
        pager
    }

    pub fn with_backup_stream<W: Write>(self, stream: W) -> PagerWithBackup<W> {
        PagerWithBackup {
            pager: self,
            backup: stream,
        }
    }
}

impl PagerProcess {
    pub fn new() -> Option<Self> {
        let child = if !stdout().is_terminal() {
            None
        } else {
            let config = config();
            let mut pager_config = config.pager.split_ascii_whitespace();
            if let Some(pager_target) = pager_config.next() {
                let mut cmd = Command::new(pager_target);
                cmd.args(pager_config);
                Some(cmd)
            } else {
                None
            }
            .and_then(|mut cmd| cmd.stdin(Stdio::piped()).spawn().ok())
        };

        child.map(|child| Self { child })
    }

    fn stdin(&self) -> std::io::Result<&ChildStdin> {
        match &self.child.stdin {
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Pager process stdin not available",
            )),
            Some(stdin) => Ok(stdin),
        }
    }
}

impl Write for PagerProcess {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stdin()?.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stdin()?.flush()
    }
}

impl Drop for PagerProcess {
    fn drop(&mut self) {
        match self.child.wait() {
            Ok(_) => {}
            Err(err) => {
                eprintln!("Failed to wait on pager: {err}");
            }
        }
    }
}

impl<W: Write> Write for PagerWithBackup<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(data) = self.pager.process.as_ref() {
            data.lock().write(buf)
        } else {
            self.backup.write(buf)
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(data) = self.pager.process.as_ref() {
            data.lock().flush()
        } else {
            self.backup.flush()
        }
    }
}

pub fn get_current_pager() -> Pager {
    Pager {
        process: CURRENT_PAGER.read().upgrade(),
    }
}
