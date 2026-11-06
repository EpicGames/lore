use std::fmt;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// How often SWFS has called back into Lore for what it holds of a mount: to fetch a file's
/// content, and to evaluate a directory it had not yet listed. Each is what an unnecessary read of
/// the mount costs, so an operation reports how many came while it ran.
#[derive(Debug, Default)]
pub struct CallbackCounters {
    read_file: AtomicU64,
    directory_evaluations: AtomicU64,
}

/// The counts at one moment, to measure later counts against.
#[derive(Debug, Clone, Copy)]
pub struct CallbackMark {
    read_file: u64,
    directory_evaluations: u64,
}

/// The callbacks that came between a mark and now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallbackDelta {
    pub read_file: u64,
    pub directory_evaluations: u64,
}

impl CallbackCounters {
    pub fn record_read_file(&self) {
        self.read_file.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_directory_evaluation(&self) {
        self.directory_evaluations.fetch_add(1, Ordering::Relaxed);
    }

    pub fn mark(&self) -> CallbackMark {
        CallbackMark {
            read_file: self.read_file.load(Ordering::Relaxed),
            directory_evaluations: self.directory_evaluations.load(Ordering::Relaxed),
        }
    }

    pub fn since(&self, mark: CallbackMark) -> CallbackDelta {
        let now = self.mark();
        CallbackDelta {
            read_file: now.read_file - mark.read_file,
            directory_evaluations: now.directory_evaluations - mark.directory_evaluations,
        }
    }
}

impl fmt::Display for CallbackDelta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "read_file callbacks +{}, directory evaluations +{}",
            self.read_file, self.directory_evaluations
        )
    }
}
