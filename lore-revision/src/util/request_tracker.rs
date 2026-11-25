// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fmt;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

/// Counts one operation's immutable store requests by kind and records the peak number of requests
/// in flight for each kind, and times the steps of its fragment collections.
///
/// One tracker is shared by every task of an operation, so its counts cover the whole task tree
/// however wide it fans out. It only observes and never makes a request wait.
#[derive(Default)]
pub struct StoreRequestTracker {
    /// The per-address fragment lookups.
    lookups: InFlightCounter,
    /// The reads of metadata blobs referenced by changed file metadata blocks.
    metadata_reads: InFlightCounter,
    /// When the first fragment collection using this tracker started.
    collect_started: OnceLock<Instant>,
    /// How many fragment collections have started with this tracker. A merge runs one for each
    /// parent, side by side.
    collections: AtomicU64,
    /// How many fragment collections the operation declared it runs, before any started.
    expected_collections: AtomicU64,
    /// For each [`CollectStep`], the microseconds from `collect_started` to the latest time a
    /// collection finished the step.
    step_finished_at: [AtomicU64; CollectStep::ALL.len()],
    /// For each [`CollectStep`], how many collections have started it. Work dropped part-way
    /// stays counted as started.
    step_starts: [AtomicU64; CollectStep::ALL.len()],
    /// For each [`CollectStep`], how many collections have finished it.
    step_finishes: [AtomicU64; CollectStep::ALL.len()],
}

impl StoreRequestTracker {
    /// Counts a fragment lookup as in flight until the returned guard is dropped.
    pub fn track_lookup(&self) -> StoreRequestGuard<'_> {
        self.lookups.track()
    }

    /// The most fragment lookups that were in flight at once.
    pub fn peak_lookups_in_flight(&self) -> u64 {
        self.lookups.peak()
    }

    /// The number of fragment lookups tracked so far.
    pub fn lookups(&self) -> u64 {
        self.lookups.total()
    }

    /// Counts a metadata blob read as in flight until the returned guard is dropped.
    pub fn track_metadata_read(&self) -> StoreRequestGuard<'_> {
        self.metadata_reads.track()
    }

    /// The most metadata blob reads that were in flight at once.
    pub fn peak_metadata_reads_in_flight(&self) -> u64 {
        self.metadata_reads.peak()
    }

    /// The number of metadata blob reads tracked so far.
    pub fn metadata_reads(&self) -> u64 {
        self.metadata_reads.total()
    }

    /// Marks the start of a fragment collection. Its steps are timed from the start of the first.
    pub fn start_collection(&self) {
        self.collect_started.get_or_init(Instant::now);
        self.collections.fetch_add(1, Ordering::AcqRel);
    }

    /// Declares that `count` fragment collections share this tracker, for an operation that
    /// spawns them, so a step reads as finished only once each has finished it, even before the
    /// later ones start.
    pub fn expect_collections(&self, count: u64) {
        self.expected_collections.fetch_max(count, Ordering::AcqRel);
    }

    /// Runs `work` as `step`, and records when it finished, whether it succeeded or failed.
    pub async fn time_step<T>(&self, step: CollectStep, work: impl Future<Output = T>) -> T {
        self.start_step(step);
        let output = work.await;
        self.finish_step(step, Instant::now());
        output
    }

    /// Counts `step` as running until a matching [`StoreRequestTracker::finish_step`], for a step
    /// made of tasks that end separately.
    pub fn start_step(&self, step: CollectStep) {
        self.step_starts[step.index()].fetch_add(1, Ordering::AcqRel);
    }

    /// Starts `step` for a collection that runs it as separate tasks, finished once the spawner
    /// and every task added have called [`StepTasks::done`].
    pub fn start_tasks(self: &Arc<Self>, step: CollectStep) -> Arc<StepTasks> {
        self.start_step(step);
        Arc::new(StepTasks {
            tracker: self.clone(),
            step,
            remaining: AtomicU64::new(1),
        })
    }

    /// Records one collection finishing `step` at `at`.
    pub fn finish_step(&self, step: CollectStep, at: Instant) {
        if let Some(started) = self.collect_started.get() {
            let since_start = at.saturating_duration_since(*started);
            let micros = u64::try_from(since_start.as_micros()).unwrap_or(u64::MAX);
            self.step_finished_at[step.index()].fetch_max(micros, Ordering::AcqRel);
        }
        // Counted after the time, so a snapshot that sees every finish also sees the latest time.
        self.step_finishes[step.index()].fetch_add(1, Ordering::AcqRel);
    }

    /// Where each step stands, timed from the start of the first collection, or `None` if no
    /// collection started.
    pub fn collect_steps(&self) -> Option<CollectSteps> {
        self.collect_started.get()?;
        let collections = self
            .collections
            .load(Ordering::Acquire)
            .max(self.expected_collections.load(Ordering::Acquire));
        let mut states = [StepState::Waiting; CollectStep::ALL.len()];
        for step in CollectStep::ALL {
            let index = step.index();
            let finishes = self.step_finishes[index].load(Ordering::Acquire);
            let starts = self.step_starts[index].load(Ordering::Acquire);
            states[index] = if starts > finishes {
                StepState::Running
            } else if finishes >= collections {
                StepState::Finished(Duration::from_micros(
                    self.step_finished_at[index].load(Ordering::Acquire),
                ))
            } else {
                StepState::Waiting
            };
        }
        Some(CollectSteps { states })
    }
}

/// The tasks one collection runs a [`CollectStep`] as.
pub struct StepTasks {
    tracker: Arc<StoreRequestTracker>,
    step: CollectStep,
    /// The tasks not yet done, with the spawner counted as one.
    remaining: AtomicU64,
}

impl StepTasks {
    /// Counts one more task, which must call [`StepTasks::done`] when it ends.
    pub fn add(&self) {
        self.remaining.fetch_add(1, Ordering::AcqRel);
    }

    /// Marks one task, or the spawner, done. The last finishes the step. A task dropped before
    /// it ends leaves the step running.
    pub fn done(&self) {
        if self.remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.tracker.finish_step(self.step, Instant::now());
        }
    }
}

/// One part of a fragment collection, which runs beside the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollectStep {
    /// The fragments of the state diffed from, and of the lists they name.
    FromState,
    /// The fragments of the state collected, and of the lists they name.
    ToState,
    /// The node block list of the state diffed from.
    FromNodeBlocks,
    /// The node block list of the state collected.
    ToNodeBlocks,
    /// The file metadata block list of the state diffed from.
    FromFileMetadataBlocks,
    /// The file metadata block list of the state collected.
    ToFileMetadataBlocks,
    /// The walk of both trees for the file content the collected state adds.
    NewFiles,
    /// The fragments the revision's own metadata names.
    RevisionMetadata,
    /// The name tables of the changed node blocks. Starts once both node block lists are read,
    /// and is skipped if either read failed.
    NewNames,
    /// The lookups of the changed node blocks. Starts once both node block lists are read, and is
    /// skipped if either read failed.
    NewNodeBlocks,
    /// The metadata blobs of the changed file metadata blocks. Starts once both file metadata
    /// block lists are read, and is skipped if any step before it failed.
    FileMetadataBlobs,
    /// The fragments of the branch's metadata.
    BranchMetadata,
}

impl CollectStep {
    pub const ALL: [Self; 12] = [
        Self::FromState,
        Self::ToState,
        Self::FromNodeBlocks,
        Self::ToNodeBlocks,
        Self::FromFileMetadataBlocks,
        Self::ToFileMetadataBlocks,
        Self::NewFiles,
        Self::RevisionMetadata,
        Self::NewNames,
        Self::NewNodeBlocks,
        Self::FileMetadataBlobs,
        Self::BranchMetadata,
    ];

    /// The step's name in metric labels and logs.
    pub fn label(self) -> &'static str {
        match self {
            Self::FromState => "from_state",
            Self::ToState => "to_state",
            Self::FromNodeBlocks => "from_node_blocks",
            Self::ToNodeBlocks => "to_node_blocks",
            Self::FromFileMetadataBlocks => "from_file_metadata_blocks",
            Self::ToFileMetadataBlocks => "to_file_metadata_blocks",
            Self::NewFiles => "new_files",
            Self::RevisionMetadata => "revision_metadata",
            Self::NewNames => "new_names",
            Self::NewNodeBlocks => "new_node_blocks",
            Self::FileMetadataBlobs => "file_metadata_blobs",
            Self::BranchMetadata => "branch_metadata",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// Where one [`CollectStep`] stood when a [`CollectSteps`] was taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepState {
    /// No collection was running the step, and at least one had not reached it yet.
    Waiting,
    /// A collection was running the step.
    Running,
    /// Every collection had finished the step, the last of them this long after the first
    /// collection started.
    Finished(Duration),
}

#[derive(Clone, Copy, Debug)]
pub struct CollectSteps {
    states: [StepState; CollectStep::ALL.len()],
}

impl CollectSteps {
    /// Where `step` stood.
    pub fn state(&self, step: CollectStep) -> StepState {
        self.states[step.index()]
    }

    /// The time from the start of the first collection to when the last one finished `step`,
    /// `None` if some collection had not.
    pub fn finished(&self, step: CollectStep) -> Option<Duration> {
        match self.state(step) {
            StepState::Finished(finished) => Some(finished),
            StepState::Waiting | StepState::Running => None,
        }
    }
}

/// Each step as `name=<ms>` once finished, `name=running` or `name=waiting` otherwise.
impl fmt::Display for CollectSteps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, step) in CollectStep::ALL.into_iter().enumerate() {
            if index != 0 {
                f.write_str(" ")?;
            }
            match self.state(step) {
                StepState::Finished(finished) => {
                    write!(f, "{}={}", step.label(), finished.as_millis())?;
                }
                StepState::Running => write!(f, "{}=running", step.label())?,
                StepState::Waiting => write!(f, "{}=waiting", step.label())?,
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct InFlightCounter {
    in_flight: AtomicU64,
    peak: AtomicU64,
    total: AtomicU64,
}

impl InFlightCounter {
    fn track(&self) -> StoreRequestGuard<'_> {
        let in_flight = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak.fetch_max(in_flight, Ordering::Relaxed);
        self.total.fetch_add(1, Ordering::Relaxed);
        StoreRequestGuard { counter: self }
    }

    fn peak(&self) -> u64 {
        self.peak.load(Ordering::Relaxed)
    }

    fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
}

/// A request counted as in flight by a [`StoreRequestTracker`] until dropped.
pub struct StoreRequestGuard<'a> {
    counter: &'a InFlightCounter,
}

impl Drop for StoreRequestGuard<'_> {
    fn drop(&mut self) {
        self.counter.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}
