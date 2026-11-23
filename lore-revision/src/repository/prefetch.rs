// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Warming a revision's tree into the local store, walked from the root or loaded block by block,
//! the access recordings that name what to warm, and fetching the node blocks states differ in into
//! the store a server's reads fill.

use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::mem::offset_of;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use lore_base::lore_spawn;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_base::types::TypedBytes;
use lore_error_set::prelude::*;
use lore_storage::ImmutableStore;
use lore_storage::ReadOptions;
use lore_storage::StoreMatchResult;
use lore_transport::StorageSession;
use parking_lot::Mutex;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
use zerocopy::FromBytes;

use crate::immutable;
use crate::immutable::CacheGets;
use crate::lore_debug;
use crate::lore_warn;
use crate::node::NodeBlockData;
use crate::node::NodeBlockFormat;
use crate::repository::RepositoryContext;
use crate::state::State;
use crate::state::StateError;
use crate::state::block::read_stored_node_block;
use crate::state::stored_name_table;
use crate::util::path::RelativePath;

/// The environment variable that selects [`TreePrefetch`] for a [`plan`] given none.
const TREE_PREFETCH_ENV: &str = "LORE_TREE_PREFETCH_DEPTH";

/// How much of a revision's tree a warm-up loads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TreePrefetch {
    /// Nothing.
    Off,
    /// The view-filtered tree down to this many levels below the root: every block a listing of a
    /// directory above that depth reads.
    Levels(NonZeroU32),
    /// The whole view-filtered tree, walked.
    View,
    /// Every node block, with its name table and file-metadata block, taken from the block address
    /// lists: no walk and no view. The default, as no block waits for the one before it on a
    /// sibling chain, as a walk's blocks do.
    #[default]
    Blocks,
}

impl TreePrefetch {
    /// `value` as `LORE_TREE_PREFETCH_DEPTH` spells it: a level count (`0` for none), `all` for
    /// [`Self::View`] or `blocks` for [`Self::Blocks`]. `None` for anything else.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("all") {
            return Some(TreePrefetch::View);
        }
        if value.eq_ignore_ascii_case("blocks") {
            return Some(TreePrefetch::Blocks);
        }
        Some(NonZeroU32::new(value.parse().ok()?).map_or(TreePrefetch::Off, TreePrefetch::Levels))
    }
}

/// One path of an access recording.
#[derive(Clone)]
pub struct RecordedPath {
    pub path: RelativePath,
    /// Recorded with a trailing `/`: its listing is warmed rather than its content.
    pub directory: bool,
}

/// The paths of an access recording in first-access order, each once, and the node block indices
/// any line names after a tab, separated by spaces, in first-need order, each once. A line holds
/// its path up to the tab or the line end, spelled as it is; the root is `/` and a directory ends
/// with `/`. Paths are compared as parsed, so `a//b` repeats `a/b`. A line that is not UTF-8 is
/// skipped whole; a path not within the tree or naming the root as a file, and an index that does
/// not parse, are skipped alone.
#[lore_macro::test_pub]
fn parse_recording(recording: &[u8]) -> (Vec<RecordedPath>, Vec<usize>) {
    let mut paths = Vec::new();
    let mut blocks = Vec::new();
    for line in recording.split(|&byte| byte == b'\n') {
        let Ok(line) = std::str::from_utf8(line.strip_suffix(b"\r").unwrap_or(line)) else {
            continue;
        };
        let (path, hints) = line.split_once('\t').unwrap_or((line, ""));
        blocks.extend(
            hints
                .split(' ')
                .filter_map(|hint| hint.parse::<usize>().ok()),
        );
        let directory = path.ends_with('/');
        if let Ok(path) = RelativePath::new_from_initial_path(path)
            && (directory || !path.is_empty())
        {
            paths.push(RecordedPath { path, directory });
        }
    }
    let first = {
        let keys: Vec<(&str, bool)> = paths
            .iter()
            .map(|recorded| (recorded.path.as_str(), recorded.directory))
            .collect();
        first_occurrences(&keys)
    };
    let mut first = first.into_iter();
    paths.retain(|_| first.next().unwrap_or(false));
    let mut first = first_occurrences(&blocks).into_iter();
    blocks.retain(|_| first.next().unwrap_or(false));
    (paths, blocks)
}

/// Whether each item is the first of its value, found by sorting rather than hashing.
fn first_occurrences<T: Ord>(items: &[T]) -> Vec<bool> {
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_unstable_by(|&a, &b| items[a].cmp(&items[b]).then(a.cmp(&b)));
    let mut first = vec![true; items.len()];
    for pair in order.windows(2) {
        if items[pair[0]] == items[pair[1]] {
            first[pair[1]] = false;
        }
    }
    first
}

/// An access recording, as read from its file.
pub struct Recording {
    /// The file it was read from.
    pub source: PathBuf,
    pub paths: Vec<RecordedPath>,
    /// Node block indices the recording names, in first-need order.
    pub blocks: Vec<usize>,
}

/// What a warm-up loads: the tree, and a recording's paths and blocks.
pub struct PrefetchPlan {
    pub tree: TreePrefetch,
    pub recording: Option<Recording>,
}

impl PrefetchPlan {
    /// Whether the plan has no tree warm-up and no recording.
    pub fn is_empty(&self) -> bool {
        self.tree == TreePrefetch::Off && self.recording.is_none()
    }
}

/// The plan of `tree`, else the environment's setting, and the recording at `recording`, read now
/// so that a file that cannot be read is reported to the command.
pub async fn plan(tree: Option<TreePrefetch>, recording: Option<&str>) -> PrefetchPlan {
    let recording = match recording {
        Some(source) => read_recording(source).await,
        None => None,
    };
    PrefetchPlan {
        tree: tree.unwrap_or_else(tree_prefetch_from_env),
        recording,
    }
}

/// The setting [`TREE_PREFETCH_ENV`] names in this process's environment; the default
/// ([`TreePrefetch::Blocks`]) where it is unset or names none.
fn tree_prefetch_from_env() -> TreePrefetch {
    let Ok(value) = std::env::var(TREE_PREFETCH_ENV) else {
        return TreePrefetch::default();
    };
    TreePrefetch::parse(&value).unwrap_or_else(|| {
        lore_warn!(
            "{TREE_PREFETCH_ENV}={value} is not a level count, `all` or `blocks`; the tree \
             prefetch uses its default"
        );
        TreePrefetch::default()
    })
}

/// Largest prefetch file [`plan`] reads. A recording holds a line per first access, so a build that
/// opens a few hundred thousand files records tens of MiB, past `lore_io::WHOLE_FILE_LIMIT`.
#[lore_macro::test_pub]
const PREFETCH_FILE_LIMIT: usize = 128 << 20;

/// The recording at `source`, taken in one positional read of the whole file, as the parse needs
/// every byte at once; `None`, logged, where the file cannot be read or exceeds
/// [`PREFETCH_FILE_LIMIT`].
async fn read_recording(source: &str) -> Option<Recording> {
    let read = async {
        let file = lore_io::IoDriver::global()
            .open(source, &lore_io::OpenOptions::new().read(true))
            .await?;
        let len = file.metadata().await?.len();
        match usize::try_from(len) {
            Ok(len) if len <= PREFETCH_FILE_LIMIT => file.read_exact_at(len, 0).await,
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{len} bytes exceeds the {PREFETCH_FILE_LIMIT} byte prefetch file limit"),
            )),
        }
    };
    let bytes = match read.await {
        Ok(bytes) => bytes,
        Err(error) => {
            lore_warn!(
                "Prefetch file {source} was not read, nothing is prefetched from it: {error}"
            );
            return None;
        }
    };
    let (paths, blocks) = parse_recording(&bytes);
    Some(Recording {
        source: PathBuf::from(source),
        paths,
        blocks,
    })
}

/// Answers whether a warm-up is to start no more work.
pub type StopWhen = Arc<dyn Fn() -> bool + Send + Sync>;

/// Node blocks a block warm-up has in flight, each fetched with its name table after it and its
/// file-metadata block beside it.
const BLOCKS_IN_FLIGHT: usize = 512;

/// What a block warm-up asked for of each kind, the content bytes of the node blocks it read, the
/// payload bytes it fetched of the other kinds, and its fetches that failed.
#[derive(Clone, Copy, Debug, Default)]
pub struct WarmedBlocks {
    pub blocks: u64,
    pub block_bytes: u64,
    pub name_tables: u64,
    pub name_table_bytes: u64,
    pub file_metadata: u64,
    pub file_metadata_bytes: u64,
    pub failures: u64,
}

impl std::ops::AddAssign for WarmedBlocks {
    fn add_assign(&mut self, other: Self) {
        self.blocks += other.blocks;
        self.block_bytes += other.block_bytes;
        self.name_tables += other.name_tables;
        self.name_table_bytes += other.name_table_bytes;
        self.file_metadata += other.file_metadata;
        self.file_metadata_bytes += other.file_metadata_bytes;
        self.failures += other.failures;
    }
}

/// Fetches node blocks of `state`'s tree into the local store with their name tables and
/// file-metadata blocks, taking the addresses from the block address lists: nothing is walked and
/// no view applies. `blocks` names the block indices in the order to fetch them, skipping any the
/// lists do not hold; `None` fetches every block. Every block fetch goes through one session,
/// [`BLOCKS_IN_FLIGHT`] blocks at a time ([`warm_block`]), at normal priority, and is checked on
/// receipt as a read's is; one that fails is counted, and the warm-up goes on. It reads nothing
/// where `stop` answers true at the start, starts no block once it does, and logs what it warmed
/// in one line.
pub async fn warm_blocks(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    blocks: Option<&[usize]>,
    stop: StopWhen,
) -> Result<WarmedBlocks, StateError> {
    if stop() {
        return Ok(WarmedBlocks::default());
    }
    let start = Instant::now();
    let (nodes, file_metadata) = state.block_address_lists(repository.clone()).await?;
    let fetch = WarmFetch::connect(repository).await?;
    let nodes = nodes.as_type_slice::<Hash>();
    let file_metadata = file_metadata.as_type_slice::<Hash>();
    let warmed = match blocks {
        Some(blocks) => {
            warm_block_indices(&fetch, nodes, file_metadata, blocks.iter().copied(), &stop).await
        }
        None => warm_block_indices(&fetch, nodes, file_metadata, 0..nodes.len(), &stop).await,
    }?;
    lore_debug!(
        "Warmed {} node blocks, {} name tables and {} file-metadata blocks, {} failed, in {:.3}s",
        warmed.blocks,
        warmed.name_tables,
        warmed.file_metadata,
        warmed.failures,
        start.elapsed().as_secs_f64()
    );
    Ok(warmed)
}

/// Fetches into the local store the node blocks of `from`'s tree and of `to`'s whose addresses
/// differ at the same block index, both sides', in index order, each with the name table it names.
/// Both states are of `repository`. A commit keeps each node at the block index it had in the
/// revision before, so between two revisions of one line of commits a block holding no changed
/// node has the same address on both sides. A merge adds the nodes the other line added at block
/// indices of its own, so between revisions of two lines that merged, a block can differ while
/// the nodes it holds are alike. The blocks are compared [`BLOCKS_IN_FLIGHT`] at a time: those the
/// local store holds are left out, in one query of the store for each, and the remote is connected
/// to only for a block the store lacks. They are fetched as [`warm_blocks`] fetches them, without
/// their file-metadata blocks.
///
/// It reads nothing where `repository` cannot fetch, as a context without a remote or one reading
/// the remote alone cannot ([`immutable::may_cache`]), or where `stopped` is cancelled at the
/// start. Once `stopped` is cancelled it compares and starts no more blocks and gives up a connect
/// in progress, and the fetches in flight leave their name tables unfetched. A warm-up that fails
/// has its fetches in flight aborted, and one that does not logs what it warmed in one line.
#[lore_macro::test_pub]
pub(crate) fn warm_differing_blocks(
    repository: Arc<RepositoryContext>,
    from: Arc<State>,
    to: Arc<State>,
    stopped: CancellationToken,
) -> impl Future<Output = Result<WarmedBlocks, StateError>> {
    warm_differing_blocks_in_chunks(repository, from, to, stopped, BLOCKS_IN_FLIGHT)
}

/// [`warm_differing_blocks`], comparing the blocks with the local store `chunk` at a time.
#[lore_macro::test_pub]
async fn warm_differing_blocks_in_chunks(
    repository: Arc<RepositoryContext>,
    from: Arc<State>,
    to: Arc<State>,
    stopped: CancellationToken,
    chunk: usize,
) -> Result<WarmedBlocks, StateError> {
    if stopped.is_cancelled() || !immutable::may_cache(&repository) {
        return Ok(WarmedBlocks::default());
    }
    let start = Instant::now();
    let lists = tokio::try_join!(
        from.node_block_address_list(repository.clone()),
        to.node_block_address_list(repository.clone()),
    );
    let revisions = (from.revision(), to.revision());
    drop((from, to));
    let (from_blocks, to_blocks) = lists?;
    let differing = differing_blocks(
        from_blocks.as_type_slice::<Hash>(),
        to_blocks.as_type_slice::<Hash>(),
    );
    let mut in_flight = InFlight::default();
    let started =
        start_lacking_blocks(&repository, differing, chunk, &stopped, &mut in_flight).await;
    let ((compared, lacking), warmed) = in_flight.end(started).await?;
    lore_debug!(
        "Warmed {} node blocks of the {compared} compared that revisions {} and {} differ in, \
         {lacking} lacking locally, with {} name tables, {} failed, in {:.3}s",
        warmed.blocks,
        revisions.0,
        revisions.1,
        warmed.name_tables,
        warmed.failures,
        start.elapsed().as_secs_f64()
    );
    Ok(warmed)
}

/// Starts in `in_flight` the fetch of each block of `differing` that the local store lacks, asking
/// the store about `chunk` blocks at a time and connecting once, for the first block it lacks, and
/// answers how many blocks it compared and how many of those the store lacked. Once `stopped` is
/// cancelled it compares no more blocks, and gives up a connect in progress.
async fn start_lacking_blocks(
    repository: &Arc<RepositoryContext>,
    mut differing: impl Iterator<Item = Address>,
    chunk: usize,
    stopped: &CancellationToken,
    in_flight: &mut InFlight,
) -> Result<(usize, usize), StateError> {
    let stop: StopWhen = {
        let stopped = stopped.clone();
        Arc::new(move || stopped.is_cancelled())
    };
    let mut fetch = None;
    // Grown as far as the first chunk needs, which for trees that differ in a few blocks is far
    // short of `chunk`, and reused for the rest.
    let mut blocks = Vec::new();
    let (mut compared, mut lacking) = (0, 0);
    while !stopped.is_cancelled() {
        blocks.clear();
        blocks.extend(differing.by_ref().take(chunk));
        if blocks.is_empty() {
            break;
        }
        compared += blocks.len();
        let lacking_blocks = immutable::not_held_locally(repository, &blocks)
            .await
            .forward::<StateError>("Querying the local store for the blocks two trees differ in")?;
        if lacking_blocks.is_empty() {
            continue;
        }
        lacking += lacking_blocks.len();
        let fetch = match &mut fetch {
            Some(fetch) => &*fetch,
            unconnected @ None => {
                let connected = tokio::select! {
                    biased;
                    () = stopped.cancelled() => None,
                    connected = WarmFetch::connect(repository.clone()) => Some(connected?),
                };
                let Some(connected) = connected else {
                    break;
                };
                &*unconnected.insert(connected)
            }
        };
        let lacking_blocks = lacking_blocks.into_iter().map(|block| (block, None));
        in_flight.start(fetch, lacking_blocks, &stop).await?;
    }
    Ok((compared, lacking))
}

/// Fetches the node blocks the list `nodes` holds at `indices`, each with the file-metadata block
/// `file_metadata` holds at its index, as [`warm_blocks`] does, and waits for every fetch to end.
/// An index the lists do not hold is skipped.
///
/// A function of its own because the fetches in flight are held across two awaits: in
/// [`warm_blocks`]' body they would take space in every state of its future.
async fn warm_block_indices(
    fetch: &WarmFetch,
    nodes: &[Hash],
    file_metadata: &[Hash],
    indices: impl Iterator<Item = usize>,
    stop: &StopWhen,
) -> Result<WarmedBlocks, StateError> {
    let blocks = indices.filter_map(|index| {
        block_address(nodes, index).map(|block| (block, block_address(file_metadata, index)))
    });
    let mut in_flight = InFlight::default();
    let started = in_flight.start(fetch, blocks, stop).await;
    let ((), warmed) = in_flight.end(started).await?;
    Ok(warmed)
}

/// The addresses of the node blocks the lists `from` and `to` hold differently at the same index,
/// in index order, the one `from` holds first. A zero hash, or an index past a list's end, is a
/// block with nothing stored, which differs from any stored block and is not named.
#[lore_macro::test_pub]
fn differing_blocks<'a>(from: &'a [Hash], to: &'a [Hash]) -> impl Iterator<Item = Address> + 'a {
    (0..from.len().max(to.len()))
        .filter(move |&index| from.get(index) != to.get(index))
        .flat_map(move |index| [block_address(from, index), block_address(to, index)])
        .flatten()
}

/// [`warm_differing_blocks`] in a task of its own.
#[must_use = "dropping the warm-up aborts it"]
#[lore_macro::test_pub]
pub(crate) struct DifferingBlocksWarmUp {
    stopped: CancellationToken,
    task: AbortOnDropHandle<()>,
}

impl DifferingBlocksWarmUp {
    /// Starts warming the blocks `from` and `to` differ in. A warm-up that fails is logged, and
    /// leaves the blocks it did not fetch to be fetched as they are read. Dropped rather than
    /// finished, it is aborted.
    pub fn start(repository: Arc<RepositoryContext>, from: Arc<State>, to: Arc<State>) -> Self {
        let stopped = CancellationToken::new();
        let warm_up = warm_differing_blocks(repository, from, to, stopped.clone());
        let task = lore_spawn!(crate::subtask!(futures::FutureExt::map(
            warm_up,
            |warmed| {
                if let Err(error) = warmed {
                    lore_debug!("The warm-up of the blocks two trees differ in stopped: {error}");
                }
            }
        )));
        Self {
            stopped,
            task: AbortOnDropHandle::new(task),
        }
    }

    /// Stops the warm-up, as [`warm_differing_blocks`] stops once cancelled, and waits for the
    /// node block fetches it has in flight to end, so that nothing it holds outlives the caller.
    pub async fn finish(self) {
        self.stopped.cancel();
        if let Err(error) = self.task.await {
            lore_warn!("The warm-up of the blocks two trees differ in failed: {error}");
        }
    }
}

/// The address of the block `hashes` holds at `index`: `None` for a zero hash, or an index past
/// the end, a block with nothing stored.
fn block_address(hashes: &[Hash], index: usize) -> Option<Address> {
    hashes
        .get(index)
        .filter(|hash| !hash.is_zero())
        .map(|hash| Address::zero_context_hash(*hash))
}

/// What every block fetch of a block warm-up goes through: one session, the connection's cache get
/// permits, and the options a node block is read with.
#[derive(Clone)]
struct WarmFetch {
    repository: Arc<RepositoryContext>,
    session: Arc<StorageSession>,
    permits: CacheGets,
    options: ReadOptions,
}

impl WarmFetch {
    /// The fetch of a block warm-up through `repository`'s cache session
    /// ([`immutable::cache_session`]).
    async fn connect(repository: Arc<RepositoryContext>) -> Result<Self, StateError> {
        let (session, permits) = immutable::cache_session(&repository)
            .await
            .forward::<StateError>("Connecting for a block warm-up")?;
        Ok(Self {
            options: immutable::read_options_from_repository(&repository).with_cache(),
            repository,
            session,
            permits,
        })
    }

    /// Fetches the fragment at `address` the local store lacks, as [`immutable::cache`] does.
    fn cache(
        &self,
        address: Address,
        cache_fragmented: bool,
    ) -> impl Future<Output = immutable::Cached> {
        immutable::cache_through(
            self.repository.clone(),
            self.session.clone(),
            self.permits.clone(),
            vec![address],
            cache_fragmented,
        )
    }
}

/// The node block fetches of a block warm-up in flight, at most [`BLOCKS_IN_FLIGHT`], and what
/// those that ended warmed.
#[derive(Default)]
struct InFlight {
    blocks: JoinSet<WarmedBlocks>,
    warmed: WarmedBlocks,
}

impl InFlight {
    /// Starts fetching each node block of `blocks` with its file-metadata block beside it
    /// ([`warm_block`]), waiting for a fetch to end while [`BLOCKS_IN_FLIGHT`] are in flight, and
    /// starting none once `stop` answers true.
    ///
    /// Not an `async fn`, which would hold a second copy of its arguments.
    #[allow(clippy::manual_async_fn)]
    fn start<'a>(
        &'a mut self,
        fetch: &'a WarmFetch,
        blocks: impl Iterator<Item = (Address, Option<Address>)> + 'a,
        stop: &'a StopWhen,
    ) -> impl Future<Output = Result<(), StateError>> + 'a {
        async move {
            for (block, metadata) in blocks {
                while self.blocks.len() >= BLOCKS_IN_FLIGHT
                    && let Some(done) = self.blocks.join_next().await
                {
                    self.warmed += done.internal("Block warm-up task failed")?;
                }
                if stop() {
                    break;
                }
                let spawned = &mut self.blocks;
                lore_spawn!(
                    spawned,
                    warm_block(fetch.clone(), block, metadata, stop.clone())
                );
            }
            Ok(())
        }
    }

    /// Ends the fetches in flight, so that no fetch task outlives the warm-up: where `started`
    /// holds, waits for every one to end and answers it with what every fetch warmed, or the first
    /// fetch task that failed; where `started` failed, aborts them and waits for their tasks to end
    /// before answering its error.
    ///
    /// Not an `async fn`, which would hold a second copy of its arguments.
    #[allow(clippy::manual_async_fn)]
    fn end<T>(
        mut self,
        started: Result<T, StateError>,
    ) -> impl Future<Output = Result<(T, WarmedBlocks), StateError>> {
        async move {
            let started = match started {
                Ok(started) => started,
                Err(error) => {
                    self.blocks.shutdown().await;
                    return Err(error);
                }
            };
            let mut failed = None;
            while let Some(done) = self.blocks.join_next().await {
                match done {
                    Ok(warmed) => self.warmed += warmed,
                    Err(error) => {
                        failed.get_or_insert(error);
                    }
                }
            }
            if let Some(error) = failed {
                Err(error).internal("Block warm-up task failed")?;
            }
            Ok((started, self.warmed))
        }
    }
}

/// Fetches the node block at `block` and then the name table it names, beside the file-metadata
/// block at `metadata`, counting what fails. The node block is read whole
/// ([`lore_storage::read::load_fragment`]) from the local store, or where the store lacks it, under
/// one of the cache get permits, by a read that looks locally again and then fetches and stores it;
/// a failure to store is not counted, as a read's is not. Its content names the name table, so it
/// is expanded once and released before the name table is fetched. A node block that fails leaves
/// its name table unfetched, as does one fetched once `stop` answers true.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn warm_block(
    fetch: WarmFetch,
    block: Address,
    metadata: Option<Address>,
    stop: StopWhen,
) -> impl Future<Output = WarmedBlocks> {
    async move {
        let node_block = async {
            let load = |session: Option<Arc<StorageSession>>| {
                lore_storage::read::load_fragment(
                    fetch.repository.immutable_store(),
                    fetch.repository.id,
                    block,
                    fetch.options,
                    session,
                )
            };
            let loaded = if let Ok(local) = load(None).await {
                Ok(local)
            } else {
                let _permit = match &fetch.permits {
                    Some(permits) => permits.acquire().await.ok(),
                    None => None,
                };
                load(Some(fetch.session.clone())).await
            };
            let (block_bytes, name_table) = {
                let Ok((_, content)) = loaded else {
                    return WarmedBlocks {
                        blocks: 1,
                        failures: 1,
                        ..WarmedBlocks::default()
                    };
                };
                (content.len() as u64, name_table_of(&content))
            };
            let mut warmed = WarmedBlocks {
                blocks: 1,
                block_bytes,
                ..WarmedBlocks::default()
            };
            if let Some(address) = name_table
                && !stop()
            {
                let cached = fetch.cache(address, true).await;
                warmed.name_tables = 1;
                warmed.name_table_bytes = cached.bytes;
                warmed.failures = u64::from(cached.failure.is_some());
            }
            warmed
        };
        let metadata_block = async {
            let Some(address) = metadata else {
                return WarmedBlocks::default();
            };
            let cached = fetch.cache(address, false).await;
            WarmedBlocks {
                file_metadata: 1,
                file_metadata_bytes: cached.bytes,
                failures: u64::from(cached.failure.is_some()),
                ..WarmedBlocks::default()
            }
        };
        let (mut warmed, metadata_warmed) = tokio::join!(node_block, metadata_block);
        warmed += metadata_warmed;
        warmed
    }
}

/// The name table the node block `content` names: `None` for a block with none, or in a format
/// other than [`NodeBlockData`], whose name table a listing loads on demand.
#[lore_macro::test_pub]
fn name_table_of(content: &[u8]) -> Option<Address> {
    if content.len() != size_of::<NodeBlockData>() {
        return None;
    }
    let (version, _) =
        u32::read_from_prefix(&content[offset_of!(NodeBlockData, version)..]).ok()?;
    let (name_table, _) =
        Hash::read_from_prefix(&content[offset_of!(NodeBlockData, name_table)..]).ok()?;
    (version <= NodeBlockFormat::Nametable as u32 && !name_table.is_zero())
        .then(|| Address::zero_context_hash(name_table))
}

/// What the prefetches of one operation did. The counts only grow; a fetch still running when
/// its prefetch stopped is counted when it ends.
#[derive(Debug, Default)]
pub struct PrefetchTracker {
    runs: AtomicU64,
    /// Prefetches that ran to their end, their fetches included.
    finished: AtomicU64,
    /// Prefetches skipped for a state of more blocks than they were given to compare.
    skipped: AtomicU64,
    /// The time the finished prefetches took, in microseconds.
    finished_micros: AtomicU64,
    /// The node blocks at the indexes where the compared states differ that were looked for in the
    /// store the prefetch fills.
    blocks_changed: AtomicU64,
    blocks_missing: AtomicU64,
    blocks_fetched: AtomicU64,
    /// The name tables of the fetched blocks that were read, whether the store the prefetch fills
    /// held them or they were fetched into it.
    name_tables_fetched: AtomicU64,
    /// State loads, tree and block list reads, local queries and block and name table reads that
    /// failed.
    failed: AtomicU64,
}

/// A snapshot of a [`PrefetchTracker`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PrefetchCounts {
    pub runs: u64,
    pub finished: u64,
    pub skipped: u64,
    /// The time the finished prefetches took.
    pub elapsed: Duration,
    pub blocks_changed: u64,
    pub blocks_missing: u64,
    pub blocks_fetched: u64,
    pub name_tables_fetched: u64,
    pub failed: u64,
}

impl PrefetchTracker {
    pub fn counts(&self) -> PrefetchCounts {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        PrefetchCounts {
            runs: load(&self.runs),
            finished: load(&self.finished),
            skipped: load(&self.skipped),
            elapsed: Duration::from_micros(load(&self.finished_micros)),
            blocks_changed: load(&self.blocks_changed),
            blocks_missing: load(&self.blocks_missing),
            blocks_fetched: load(&self.blocks_fetched),
            name_tables_fetched: load(&self.name_tables_fetched),
            failed: load(&self.failed),
        }
    }

    fn add(counter: &AtomicU64, amount: usize) {
        counter.fetch_add(amount as u64, Ordering::Relaxed);
    }
}

impl fmt::Display for PrefetchCounts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "runs={} finished={} skipped={} ms={} blocks_changed={} blocks_missing={} \
             blocks_fetched={} name_tables_fetched={} failed={}",
            self.runs,
            self.finished,
            self.skipped,
            self.elapsed.as_millis(),
            self.blocks_changed,
            self.blocks_missing,
            self.blocks_fetched,
            self.name_tables_fetched,
            self.failed,
        )
    }
}

/// Fetches into `local` the node blocks at which the state of revision `state` differs from that of
/// any of `against`, both sides' block at each such index, each with the name table it names.
/// `local` must be the store that reads through `repository`'s store fill with what they read: the
/// prefetch asks `local` which blocks it holds, and reads the rest through `repository`'s store.
///
/// A revision's blocks keep their indexes in the revisions edited from it, so the blocks at an
/// index where two states' block hashes differ hold every node one changed against the other. A
/// block holding only nodes neither changed is not fetched, even where it holds the children of a
/// changed directory.
///
/// A state of more than `max_blocks` blocks is not compared, and nothing is fetched.
///
/// The blocks are compared with `local` and fetched as they are found, 512 at a time. Each fetch
/// is a task of its own that runs to its end even once this future is dropped, so a reader that
/// joined the fetch is answered; dropping the future stops the comparing and the fetches it would
/// start, and a fetch whose node block is still being read then leaves its name table unread.
/// It only fetches ahead and answers nothing: a failure is counted, and leaves what it would have
/// fetched unfetched.
pub fn prefetch_changed_node_blocks(
    repository: Arc<RepositoryContext>,
    local: Arc<dyn ImmutableStore>,
    state: Hash,
    against: Vec<Hash>,
    max_blocks: usize,
    tracker: Arc<PrefetchTracker>,
) -> impl Future<Output = ()> {
    prefetch_changed_node_blocks_in_chunks(
        repository,
        local,
        state,
        against,
        max_blocks,
        tracker,
        BLOCKS_IN_FLIGHT,
    )
}

/// [`prefetch_changed_node_blocks`], comparing the blocks with `local` `chunk` at a time.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
#[lore_macro::test_pub]
fn prefetch_changed_node_blocks_in_chunks(
    repository: Arc<RepositoryContext>,
    local: Arc<dyn ImmutableStore>,
    state: Hash,
    against: Vec<Hash>,
    max_blocks: usize,
    tracker: Arc<PrefetchTracker>,
    chunk: usize,
) -> impl Future<Output = ()> {
    async move {
        let started = tokio::time::Instant::now();
        PrefetchTracker::add(&tracker.runs, 1);
        let Some(lists) =
            compared_block_lists(&repository, state, against, max_blocks, &tracker).await
        else {
            return;
        };
        let Some((state_list, against_lists)) = lists.split_first() else {
            return;
        };
        if !fetch_changed_blocks(
            &repository,
            &local,
            state_list,
            against_lists,
            &tracker,
            chunk,
        )
        .await
        {
            return;
        }
        PrefetchTracker::add(&tracker.finished, 1);
        tracker.finished_micros.fetch_add(
            u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }
}

/// Compares the blocks at which the list `state` differs from any of `against` with `local`
/// `chunk` at a time, and fetches each it lacks as it is found ([`fetch_block`]), at most
/// [`BLOCKS_IN_FLIGHT`] at once. Answers whether it compared every block and every fetch ended.
///
/// A function of its own because the changed blocks are held across its awaits: in its caller's
/// body they would take space beside the loading of the states.
async fn fetch_changed_blocks(
    repository: &Arc<RepositoryContext>,
    local: &Arc<dyn ImmutableStore>,
    state: &[Hash],
    against: &[Vec<Hash>],
    tracker: &Arc<PrefetchTracker>,
    chunk: usize,
) -> bool {
    let stopped = CancellationToken::new();
    let _stop_when_dropped = stopped.clone().drop_guard();
    let permits = Arc::new(Semaphore::new(BLOCKS_IN_FLIGHT));
    let name_tables: Arc<Mutex<HashSet<Hash>>> = Arc::default();
    let mut changed = changed_blocks(state, against);
    // Grown as far as the first chunk needs, and reused for the rest.
    let mut blocks = Vec::new();
    loop {
        blocks.clear();
        blocks.extend(changed.by_ref().take(chunk));
        if blocks.is_empty() {
            break;
        }
        // A block can be named at more than one index; a chunk asks for it once.
        blocks.sort_unstable();
        blocks.dedup();
        PrefetchTracker::add(&tracker.blocks_changed, blocks.len());
        let missing = missing_locally(local, repository, &blocks, tracker).await;
        PrefetchTracker::add(&tracker.blocks_missing, missing.len());
        for block in missing {
            let Ok(permit) = permits.clone().acquire_owned().await else {
                return false;
            };
            lore_spawn!(fetch_block(
                repository.clone(),
                block,
                FetchShared {
                    stopped: stopped.clone(),
                    name_tables: name_tables.clone(),
                    tracker: tracker.clone(),
                },
                permit
            ));
        }
    }
    // Every permit back means every fetch has ended.
    permits.acquire_many(BLOCKS_IN_FLIGHT as u32).await.is_ok()
}

/// The stored node block hashes of the states of `state` and of those of `against` that load,
/// `state`'s first.
/// `None` where `state` or every one of `against` fails to load, or a state can't be compared.
async fn compared_block_lists(
    repository: &Arc<RepositoryContext>,
    state: Hash,
    against: Vec<Hash>,
    max_blocks: usize,
    tracker: &PrefetchTracker,
) -> Option<Vec<Vec<Hash>>> {
    let (state, against) = load_compared(repository, state, against, tracker).await?;
    node_block_lists(
        repository,
        std::iter::once(&state).chain(&against),
        max_blocks,
        tracker,
    )
    .await
}

/// The states of `state` and of those of `against` that load. `None` where `state` or every one of
/// `against` fails to.
async fn load_compared(
    repository: &Arc<RepositoryContext>,
    state: Hash,
    against: Vec<Hash>,
    tracker: &PrefetchTracker,
) -> Option<(Arc<State>, Vec<Arc<State>>)> {
    let mut loaded = Vec::with_capacity(against.len() + 1);
    let mut state_loaded = false;
    for (index, result) in futures::future::join_all(
        std::iter::once(state)
            .chain(against)
            .map(|revision| State::deserialize(repository.clone(), revision)),
    )
    .await
    .into_iter()
    .enumerate()
    {
        match result {
            Ok(state) => {
                state_loaded |= index == 0;
                loaded.push(state);
            }
            Err(_) => PrefetchTracker::add(&tracker.failed, 1),
        }
    }
    if !state_loaded || loaded.len() < 2 {
        return None;
    }
    let state = loaded.remove(0);
    Some((state, loaded))
}

/// The addresses of the blocks at each index where the list `state` holds a different block from
/// one of `against`: `state`'s block, then the block of each of `against` that differs, each named
/// once per index. A zero hash, or an index past a list's end, is a block with nothing stored,
/// which differs from any stored block and is not named.
fn changed_blocks<'a>(
    state: &'a [Hash],
    against: &'a [Vec<Hash>],
) -> impl Iterator<Item = Address> + 'a {
    let length = against.iter().map(Vec::len).fold(state.len(), usize::max);
    (0..length).flat_map(move |index| {
        let ours = state.get(index);
        let changed = against.iter().any(|list| list.get(index) != ours);
        let theirs = against
            .iter()
            .enumerate()
            .filter(move |&(position, list)| {
                let block = list.get(index);
                block != ours
                    && against[..position]
                        .iter()
                        .all(|earlier| earlier.get(index) != block)
            })
            .filter_map(move |(_, list)| block_address(list, index));
        changed
            .then(|| block_address(state, index))
            .flatten()
            .into_iter()
            .chain(theirs)
    })
}

/// The stored node block hashes of each of `states`, in block order. `None` where a state has
/// more than `max_blocks` blocks, or a tree or block list can't be read.
async fn node_block_lists(
    repository: &Arc<RepositoryContext>,
    states: impl Iterator<Item = &Arc<State>>,
    max_blocks: usize,
    tracker: &PrefetchTracker,
) -> Option<Vec<Vec<Hash>>> {
    let mut lists = Vec::new();
    for state in states {
        let Ok((block_count, hash_node)) = state
            .tree(repository.clone())
            .await
            .map(|tree| (tree.block_count as usize, tree.hash_node))
        else {
            PrefetchTracker::add(&tracker.failed, 1);
            return None;
        };
        // The block count sizes the block list read below, so it is checked first.
        if block_count > max_blocks {
            PrefetchTracker::add(&tracker.skipped, 1);
            return None;
        }
        if hash_node.is_zero() {
            lists.push(Vec::new());
            continue;
        }
        let Ok(list) = state
            .node_block_addresses(repository, hash_node, block_count)
            .await
        else {
            PrefetchTracker::add(&tracker.failed, 1);
            return None;
        };
        lists.push(
            list.as_type_slice::<Hash>()
                .iter()
                .take(block_count)
                .copied()
                .collect(),
        );
    }
    Some(lists)
}

/// The addresses of `addresses` whose payload `local` lacks. Where the query fails, none are, so
/// none of them is fetched.
async fn missing_locally(
    local: &Arc<dyn ImmutableStore>,
    repository: &RepositoryContext,
    addresses: &[Address],
    tracker: &PrefetchTracker,
) -> Vec<Address> {
    let mut resolved = vec![StoreMatchResult::default(); addresses.len()];
    if local
        .clone()
        .query(repository.id, addresses, &mut resolved)
        .await
        .is_err()
    {
        PrefetchTracker::add(&tracker.failed, 1);
        return Vec::new();
    }
    addresses
        .iter()
        .zip(resolved.iter())
        .filter(|(_, resolved)| !immutable::held_locally(resolved))
        .map(|(address, _)| *address)
        .collect()
}

/// What the block fetches of one prefetch share.
struct FetchShared {
    /// Cancelled once the prefetch is dropped.
    stopped: CancellationToken,
    /// The name tables a fetch has started reading.
    name_tables: Arc<Mutex<HashSet<Hash>>>,
    tracker: Arc<PrefetchTracker>,
}

/// Reads the node block at `block` through `repository`'s store, which fills the store the prefetch
/// fills with it, and then the name table it names, unless the prefetch has stopped by then or another of its
/// fetches has read that name table. Holds `permit` until it ends.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn fetch_block(
    repository: Arc<RepositoryContext>,
    block: Address,
    shared: FetchShared,
    permit: OwnedSemaphorePermit,
) -> impl Future<Output = ()> {
    async move {
        let tracker = &shared.tracker;
        match stored_name_table(read_stored_node_block(repository.clone(), block).await) {
            Ok(name_table) => {
                PrefetchTracker::add(&tracker.blocks_fetched, 1);
                // A prefetch dropped since this block's read started starts no more reads.
                if !name_table.is_zero()
                    && !shared.stopped.is_cancelled()
                    && shared.name_tables.lock().insert(name_table)
                {
                    let options = immutable::read_options_from_repository(&repository).with_cache();
                    let address = Address::zero_context_hash(name_table);
                    match immutable::read(repository, address, None, options).await {
                        Ok(_) => PrefetchTracker::add(&tracker.name_tables_fetched, 1),
                        Err(_) => PrefetchTracker::add(&tracker.failed, 1),
                    }
                }
            }
            Err(_) => PrefetchTracker::add(&tracker.failed, 1),
        }
        drop(permit);
    }
}
