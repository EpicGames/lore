// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The background warm-up a virtual clone starts once it has published: the paths of an access
//! recording, then the tree, fetched into the local store the mount reads.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;

use dashmap::DashMap;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use lore_base::lore_spawn;
use lore_base::types::Address;
use lore_transport::StorageSession;
use tokio::sync::Notify;
use tokio::task::AbortHandle;
use tokio::task::JoinSet;

use crate::fs::fuse::filesystem::FuseFilesystem;
use crate::fs::fuse::listing::entry_flags;
use crate::fs::fuse::served::Served;
use crate::immutable;
use crate::immutable::CacheGets;
use crate::lore_info;
use crate::lore_warn;
use crate::repository::clone::warm_tree;
use crate::repository::prefetch::PrefetchPlan;
use crate::repository::prefetch::RecordedPath;
use crate::repository::prefetch::Recording;
use crate::repository::prefetch::StopWhen;
use crate::repository::prefetch::TreePrefetch;
use crate::repository::prefetch::WarmedBlocks;
use crate::repository::prefetch::warm_blocks;

/// Recorded paths resolving at once, started in recorded order.
const RECORDING_RESOLVING: usize = 2048;

/// Resolved files whose contents are fetching at once.
const RECORDING_CONTENT_FETCHES: usize = 512;

const MIB: f64 = (1u64 << 20) as f64;

/// The warm-ups running on one provider, their tasks by start, and a wake-up for when the last
/// one ends.
#[derive(Default)]
pub(crate) struct PrefetchTracker {
    running: AtomicU32,
    finished: Notify,
    tasks: DashMap<u64, AbortHandle>,
    started: AtomicU64,
}

/// Counts one running warm-up for as long as it lives, however its task ends.
struct Running {
    filesystem: Arc<FuseFilesystem>,
    id: u64,
}

impl Running {
    fn start(filesystem: Arc<FuseFilesystem>) -> Self {
        filesystem.prefetch.running.fetch_add(1, Ordering::AcqRel);
        let id = filesystem.prefetch.started.fetch_add(1, Ordering::Relaxed);
        Self { filesystem, id }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let tracker = &self.filesystem.prefetch;
        tracker.tasks.remove(&self.id);
        if tracker.running.fetch_sub(1, Ordering::AcqRel) == 1 {
            tracker.finished.notify_waiters();
        }
    }
}

/// What resolving one recorded path found.
enum Resolved {
    /// A directory, its listing and its ancestors' now built.
    Directory,
    /// A file, with its content address.
    File(Address),
    /// Absent from the revision, excluded by the view, or of the other kind.
    Skipped,
    /// A read failed.
    Failed,
}

#[derive(Default)]
struct RecordingWarmed {
    directories: u64,
    files: u64,
    /// Payload bytes fetched for the files' contents.
    content_bytes: u64,
    skipped: u64,
    /// Paths whose read failed, and files whose content failed to fetch.
    failed: u64,
    /// A swap published another revision before every path was resolved.
    superseded: bool,
}

impl FuseFilesystem {
    /// Warms what `plan` names in one background task and returns at once: the recording's
    /// paths first, in recorded order, then the tree. Each part logs one summary line.
    /// [`Self::stop_prefetch`] cancels the task.
    pub fn start_prefetch(self: &Arc<Self>, plan: PrefetchPlan) {
        let running = Running::start(self.clone());
        let id = running.id;
        let task = lore_spawn!(async move {
            Box::pin(running.filesystem.prefetch(plan)).await;
            drop(running);
        });
        self.prefetch.tasks.insert(id, task.abort_handle());
        if task.is_finished() {
            self.prefetch.tasks.remove(&id);
        }
    }

    /// Cancels every warm-up running on this provider, as an unmount does.
    pub fn stop_prefetch(&self) {
        self.prefetch.tasks.retain(|_, task| {
            task.abort();
            false
        });
    }

    /// Whether a warm-up started by [`Self::start_prefetch`] is still running.
    pub fn prefetch_running(&self) -> bool {
        self.prefetch.running.load(Ordering::Acquire) > 0
    }

    /// Returns once no warm-up is running.
    pub async fn prefetch_finished(&self) {
        let tracker = &self.prefetch;
        loop {
            let finished = tracker.finished.notified();
            if tracker.running.load(Ordering::Acquire) == 0 {
                return;
            }
            finished.await;
        }
    }

    async fn prefetch(&self, plan: PrefetchPlan) {
        let served = self.served();
        if let Some(recording) = &plan.recording {
            self.scoped(async {
                tokio::join!(
                    Box::pin(prefetch_recorded_blocks(&served, recording)),
                    Box::pin(self.prefetch_recording(&served, recording))
                )
            })
            .await;
        }
        if plan.tree != TreePrefetch::Off {
            Box::pin(self.scoped(prefetch_tree(&served, plan.tree))).await;
        }
    }

    async fn prefetch_recording(&self, served: &Served, recording: &Recording) {
        let started = Instant::now();
        let fetched = lore_storage::remote_fetch_bytes();
        let warmed = self.warm_recording(served, &recording.paths).await;
        lore_info!(
            "Prefetch of {} recorded paths from {} for revision {}{}: {} directories and {} files \
             warmed ({:.1} MiB of content), {} skipped, {} failed, {:.1} MiB fetched in {:.2} s",
            recording.paths.len(),
            recording.source.display(),
            served.revision(),
            if warmed.superseded {
                ", stopped by a swap"
            } else {
                ""
            },
            warmed.directories,
            warmed.files,
            mib(warmed.content_bytes),
            warmed.skipped,
            warmed.failed,
            fetched_since(fetched),
            started.elapsed().as_secs_f64()
        );
    }

    /// Builds the listing of every recorded directory and of every recorded file's ancestors,
    /// as a lookup would, and fetches every recorded file's content. Paths start resolving in
    /// recorded order, [`RECORDING_RESOLVING`] at a time, so that one slow listing holds up no
    /// other; each resolved file's content is cached through one session, taken once, in the
    /// order the files resolve, [`RECORDING_CONTENT_FETCHES`] files at a time. With no session,
    /// every file's content counts as failed.
    async fn warm_recording(&self, served: &Served, paths: &[RecordedPath]) -> RecordingWarmed {
        let mut warmed = RecordingWarmed::default();
        let session = immutable::cache_session(&served.repository)
            .await
            .inspect_err(|error| lore_warn!("Prefetch of recorded file contents: {error}"))
            .ok();
        let mut contents: JoinSet<(u64, bool)> = JoinSet::new();
        let mut resolved: VecDeque<Address> = VecDeque::new();
        let mut resolving = FuturesUnordered::new();
        let mut pending = paths.iter();
        loop {
            while resolving.len() < RECORDING_RESOLVING && !served.superseded() {
                let Some(recorded) = pending.next() else {
                    break;
                };
                resolving.push(self.resolve_recorded(served, recorded));
            }
            while contents.len() < RECORDING_CONTENT_FETCHES
                && let Some(address) = resolved.pop_front()
            {
                match &session {
                    Some(session) => fetch_content(&mut contents, served, session, address),
                    None => warmed.failed += 1,
                }
            }
            tokio::select! {
                Some(outcome) = resolving.next() => match outcome {
                    Resolved::Directory => warmed.directories += 1,
                    Resolved::File(address) => {
                        warmed.files += 1;
                        resolved.push_back(address);
                    }
                    Resolved::Skipped => warmed.skipped += 1,
                    Resolved::Failed => warmed.failed += 1,
                },
                Some(fetched) = contents.join_next() => {
                    let (bytes, failed) = fetched.unwrap_or((0, true));
                    warmed.content_bytes += bytes;
                    warmed.failed += u64::from(failed);
                }
                else => break,
            }
        }
        warmed.superseded = pending.len() > 0;
        warmed
    }

    async fn resolve_recorded(&self, served: &Served, recorded: &RecordedPath) -> Resolved {
        if recorded.directory {
            return match self.listing_at_path(served, &recorded.path).await {
                Ok(Some(_)) => Resolved::Directory,
                Ok(None) => Resolved::Skipped,
                Err(_) => Resolved::Failed,
            };
        }
        match self.entry_at_path(served, &recorded.path).await {
            Ok(Some(entry)) => {
                if entry.flags & entry_flags::IS_DIR == 0 {
                    Resolved::File(entry.address)
                } else {
                    Resolved::Skipped
                }
            }
            Ok(None) => Resolved::Skipped,
            Err(_) => Resolved::Failed,
        }
    }
}

/// Caches the content at `address` through `session` in a task of `contents`, which answers the
/// payload bytes fetched and whether the fetch failed.
fn fetch_content(
    contents: &mut JoinSet<(u64, bool)>,
    served: &Served,
    (session, permits): &(Arc<StorageSession>, CacheGets),
    address: Address,
) {
    let cached = immutable::cache_through(
        served.repository.clone(),
        session.clone(),
        permits.clone(),
        vec![address],
        true,
    );
    lore_spawn!(
        contents,
        futures::FutureExt::map(cached, |cached| (cached.bytes, cached.failure.is_some()))
    );
}

/// Fetches the node blocks `recording` names, in its order, as [`warm_blocks`] does, and logs
/// one summary line.
async fn prefetch_recorded_blocks(served: &Arc<Served>, recording: &Recording) {
    if recording.blocks.is_empty() {
        return;
    }
    let started = Instant::now();
    let watched = served.clone();
    let stop: StopWhen = Arc::new(move || watched.superseded());
    let warmed = warm_blocks(
        served.repository.clone(),
        served.state.clone(),
        Some(&recording.blocks),
        stop,
    )
    .await;
    match warmed {
        Ok(warmed) => lore_info!(
            "Prefetch of {} recorded blocks from {} for revision {}: {} in {:.2} s",
            recording.blocks.len(),
            recording.source.display(),
            served.revision(),
            warmed_blocks(&warmed),
            started.elapsed().as_secs_f64()
        ),
        Err(error) => lore_warn!(
            "Prefetch of recorded blocks for revision {} stopped after {:.2} s: {error}",
            served.revision(),
            started.elapsed().as_secs_f64()
        ),
    }
}

async fn prefetch_tree(served: &Arc<Served>, tree: TreePrefetch) {
    let started = Instant::now();
    let fetched = lore_storage::remote_fetch_bytes();
    let repository = served.repository.clone();
    let state = served.state.clone();
    let watched = served.clone();
    let stop: StopWhen = Arc::new(move || watched.superseded());
    let levels = match tree {
        TreePrefetch::Off => return,
        TreePrefetch::Levels(levels) => levels.get(),
        TreePrefetch::View => u32::MAX,
        TreePrefetch::Blocks => {
            match warm_blocks(repository, state, None, stop).await {
                Ok(warmed) => lore_info!(
                    "Tree prefetch of every block for revision {}: {}, {:.1} MiB fetched in \
                     {:.2} s",
                    served.revision(),
                    warmed_blocks(&warmed),
                    fetched_since(fetched),
                    started.elapsed().as_secs_f64()
                ),
                Err(error) => lore_warn!(
                    "Tree prefetch of every block for revision {} stopped after {:.2} s: {error}",
                    served.revision(),
                    started.elapsed().as_secs_f64()
                ),
            }
            return;
        }
    };
    let depth = if levels == u32::MAX {
        "the whole view".to_string()
    } else {
        format!("depth {levels}")
    };
    match warm_tree(repository, state, levels, stop).await {
        Ok(warmed) => lore_info!(
            "Tree prefetch to {depth} for revision {}: {} directories, {} blocks, {:.1} MiB \
             fetched in {:.2} s",
            served.revision(),
            warmed.directories,
            warmed.blocks,
            fetched_since(fetched),
            started.elapsed().as_secs_f64()
        ),
        Err(error) => lore_warn!(
            "Tree prefetch to {depth} for revision {} stopped after {:.2} s: {error}",
            served.revision(),
            started.elapsed().as_secs_f64()
        ),
    }
}

/// `warmed` as a summary line spells it: each kind's count and payload MiB.
fn warmed_blocks(warmed: &WarmedBlocks) -> String {
    format!(
        "{} node blocks ({:.1} MiB read), {} name tables ({:.1} MiB fetched), {} file-metadata \
         blocks ({:.1} MiB fetched), {} failed",
        warmed.blocks,
        mib(warmed.block_bytes),
        warmed.name_tables,
        mib(warmed.name_table_bytes),
        warmed.file_metadata,
        mib(warmed.file_metadata_bytes),
        warmed.failures
    )
}

/// MiB the process has fetched from remotes since the count stood at `before`.
fn fetched_since(before: u64) -> f64 {
    mib(lore_storage::remote_fetch_bytes().saturating_sub(before))
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / MIB
}
