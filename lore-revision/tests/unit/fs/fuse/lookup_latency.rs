// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Cold LOOKUP latency against a store that answers every read after a delay, as a remote does:
//! which fetches a listing build waits on one after another, and which it overlaps.

use std::collections::HashMap;
use std::collections::HashSet;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::test_util::TempDir;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_fuse::filesystem::Filesystem;
use lore_fuse::init::InitConfig;
use lore_fuse::mount::MountOptions;
use lore_fuse::mount::unmount_path;
use lore_fuse::session::Session;
use lore_fuse::session::SessionConfig;
use lore_fuse::skip_without_fuse;
use lore_fuse::transport::Notifier;
use lore_fuse::transport::device::DeviceTransport;
use lore_revision::fs::filesystem_provider::InstanceOperationImpl;
use lore_revision::fs::filesystem_provider::StaticDispatchInstanceOperation;
use lore_revision::fs::fuse::attr::AttrContext;
use lore_revision::fs::fuse::filesystem::FuseFilesystem;
use lore_revision::fs::fuse::inode::ROOT_INO;
use lore_revision::fs::fuse::mount::FuseMount;
use lore_revision::fs::fuse::operation::FuseOperation;
use lore_revision::fs::fuse::swap::NotificationSink;
use lore_revision::fs::fuse::swap::swap_mounted;
use lore_revision::instance::InstanceId;
use lore_revision::lore::RepositoryId;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeID;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::clone::warm_tree;
use lore_revision::repository::prefetch::PrefetchPlan;
use lore_revision::repository::prefetch::StopWhen;
use lore_revision::repository::prefetch::TreePrefetch;
use lore_revision::repository::prefetch::warm_blocks;
use lore_revision::state::State;
use lore_storage::FlushOptions;
use lore_storage::ImmutableStore;
use lore_storage::StoreError;
use lore_storage::StoreGetData;
use lore_storage::StoreMatchResult;
use lore_storage::StoreObliterateStats;
use lore_storage::hash::hash_string;

use crate::fs::fuse::fuse_fixture;
use crate::fs::fuse::fuse_fixture::RepositoryContextCreationArgsExt;
use crate::fs::fuse::fuse_test_util::req;

/// Subdirectories of `top`, one per revision, each holding more files than a node block has
/// nodes: every revision's nodes go to fresh blocks, so `top`'s sibling chain crosses one block
/// per subdirectory, as a directory's children do once they arrived over many revisions.
const SUBDIRECTORIES: usize = 8;
const FILES_PER_SUBDIRECTORY: usize = 520;

/// Read delays by what is read.
#[derive(Clone, Copy)]
struct Delays {
    name_table: Duration,
    file_metadata: Duration,
    other: Duration,
}

/// Name-table reads by hash, as a [`LatencyStore`] counts them.
type NameTableReads = Arc<parking_lot::Mutex<HashMap<Hash, usize>>>;

/// Answers every read after a delay chosen by the address, and reports nothing as held, so the
/// provider fetches what it prefetches rather than finding it present. Counts the reads of each
/// name table.
struct LatencyStore {
    inner: Arc<dyn ImmutableStore>,
    name_tables: HashSet<Hash>,
    file_metadata: HashSet<Hash>,
    delays: Delays,
    name_table_reads: NameTableReads,
}

impl LatencyStore {
    fn delay(&self, address: &Address) -> Duration {
        if self.name_tables.contains(&address.hash) {
            *self
                .name_table_reads
                .lock()
                .entry(address.hash)
                .or_default() += 1;
            self.delays.name_table
        } else if self.file_metadata.contains(&address.hash) {
            self.delays.file_metadata
        } else {
            self.delays.other
        }
    }
}

impl std::fmt::Debug for LatencyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LatencyStore")
    }
}

#[async_trait]
impl ImmutableStore for LatencyStore {
    /// Joins the claim of the store this one wraps.
    async fn hold_for_command(
        self: Arc<Self>,
    ) -> Result<Option<lore_storage::local::store_lock::StoreGuard>, StoreError> {
        self.inner.clone().hold_for_command().await
    }

    fn is_local(&self) -> bool {
        self.inner.is_local()
    }

    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        tokio::time::sleep(self.delay(&address)).await;
        self.inner.clone().get(partition, address).await
    }

    async fn get_metadata(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        tokio::time::sleep(self.delay(&address)).await;
        self.inner.clone().get_metadata(partition, address).await
    }

    async fn query(
        self: Arc<Self>,
        _partition: Partition,
        _addresses: &[Address],
        _results: &mut [StoreMatchResult],
    ) -> Result<(), StoreError> {
        Ok(())
    }

    async fn put(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
        force: bool,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .put(partition, address, fragment, payload, force)
            .await
    }

    async fn obliterate(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        stats: Arc<StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .obliterate(partition, address, stats)
            .await
    }

    async fn evict(
        self: Arc<Self>,
        max_capacity: usize,
        sync_data: bool,
        sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<usize, StoreError> {
        self.inner
            .clone()
            .evict(max_capacity, sync_data, sink)
            .await
    }

    async fn compact(
        self: Arc<Self>,
        max_size: usize,
        at: Option<usize>,
        sync_data: bool,
        sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<Option<usize>, StoreError> {
        self.inner
            .clone()
            .compact(max_size, at, sync_data, sink)
            .await
    }

    async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
        self.inner.clone().compact_resume_at().await
    }

    fn max_query_batch(&self) -> Option<usize> {
        self.inner.max_query_batch()
    }

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), StoreError> {
        self.inner.clone().flush(options).await
    }

    async fn verify(self: Arc<Self>, heal: bool) -> Result<(), StoreError> {
        self.inner.clone().verify(heal).await
    }

    async fn copy(
        self: Arc<Self>,
        source_partition: Partition,
        source_address: Address,
        destination_partition: Partition,
        destination_context: Context,
        behavior: lore_storage::immutable_store::CopyBehavior,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .copy(
                source_partition,
                source_address,
                destination_partition,
                destination_context,
                behavior,
            )
            .await
    }
}

fn attr_context() -> AttrContext {
    AttrContext {
        uid: 1000,
        gid: 1000,
        blksize: 1 << 17,
        base_time: 1_700_000_000_000,
    }
}

/// `top/dNN/fNNNN.txt` for one subdirectory `dir`.
fn subdirectory(dir: usize) -> Vec<(String, Vec<u8>)> {
    (0..FILES_PER_SUBDIRECTORY)
        .map(|file| {
            (
                format!("top/d{dir:02}/f{file:04}.txt"),
                format!("{dir}/{file}\n").into_bytes(),
            )
        })
        .collect()
}

/// The hashes of every node block's name table, and of the file metadata block list and every
/// block it names, read through the committed state.
async fn classified_hashes(fixture: &fuse_fixture::FuseFixture) -> (HashSet<Hash>, HashSet<Hash>) {
    let repository = fixture.repository.clone();
    let tree = fixture
        .state
        .tree(repository.clone())
        .await
        .expect("tree header");
    let mut name_tables = HashSet::new();
    for index in 0..tree.block_count as usize {
        let block = fixture
            .state
            .block(repository.clone(), index)
            .await
            .expect("node block");
        name_tables.insert(block.read_owned().node_block().name_table);
    }
    assert!(
        !tree.hash_file_metadata.is_zero(),
        "the fixture stores file metadata"
    );
    let list = lore_revision::immutable::read(
        repository,
        Address::zero_context_hash(tree.hash_file_metadata),
        None,
        lore_storage::ReadOptions::default(),
    )
    .await
    .expect("file metadata block list");
    let (hashes, _) = list.as_chunks::<{ size_of::<Hash>() }>();
    let mut file_metadata: HashSet<Hash> = hashes
        .iter()
        .map(|hash| Hash::from(hash.as_slice()))
        .filter(|hash| !hash.is_zero())
        .collect();
    file_metadata.insert(tree.hash_file_metadata);
    (name_tables, file_metadata)
}

/// A provider over a cold copy of the fixture's state, reading through a [`LatencyStore`].
async fn cold_provider(fixture: &fuse_fixture::FuseFixture, delays: Delays) -> Arc<FuseFilesystem> {
    cold_provider_counting(fixture, delays).await.0
}

/// [`cold_provider`], with the store's name-table read counts.
async fn cold_provider_counting(
    fixture: &fuse_fixture::FuseFixture,
    delays: Delays,
) -> (Arc<FuseFilesystem>, NameTableReads) {
    let (repository, state, name_table_reads) = cold_state(fixture, delays).await;
    let fs = lore_base::runtime::LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            FuseFilesystem::new(repository, state, attr_context()),
        )
        .await
        .expect("provider construction");
    (fs, name_table_reads)
}

/// A cold copy of the fixture's state read through a [`LatencyStore`], with the store's
/// name-table read counts.
async fn cold_state(
    fixture: &fuse_fixture::FuseFixture,
    delays: Delays,
) -> (Arc<RepositoryContext>, Arc<State>, NameTableReads) {
    let (name_tables, file_metadata) = lore_base::runtime::LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            classified_hashes(fixture),
        )
        .await;
    let name_table_reads = NameTableReads::default();
    let store: Arc<dyn ImmutableStore> = Arc::new(LatencyStore {
        inner: fixture.repository.immutable_store(),
        name_tables,
        file_metadata,
        delays,
        name_table_reads: name_table_reads.clone(),
    });
    let mutable_store = fixture
        .repository
        .try_mutable_store_arc()
        .expect("fixture repository must hold a write token");
    let repository = Arc::new(RepositoryContext::new(
        fuse_fixture::default_repository_creation_args(store, mutable_store)
            .with_path(&fixture.path)
            .with_id(fixture.repository.id)
            .with_instance_id(fixture.repository.instance_id),
    ));
    let state = lore_base::runtime::LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            State::deserialize(repository.clone(), fixture.state.revision()),
        )
        .await
        .expect("cold state");
    (repository, state, name_table_reads)
}

/// One revision per subdirectory of `top`. Every revision after the first also fills the tree's
/// file metadata blocks.
async fn committed_wide_tree() -> fuse_fixture::FuseFixture {
    let owned: Vec<Vec<(String, Vec<u8>)>> = (0..SUBDIRECTORIES).map(subdirectory).collect();
    let files: Vec<Vec<(&str, &[u8])>> = owned
        .iter()
        .map(|revision| {
            revision
                .iter()
                .map(|(path, contents)| (path.as_str(), contents.as_slice()))
                .collect()
        })
        .collect();
    let revisions: Vec<&[(&str, &[u8])]> = files.iter().map(Vec::as_slice).collect();
    fuse_fixture::committed_revisions(&revisions).await
}

/// Each block on a directory's sibling chain is found only by loading the block before it, so
/// the blocks load one after another; their name tables must load alongside, not in turn.
#[tokio::test]
async fn a_cold_listing_loads_name_tables_alongside_the_sibling_chain() {
    let fixture = committed_wide_tree().await;
    let delays = Delays {
        name_table: Duration::from_millis(150),
        file_metadata: Duration::from_millis(25),
        other: Duration::from_millis(25),
    };
    let fs = cold_provider(&fixture, delays).await;
    let top = fs
        .lookup(&req(), ROOT_INO, b"top")
        .await
        .expect("lookup of top");

    let started = Instant::now();
    fs.lookup(&req(), top.ino, b"d03")
        .await
        .expect("lookup under top");
    let elapsed = started.elapsed();

    let listing = fs
        .listing_for(&fs.served(), top.ino)
        .await
        .expect("listing of top");
    let mut blocks: Vec<usize> = listing
        .node_ids
        .iter()
        .map(|&node| NodeBlock::index(node))
        .collect();
    blocks.sort_unstable();
    blocks.dedup();
    assert!(
        blocks.len() >= 6,
        "top's children span {} blocks, too few to tell overlap from turn-taking",
        blocks.len()
    );
    let turn_taking_floor = delays.name_table * (blocks.len() as u32 - 1);
    assert!(
        elapsed < turn_taking_floor,
        "listing {} blocks took {elapsed:?}, not under the {turn_taking_floor:?} name tables \
         loaded in turn would take",
        blocks.len()
    );
}

/// The file metadata block list and blocks are a prefetch: a listing never waits for them.
#[tokio::test]
async fn a_cold_listing_does_not_wait_for_file_metadata() {
    let fixture = committed_wide_tree().await;
    let delays = Delays {
        name_table: Duration::from_millis(20),
        file_metadata: Duration::from_millis(1500),
        other: Duration::from_millis(20),
    };
    let fs = cold_provider(&fixture, delays).await;

    let started = Instant::now();
    fs.lookup(&req(), ROOT_INO, b"top")
        .await
        .expect("lookup of top");
    let elapsed = started.elapsed();

    assert!(
        elapsed < delays.file_metadata / 2,
        "the first listing took {elapsed:?} with file metadata reads taking {:?}",
        delays.file_metadata
    );
}

/// Concurrent LOOKUPs of names in one cold directory share one build of its listing: every
/// name table on its sibling chain is read once.
#[tokio::test]
async fn concurrent_lookups_in_one_cold_directory_build_its_listing_once() {
    let fixture = committed_wide_tree().await;
    let delays = Delays {
        name_table: Duration::from_millis(100),
        file_metadata: Duration::from_millis(5),
        other: Duration::from_millis(20),
    };
    let (fs, name_table_reads) = cold_provider_counting(&fixture, delays).await;
    let top = fs
        .lookup(&req(), ROOT_INO, b"top")
        .await
        .expect("lookup of top");

    let lookups = (0..SUBDIRECTORIES).map(|dir| {
        let fs = fs.clone();
        lore_base::lore_spawn!(async move {
            fs.lookup(&req(), top.ino, format!("d{dir:02}").as_bytes())
                .await
                .is_ok()
        })
    });
    for found in futures::future::join_all(lookups).await {
        assert!(found.expect("lookup task"), "every subdirectory is found");
    }

    let reads = name_table_reads.lock();
    let most = reads.values().copied().max().unwrap_or(0);
    assert_eq!(
        most, 1,
        "a name table was read {most} times; the listing was built more than once"
    );
}

/// Concurrent path resolutions under one cold directory the kernel has not looked up, as the
/// recording prefetch makes them, share one build of its listing.
#[tokio::test]
async fn concurrent_path_resolutions_under_one_cold_directory_build_its_listing_once() {
    let fixture = committed_wide_tree().await;
    let delays = Delays {
        name_table: Duration::from_millis(100),
        file_metadata: Duration::from_millis(5),
        other: Duration::from_millis(20),
    };
    let (fs, name_table_reads) = cold_provider_counting(&fixture, delays).await;
    fs.root_listing(&fs.served())
        .await
        .expect("the root listing");
    name_table_reads.lock().clear();

    let resolutions = (0..SUBDIRECTORIES).map(|dir| {
        let fs = fs.clone();
        lore_base::lore_spawn!(async move {
            let served = fs.served();
            let path = lore_revision::util::path::RelativePath::new_from_initial_path(format!(
                "top/d{dir:02}"
            ))
            .expect("path");
            fs.entry_at_path(&served, &path)
                .await
                .is_ok_and(|found| found.is_some())
        })
    });
    for found in futures::future::join_all(resolutions).await {
        assert!(
            found.expect("resolution task"),
            "every subdirectory is found"
        );
    }

    let reads = name_table_reads.lock();
    let most = reads.values().copied().max().unwrap_or(0);
    assert_eq!(
        most, 1,
        "a name table was read {most} times; the listing was built more than once"
    );
}

/// A LOOKUP dropped while its listing builds, as an INTERRUPT drops a deferred one, leaves no
/// build behind.
#[tokio::test]
async fn a_dropped_listing_build_leaves_no_build_in_flight() {
    let fixture = committed_wide_tree().await;
    let delays = Delays {
        name_table: Duration::from_millis(200),
        file_metadata: Duration::from_millis(5),
        other: Duration::from_millis(200),
    };
    let fs = cold_provider(&fixture, delays).await;
    let top = fs
        .lookup(&req(), ROOT_INO, b"top")
        .await
        .expect("lookup of top");
    assert_eq!(fs.listing_builds_in_flight(), 0);

    let dropped = tokio::time::timeout(
        Duration::from_millis(50),
        fs.lookup(&req(), top.ino, b"d00"),
    )
    .await;
    assert!(
        dropped.is_err(),
        "the lookup is still building when dropped"
    );
    assert_eq!(fs.listing_builds_in_flight(), 0);
}

/// An unmount cancels the background prefetch a clone started on the mount, however far the
/// prefetch has come.
#[tokio::test(flavor = "multi_thread")]
async fn an_unmount_cancels_a_running_prefetch() {
    skip_without_fuse!();
    let fixture = committed_wide_tree().await;
    let delays = Delays {
        name_table: Duration::from_secs(5),
        file_metadata: Duration::from_secs(5),
        other: Duration::from_secs(5),
    };
    let (repository, state, _) = cold_state(&fixture, delays).await;
    let dir = TempDir::new("lore-fuse-prefetch-cancel-");
    let mount = lore_base::runtime::LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            FuseMount::mount(
                repository,
                state,
                dir.path(),
                InstanceId::generate(),
                1_700_000_000_000,
            ),
        )
        .await
        .expect("mount");
    lore_base::runtime::LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            mount.filesystem.start_prefetch(PrefetchPlan {
                tree: TreePrefetch::Blocks,
                recording: None,
            });
        })
        .await;
    assert!(mount.filesystem.prefetch_running());

    // Lazy: a desktop service's LOOKUP in the new mount waits on the same slow reads and holds
    // it busy, which would refuse a plain unmount.
    mount.unmount(true).await.expect("unmount");
    tokio::time::timeout(Duration::from_secs(1), mount.filesystem.prefetch_finished())
        .await
        .expect("the prefetch stops with the mount");
}

/// A tree warm-up of a revision no longer served starts no work: no block batch, which would
/// reach for the fixture's missing remote, and no block of a walk.
#[tokio::test]
async fn tree_warm_ups_of_a_superseded_revision_start_nothing() {
    let fixture = committed_wide_tree().await;
    let repository = fixture.repository.clone();
    let state = fixture.state.clone();
    let (blocks, walked, served) = lore_base::runtime::LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async move {
            let superseded: StopWhen = Arc::new(|| true);
            let blocks = warm_blocks(repository.clone(), state.clone(), None, superseded.clone())
                .await
                .expect("the block warm-up");
            let walked = warm_tree(repository.clone(), state.clone(), u32::MAX, superseded)
                .await
                .expect("the walk");
            let served = warm_tree(repository, state, u32::MAX, Arc::new(|| false))
                .await
                .expect("the walk");
            (blocks.blocks, walked.blocks, served.blocks)
        })
        .await;
    assert_eq!((blocks, walked), (0, 0));
    assert!(served > 0, "a walk of the served revision loads its blocks");
}

/// Unmounts on drop, lazily if a plain unmount is refused.
struct UnmountGuard(PathBuf);

impl Drop for UnmountGuard {
    fn drop(&mut self) {
        if unmount_path(&self.0, false).is_err() {
            let _ = unmount_path(&self.0, true);
        }
    }
}

struct Mounted {
    // Field order is drop order: the session stops before the unmount.
    session: Arc<Session<FuseFilesystem, DeviceTransport>>,
    _guard: UnmountGuard,
    root: PathBuf,
    _dir: TempDir,
}

/// Mounts `fs` read-only with `workers` session workers.
async fn mount(fs: Arc<FuseFilesystem>, workers: usize) -> Mounted {
    let dir = TempDir::new("lore-fuse-lookup-latency-");
    let options = MountOptions {
        fsname: "lore:lookup-latency".to_string(),
        max_read: 1 << 17,
        fusermount: None,
    };
    let mount = Arc::new(lore_fuse::mount::mount(dir.path(), &options).expect("mount"));
    let guard = UnmountGuard(dir.path().to_path_buf());
    let session = Session::start(
        fs.clone(),
        Arc::new(DeviceTransport::new(mount)),
        SessionConfig {
            workers,
            init: InitConfig {
                zero_message_open: true,
                ..InitConfig::default()
            },
        },
    )
    .await
    .expect("session must negotiate against a real kernel");
    fs.set_notifier(session.notifier().expect("notifier"))
        .expect("the notification thread");
    Mounted {
        session,
        _guard: guard,
        root: dir.path().to_path_buf(),
        _dir: dir,
    }
}

/// Stats every path at once, each on its own thread: the core blocking pool holds too few
/// threads to issue them together.
async fn stat_all(paths: Vec<PathBuf>) -> Vec<std::io::Result<std::fs::Metadata>> {
    let (done, finished) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let results = std::thread::scope(|scope| {
            let stats: Vec<_> = paths
                .iter()
                .map(|path| scope.spawn(move || std::fs::symlink_metadata(path)))
                .collect();
            stats
                .into_iter()
                .map(|stat| stat.join().expect("stat thread"))
                .collect()
        });
        let _ = done.send(results);
    });
    finished.await.expect("stat threads")
}

/// `top/dNN/seed.txt` for every subdirectory in a first revision, then one revision per
/// subdirectory filling it with [`subdirectory`]'s files: a subdirectory's files sit in blocks
/// that neither `top`'s listing nor any other subdirectory's touches.
async fn committed_seeded_tree() -> fuse_fixture::FuseFixture {
    let seeds: Vec<(String, Vec<u8>)> = (0..SUBDIRECTORIES)
        .map(|dir| (format!("top/d{dir:02}/seed.txt"), b"seed\n".to_vec()))
        .collect();
    let owned: Vec<Vec<(String, Vec<u8>)>> = std::iter::once(seeds)
        .chain((0..SUBDIRECTORIES).map(subdirectory))
        .collect();
    let files: Vec<Vec<(&str, &[u8])>> = owned
        .iter()
        .map(|revision| {
            revision
                .iter()
                .map(|(path, contents)| (path.as_str(), contents.as_slice()))
                .collect()
        })
        .collect();
    let revisions: Vec<&[(&str, &[u8])]> = files.iter().map(Vec::as_slice).collect();
    fuse_fixture::committed_revisions(&revisions).await
}

/// Cold LOOKUPs in different directories each wait on their own listing build off the session's
/// workers: eight of them on two workers have more listing builds under way at once than two
/// workers each holding a LOOKUP could.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_cold_lookups_in_different_directories_build_their_listings_at_once() {
    skip_without_fuse!();
    let fixture = committed_seeded_tree().await;
    let delays = Delays {
        name_table: Duration::from_millis(100),
        file_metadata: Duration::from_millis(5),
        other: Duration::from_millis(100),
    };
    let fs = cold_provider(&fixture, delays).await;
    let mounted = mount(fs.clone(), 2).await;
    let top = mounted.root.join("top");

    let subdirectories = (0..SUBDIRECTORIES).map(|dir| top.join(format!("d{dir:02}")));
    for result in stat_all(subdirectories.collect()).await {
        result.expect("stat of a subdirectory");
    }
    let files = (0..SUBDIRECTORIES).map(|dir| top.join(format!("d{dir:02}/seed.txt")));
    let (results, most) = with_most_listing_builds(&fs, stat_all(files.collect())).await;
    for result in results {
        result.expect("stat of a file in a cold subdirectory");
    }

    tokio::time::timeout(Duration::from_secs(5), mounted.session.shutdown())
        .await
        .expect("shutdown must complete promptly");
    assert!(
        most > 2,
        "{SUBDIRECTORIES} cold lookups on 2 workers had at most {most} listing builds under way at \
         once"
    );
}

/// What `work` yields, and the most listing builds `fs` had under way at once while it ran,
/// sampled every millisecond.
async fn with_most_listing_builds<T>(
    fs: &FuseFilesystem,
    work: impl Future<Output = T>,
) -> (T, usize) {
    let mut work = std::pin::pin!(work);
    let mut most = 0;
    loop {
        tokio::select! {
            done = &mut work => return (done, most),
            () = tokio::time::sleep(Duration::from_millis(1)) => {
                most = most.max(fs.listing_builds_in_flight());
            }
        }
    }
}

/// The node of directory `name` below the root of `fs`'s State.
async fn dir_node(fs: &FuseFilesystem, repository: &Arc<RepositoryContext>, name: &str) -> NodeID {
    fs.state()
        .find_subnode(
            repository.clone(),
            lore_revision::node::ROOT_NODE,
            lore_storage::hash::hash_string(name),
        )
        .await
        .expect("a directory of the tree")
}

/// How long the held-LOOKUP test waits on each step before calling it stuck.
const HELD_WAIT: Duration = Duration::from_secs(60);

/// Sends through a mount's notifier once it has one. Tells `entered` before it sends the
/// `target` entry's invalidation, and records whether that send returned only after `released`.
struct SignallingSink {
    notifier: std::sync::OnceLock<Notifier>,
    target: std::sync::OnceLock<(u64, &'static [u8])>,
    entered: std::sync::mpsc::Sender<()>,
    released: AtomicBool,
    returned_after_release: AtomicBool,
}

impl NotificationSink for SignallingSink {
    fn inval_inode(&self, ino: u64) {
        if let Some(notifier) = self.notifier.get() {
            let _ = notifier.inval_inode(ino, 0, 0);
        }
    }

    fn inval_entry(&self, parent: u64, name: &[u8]) {
        let target = self.target.get() == Some(&(parent, name));
        if target {
            let _ = self.entered.send(());
        }
        if let Some(notifier) = self.notifier.get() {
            let _ = notifier.inval_entry(parent, name, false);
        }
        if target {
            self.returned_after_release
                .store(self.released.load(Ordering::SeqCst), Ordering::SeqCst);
        }
    }
}

/// The `/sys/fs/fuse/connections` id of the `fuse.lore` mount at `path`: the minor device number
/// `/proc/self/mountinfo` gives it.
fn fuse_connection_id(path: &Path) -> Option<u32> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let target = path.to_str()?;
    mountinfo.lines().find_map(|line| {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let dash = tokens.iter().position(|&token| token == "-")?;
        (tokens.get(4) == Some(&target) && tokens.get(dash + 1) == Some(&"fuse.lore"))
            .then(|| tokens.get(2)?.split(':').nth(1)?.parse().ok())?
    })
}

/// A swap whose entry invalidation reaches a directory a LOOKUP holds, the LOOKUP's answer held
/// in the daemon until the swap sends, completes once that LOOKUP is answered. The kernel takes
/// the directory's lock for the invalidation, which the LOOKUP holds until it is answered, and
/// the answer runs on the one runtime worker the swap shares with the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_swap_invalidating_an_entry_beside_a_held_lookup_completes() {
    skip_without_fuse!();
    let (immutable, mutable, execution) =
        fuse_fixture::test_store_create().await.expect("test store");
    let (instance, first, second) = lore_base::runtime::LORE_CONTEXT
        .scope(execution.clone(), async {
            let instance = fuse_fixture::test_repository_create(
                immutable,
                mutable,
                RepositoryId::from(uuid::Uuid::now_v7()),
            )
            .await;
            std::fs::create_dir_all(instance.path.join("d00")).expect("d00");
            for name in ["gone", "held"] {
                fuse_fixture::test_file_write(&instance.path.join("d00").join(name), b"file");
            }
            let first = fuse_fixture::test_commit_tree(&instance, "with gone").await;
            // A fixture edit, outside any repository write.
            #[allow(clippy::disallowed_methods)]
            std::fs::remove_file(instance.path.join("d00/gone")).expect("remove gone");
            let second = fuse_fixture::test_commit_tree(&instance, "without gone").await;
            (instance, first, second)
        })
        .await;
    let repository = instance.repository.clone();
    let fs = lore_base::runtime::LORE_CONTEXT
        .scope(
            execution.clone(),
            FuseFilesystem::new(repository.clone(), first.clone(), attr_context()),
        )
        .await
        .expect("provider");
    let d00 = dir_node(&fs, &repository, "d00").await;
    let holding = Arc::new(AtomicBool::new(false));
    let release = Arc::new(tokio::sync::Notify::new());
    let (held, held_now) = std::sync::mpsc::channel();
    {
        let (holding, release) = (holding.clone(), release.clone());
        fs.set_listing_hook(Arc::new(
            move |node| -> lore_revision::BoxFuture<'static, ()> {
                if node != d00 || !holding.swap(false, Ordering::SeqCst) {
                    return Box::pin(async {});
                }
                let _ = held.send(());
                let release = release.clone();
                Box::pin(async move { release.notified().await })
            },
        ));
    }
    let (entered, entered_now) = std::sync::mpsc::channel();
    let sink = Arc::new(SignallingSink {
        notifier: std::sync::OnceLock::new(),
        target: std::sync::OnceLock::new(),
        entered,
        released: AtomicBool::new(false),
        returned_after_release: AtomicBool::new(false),
    });
    fs.set_notifications(sink.clone())
        .expect("the notification thread");
    let mounted = mount(fs.clone(), 2).await;
    let _ = sink
        .notifier
        .set(fs.notifier().expect("the mount's notifier").clone());
    // A deadlock is ended by aborting the connection; without that, the test would hang.
    let Some(connection) = fuse_connection_id(&mounted.root)
        .filter(|id| Path::new(&format!("/sys/fs/fuse/connections/{id}/abort")).exists())
    else {
        // Past libtest's capture, so a skip shows in a passing run.
        let _ = std::io::stderr().write_all(
            b"SKIPPED a_swap_invalidating_an_entry_beside_a_held_lookup_completes: no fusectl abort \
              file for the mount\n",
        );
        return;
    };
    // Looked up, so the swap that deletes it invalidates its entry in d00.
    std::fs::symlink_metadata(mounted.root.join("d00/gone")).expect("stat of gone");
    let d00_ino = fs
        .inodes()
        .find_child(ROOT_INO, hash_string("d00"), "d00")
        .expect("d00's inode");
    let _ = sink.target.set((d00_ino, b"gone"));
    fs.served().listings.remove(d00);
    holding.store(true, Ordering::SeqCst);
    let held_path = mounted.root.join("d00/held");
    let lookup = std::thread::spawn(move || std::fs::symlink_metadata(held_path).map(|_| ()));
    held_now
        .recv_timeout(HELD_WAIT)
        .expect("the LOOKUP of d00/held waits in the listing hook");

    let (swapped, swapped_now) = std::sync::mpsc::channel();
    // The task carries its own execution scope.
    #[allow(clippy::disallowed_methods)]
    let _swap = {
        let fs = fs.clone();
        tokio::spawn(
            lore_base::runtime::LORE_CONTEXT.scope(execution.clone(), async move {
                let operation = InstanceOperationImpl::new(
                    StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
                    true,
                );
                let result = swap_mounted(&fs, &operation, repository, second).await;
                let _ = swapped.send(result.is_ok());
            }),
        )
    };
    let sent = entered_now.recv_timeout(HELD_WAIT);
    sink.released.store(true, Ordering::SeqCst);
    release.notify_one();
    let swap = swapped_now.recv_timeout(HELD_WAIT);
    if swap.is_err() {
        // Ends the held LOOKUP, so the kernel lets the invalidation through and the suite moves on.
        // The kernel's control file for the test's own mount, outside any repository.
        #[allow(clippy::disallowed_methods)]
        std::fs::write(format!("/sys/fs/fuse/connections/{connection}/abort"), b"1")
            .expect("aborting the mount's connection");
        let _ = swapped_now.recv_timeout(HELD_WAIT);
    }
    let looked_up = lookup.join().expect("the LOOKUP thread");
    sent.expect("the swap sends gone's entry invalidation in d00 through the mount's thread");
    assert_eq!(
        swap,
        Ok(true),
        "the swap's invalidation waited on the held LOOKUP, whose answer waited on the runtime \
         worker the invalidation blocked"
    );
    assert!(
        sink.returned_after_release.load(Ordering::SeqCst),
        "gone's entry invalidation waited for the held LOOKUP"
    );
    looked_up.expect("the held LOOKUP is answered");
    tokio::time::timeout(Duration::from_secs(5), mounted.session.shutdown())
        .await
        .expect("shutdown must complete promptly");
}
