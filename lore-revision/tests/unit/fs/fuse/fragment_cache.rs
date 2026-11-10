// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use crossbeam_epoch as epoch;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::FragmentReference;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_base::types::typed_bytes::TypedBytes as _;
use lore_revision::fs::fuse::fragment_cache::Cached;
use lore_revision::fs::fuse::fragment_cache::DEFAULT_FRAGMENT_CACHE;
use lore_revision::fs::fuse::fragment_cache::FragmentCache;
use lore_revision::fs::fuse::fragment_cache::FragmentCacheStrategy;
use lore_revision::fs::fuse::fragment_cache::LeafPolicy;
use lore_revision::fs::fuse::fragment_cache::SLOT_OVERHEAD;
use lore_revision::fs::fuse::fragment_cache::copy_range;
use lore_revision::fs::fuse::fragment_cache::is_list;
use lore_revision::fs::fuse::fragment_cache::read_options;
use lore_revision::repository::RepositoryContext;
use lore_storage::FlushOptions;
use lore_storage::FragmentFlags;
use lore_storage::ImmutableStore;
use lore_storage::ReadOptions;
use lore_storage::StorageError;
use lore_storage::WriteOptions;

mod prefetch;

const LEN: usize = 3 << 20;

pub(super) fn execution() -> Arc<lore_revision::interface::ExecutionContext> {
    Arc::new(
        lore_revision::interface::ExecutionContext::new_client_with_user_id(
            lore_revision::interface::LoreGlobalArgs::default(),
            lore_revision::relay::EventDispatcher::no_dispatch(),
            "test-user".to_string(),
        ),
    )
}

async fn repository() -> Arc<RepositoryContext> {
    let (immutable, mutable) = stores().await;
    Arc::new(RepositoryContext::new_null_context(immutable, mutable))
}

pub(super) async fn stores() -> (Arc<dyn ImmutableStore>, Arc<dyn lore_storage::MutableStore>) {
    let immutable = lore_storage::local::immutable_store::create(
        None::<&str>,
        lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
        false,
        lore_storage::ImmutableStoreSettings::default(),
    )
    .await
    .expect("in-memory immutable store");
    let mutable = lore_storage::local::mutable_store::create(
        None::<&str>,
        lore_storage::MutableStoreSettings::default(),
        immutable.clone(),
    )
    .await
    .expect("in-memory mutable store");
    (immutable, mutable)
}

pub(super) fn random(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// Four bits of entropy a byte: zstd halves it.
pub(super) fn compressible(len: usize, seed: u64) -> Vec<u8> {
    random(len, seed)
        .into_iter()
        .map(|byte| b'a' + (byte & 0x0f))
        .collect()
}

pub(super) async fn write(repository: &Arc<RepositoryContext>, content: &[u8]) -> Address {
    lore_revision::immutable::write(
        repository.clone(),
        Context::default(),
        Bytes::copy_from_slice(content),
        WriteOptions::default(),
    )
    .await
    .expect("writes")
}

fn strategies() -> Vec<FragmentCacheStrategy> {
    vec![
        FragmentCacheStrategy::Storage,
        FragmentCacheStrategy::PerInode(LeafPolicy::Load),
        FragmentCacheStrategy::PerInode(LeafPolicy::Decoded(1)),
        FragmentCacheStrategy::PerInode(LeafPolicy::Decoded(4)),
        FragmentCacheStrategy::PerInode(LeafPolicy::Stored(1)),
        FragmentCacheStrategy::Global {
            budget: 1 << 20,
            decoded: true,
        },
        FragmentCacheStrategy::Global {
            budget: 1 << 20,
            decoded: false,
        },
    ]
}

pub(super) async fn read(
    cache: &FragmentCache,
    repository: &Arc<RepositoryContext>,
    ino: u64,
    address: Address,
    size: usize,
    offset: usize,
    len: usize,
) -> Vec<u8> {
    let len = len.min(size - offset);
    let mut out = vec![0u8; len];
    cache
        .read(
            repository,
            ino,
            address,
            size as u64,
            offset as u64,
            &mut out,
            false,
        )
        .await
        .unwrap_or_else(|err| panic!("read {offset}+{len}: {err}"));
    out
}

fn reads() -> Vec<(usize, usize)> {
    vec![
        (0, 4096),
        (0, 128 << 10),
        (128 << 10, 128 << 10),
        (256 << 10, 1 << 20),
        (LEN - 4096, 4096),
        (LEN - 100, 1 << 20),
        (1_234_567, 131_072),
        (77, 1),
        (1_234_567, 131_072),
    ]
}

#[tokio::test]
async fn a_read_fetches_at_priority_and_only_when_allowed() {
    let repository = repository().await;
    let fetching = read_options(&repository, true);
    assert!(fetching.priority && fetching.remote);
    assert!(!read_options(&repository, false).remote);
    assert!(read_options(&repository, false).never_wait && !fetching.never_wait);
}

#[tokio::test]
async fn every_strategy_reads_back_the_content() {
    LORE_CONTEXT
        .scope(execution(), async {
            let repository = repository().await;
            for content in [random(LEN, 11), compressible(LEN, 12)] {
                let address = write(&repository, &content).await;
                for strategy in strategies() {
                    let cache = FragmentCache::new(strategy);
                    for (offset, len) in reads() {
                        let got = read(&cache, &repository, 7, address, LEN, offset, len).await;
                        assert!(
                            got == content[offset..offset + got.len()],
                            "{strategy:?} at {offset}+{len}"
                        );
                    }
                }
            }
        })
        .await;
}

#[tokio::test]
async fn a_two_level_file_reads_back_across_list_boundaries() {
    LORE_CONTEXT
        .scope(execution(), async {
            let repository = repository().await;
            // 64-byte leaves: 16384 references span three sub-lists of 6553.
            let len = 1 << 20;
            let content = random(len, 61);
            let address = lore_revision::immutable::write(
                repository.clone(),
                Context::default(),
                Bytes::copy_from_slice(&content),
                WriteOptions::default().with_fixed_size_chunk(64),
            )
            .await
            .expect("writes");
            let boundary = 6553 * 64;
            for strategy in strategies() {
                let cache = FragmentCache::new(strategy);
                for (offset, n) in [
                    (0, 4096),
                    (boundary - 100, 300),
                    (boundary * 2 - 1, 2),
                    (0, len),
                    (len - 10, 10),
                ] {
                    let got = read(&cache, &repository, 7, address, len, offset, n).await;
                    assert!(
                        got == content[offset..offset + n],
                        "{strategy:?} at {offset}+{n}"
                    );
                }
            }
        })
        .await;
}

#[tokio::test]
async fn a_single_fragment_file_holds_no_state() {
    LORE_CONTEXT
        .scope(execution(), async {
            let repository = repository().await;
            let content = random(100_000, 5);
            let address = write(&repository, &content).await;
            for strategy in strategies() {
                let cache = FragmentCache::new(strategy);
                let got = read(&cache, &repository, 9, address, 100_000, 10, 50_000).await;
                assert!(got == content[10..50_010], "{strategy:?}");
                assert_eq!(cache.inodes_held(), 0, "{strategy:?}");
                assert_eq!(cache.held_bytes(), 0, "{strategy:?}");
            }
        })
        .await;
}

#[tokio::test]
async fn forget_drops_the_inode_state() {
    LORE_CONTEXT
        .scope(execution(), async {
            let repository = repository().await;
            let content = compressible(LEN, 3);
            let address = write(&repository, &content).await;
            let cache = FragmentCache::new(FragmentCacheStrategy::PerInode(LeafPolicy::Decoded(4)));
            read(&cache, &repository, 7, address, LEN, 0, 128 << 10).await;
            read(&cache, &repository, 8, address, LEN, 0, 128 << 10).await;
            assert_eq!(cache.inodes_held(), 2);
            let both = cache.held_bytes();
            assert!(both > 0);

            cache.forget(7);
            assert_eq!(cache.inodes_held(), 1);
            assert_eq!(
                cache.held_bytes(),
                both / 2,
                "the forgotten inode's bytes are gone"
            );
            cache.forget(8);
            assert_eq!(cache.held_bytes(), 0);
            assert!(cache.peak_bytes() >= both);
        })
        .await;
}

#[tokio::test]
async fn new_content_under_the_same_inode_is_not_served_stale() {
    LORE_CONTEXT
        .scope(execution(), async {
            let repository = repository().await;
            let first = random(LEN, 21);
            let second = random(LEN, 22);
            let first_address = write(&repository, &first).await;
            let second_address = write(&repository, &second).await;
            for strategy in strategies() {
                let cache = FragmentCache::new(strategy);
                let got = read(&cache, &repository, 7, first_address, LEN, 1000, 4096).await;
                assert!(got == first[1000..5096], "{strategy:?}");
                let got = read(&cache, &repository, 7, second_address, LEN, 1000, 4096).await;
                assert!(
                    got == second[1000..5096],
                    "{strategy:?} served the old content"
                );
                let alone = FragmentCache::new(strategy);
                read(&alone, &repository, 7, second_address, LEN, 1000, 4096).await;
                if matches!(strategy, FragmentCacheStrategy::PerInode(_)) {
                    assert_eq!(
                        cache.held_bytes(),
                        alone.held_bytes(),
                        "{strategy:?} kept the replaced content's bytes"
                    );
                }
            }
        })
        .await;
}

#[tokio::test]
async fn per_inode_state_stays_within_its_budget() {
    LORE_CONTEXT
        .scope(execution(), async {
            let repository = repository().await;
            let content = random(LEN, 31);
            let address = write(&repository, &content).await;
            let budget = 400 << 10;
            let cache = FragmentCache::new(FragmentCacheStrategy::PerInode(LeafPolicy::Decoded(1)))
                .with_inode_budget(budget);
            for ino in 10..20 {
                let got = read(&cache, &repository, ino, address, LEN, 0, 128 << 10).await;
                assert!(got == content[..128 << 10]);
                assert!(cache.held_bytes() <= budget, "{} held", cache.held_bytes());
            }
            assert!(cache.inodes_held() >= 1);
            assert!(cache.inodes_held() < 10, "older inodes were evicted");
            let got = read(&cache, &repository, 19, address, LEN, 4096, 4096).await;
            assert!(got == content[4096..8192]);
        })
        .await;
}

#[tokio::test]
async fn the_global_cache_stays_within_its_budget() {
    LORE_CONTEXT
        .scope(execution(), async {
            let repository = repository().await;
            let content = compressible(LEN, 41);
            let address = write(&repository, &content).await;
            let budget = 1 << 20;
            for decoded in [true, false] {
                let cache = FragmentCache::new(FragmentCacheStrategy::Global { budget, decoded });
                let mut offset = 0;
                while offset < LEN {
                    let got = read(&cache, &repository, 7, address, LEN, offset, 128 << 10).await;
                    assert!(got == content[offset..offset + got.len()]);
                    assert!(cache.held_bytes() <= budget, "{} held", cache.held_bytes());
                    offset += 128 << 10;
                }
                assert!(cache.held_bytes() > 0);
                assert!(cache.peak_bytes() <= budget);
            }
        })
        .await;
}

#[tokio::test]
async fn switching_strategy_drops_everything_held() {
    LORE_CONTEXT
        .scope(execution(), async {
            let repository = repository().await;
            let content = random(LEN, 51);
            let address = write(&repository, &content).await;
            let cache = FragmentCache::new(FragmentCacheStrategy::PerInode(LeafPolicy::Stored(1)));
            read(&cache, &repository, 7, address, LEN, 0, 128 << 10).await;
            assert!(cache.held_bytes() > 0);
            cache.set_strategy(FragmentCacheStrategy::Storage);
            assert_eq!(cache.held_bytes(), 0);
            assert_eq!(cache.inodes_held(), 0);
        })
        .await;
}

/// Delegates to an in-memory store, counting what each READ asks of it.
struct TestStore {
    inner: Arc<dyn ImmutableStore>,
    /// How long each get holds before answering, so overlapping ones show.
    hold: Option<std::time::Duration>,
    log: Arc<StoreLog>,
}

/// Store operations a READ made: lookups, payloads served and their bytes, and how many were
/// in flight at once.
#[derive(Default)]
pub(super) struct Operations {
    lookups: AtomicUsize,
    payloads: AtomicUsize,
    compressed: AtomicUsize,
    bytes: AtomicUsize,
    in_flight: AtomicUsize,
    most_in_flight: AtomicUsize,
}

impl Operations {
    fn served(&self, fragment: &Fragment) {
        self.payloads.fetch_add(1, Ordering::SeqCst);
        self.bytes
            .fetch_add(fragment.size_payload as usize, Ordering::SeqCst);
        if fragment.flags & FragmentFlags::PayloadCompressed != 0 {
            self.compressed.fetch_add(1, Ordering::SeqCst);
        }
    }

    pub(super) fn snapshot(&self) -> [usize; 5] {
        [
            &self.lookups,
            &self.payloads,
            &self.compressed,
            &self.bytes,
            &self.most_in_flight,
        ]
        .map(|counter| counter.swap(0, Ordering::SeqCst))
    }
}

/// One store call in flight until dropped.
struct InFlight<'a>(&'a Operations);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl TestStore {
    /// Counts a lookup and holds it as configured, in flight until the answer drops.
    async fn enter(&self) -> InFlight<'_> {
        let operations = &self.log.operations;
        operations.lookups.fetch_add(1, Ordering::SeqCst);
        let now = operations.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        operations.most_in_flight.fetch_max(now, Ordering::SeqCst);
        if let Some(hold) = self.hold {
            tokio::time::sleep(hold).await;
        }
        InFlight(operations)
    }
}

/// Holds every get while closed, counting those it holds.
#[derive(Default)]
pub(super) struct Gate {
    closed: AtomicBool,
    /// Whether gets arriving while closed pass, those already held staying held.
    passing: AtomicBool,
    held: AtomicUsize,
    opened: tokio::sync::Notify,
}

impl Gate {
    pub(super) fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    /// Lets gets arriving from now on pass, holding those already waiting until [`Self::open`].
    pub(super) fn pass_new(&self) {
        self.passing.store(true, Ordering::SeqCst);
    }

    pub(super) fn open(&self) {
        self.passing.store(false, Ordering::SeqCst);
        self.closed.store(false, Ordering::SeqCst);
        self.opened.notify_waiters();
    }

    /// Gets held now.
    pub(super) fn held(&self) -> usize {
        self.held.load(Ordering::SeqCst)
    }

    async fn pass(&self) {
        if !self.closed.load(Ordering::SeqCst) || self.passing.load(Ordering::SeqCst) {
            return;
        }
        self.held.fetch_add(1, Ordering::SeqCst);
        loop {
            let opened = self.opened.notified();
            tokio::pin!(opened);
            opened.as_mut().enable();
            if !self.closed.load(Ordering::SeqCst) {
                break;
            }
            opened.await;
        }
        self.held.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
pub(super) struct StoreLog {
    pub(super) gate: Gate,
    list_gets: AtomicUsize,
    leaf_gets: AtomicUsize,
    pub(super) operations: Operations,
}

impl StoreLog {
    pub(super) fn loads(&self) -> (usize, usize) {
        (
            self.list_gets.load(Ordering::SeqCst),
            self.leaf_gets.load(Ordering::SeqCst),
        )
    }

    pub(super) fn reset(&self) {
        self.list_gets.store(0, Ordering::SeqCst);
        self.leaf_gets.store(0, Ordering::SeqCst);
    }
}

/// A store counting every operation, holding each get for `hold`.
pub(super) fn counting_store(
    inner: Arc<dyn ImmutableStore>,
    hold: Option<std::time::Duration>,
) -> (Arc<dyn ImmutableStore>, Arc<StoreLog>) {
    let log = Arc::new(StoreLog::default());
    let store = Arc::new(TestStore {
        inner,
        hold,
        log: log.clone(),
    });
    (store, log)
}

#[async_trait::async_trait]
impl ImmutableStore for TestStore {
    /// Joins the claim of the store this one wraps.
    async fn hold_for_command(
        self: Arc<Self>,
    ) -> Result<Option<lore_storage::local::store_lock::StoreGuard>, lore_storage::StoreError> {
        self.inner.clone().hold_for_command().await
    }

    fn is_local(&self) -> bool {
        self.inner.is_local()
    }

    fn isolates_partitions(&self) -> bool {
        self.inner.isolates_partitions()
    }

    fn read_scope(&self) -> lore_storage::StoreMatch {
        self.inner.read_scope()
    }

    fn query_scope(&self) -> lore_storage::StoreMatch {
        self.inner.query_scope()
    }

    async fn query(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        results: &mut [lore_storage::StoreMatchResult],
    ) -> Result<(), lore_storage::StoreError> {
        self.inner
            .clone()
            .query(partition, addresses, results)
            .await
    }

    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<lore_storage::StoreGetData, lore_storage::StoreError> {
        self.log.gate.pass().await;
        let _in_flight = self.enter().await;
        let data = self.inner.clone().get(partition, address).await?;
        self.log.operations.served(&data.fragment);
        let counter = if is_list(&data.fragment) {
            &self.log.list_gets
        } else {
            &self.log.leaf_gets
        };
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(data)
    }

    async fn get_into(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        dst: &mut lore_storage::CallerBuffer,
    ) -> Result<(Fragment, lore_storage::PayloadRead), lore_storage::StoreError> {
        let _in_flight = self.enter().await;
        let (fragment, read) = self.inner.clone().get_into(partition, address, dst).await?;
        self.log.operations.served(&fragment);
        Ok((fragment, read))
    }

    async fn get_metadata(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<lore_storage::StoreGetData, lore_storage::StoreError> {
        self.inner.clone().get_metadata(partition, address).await
    }

    async fn put(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
        force: bool,
    ) -> Result<(), lore_storage::StoreError> {
        self.inner
            .clone()
            .put(partition, address, fragment, payload, force)
            .await
    }

    async fn obliterate(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        stats: Arc<lore_storage::StoreObliterateStats>,
    ) -> Result<(), lore_storage::StoreError> {
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
    ) -> Result<usize, lore_storage::StoreError> {
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
    ) -> Result<Option<usize>, lore_storage::StoreError> {
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

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), lore_storage::StoreError> {
        self.inner.clone().flush(options).await
    }

    async fn verify(self: Arc<Self>, heal: bool) -> Result<(), lore_storage::StoreError> {
        self.inner.clone().verify(heal).await
    }

    async fn copy(
        self: Arc<Self>,
        source_partition: Partition,
        source_address: Address,
        destination_partition: Partition,
        destination_context: Context,
        behavior: lore_storage::immutable_store::CopyBehavior,
    ) -> Result<(), lore_storage::StoreError> {
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

async fn test_repository() -> (Arc<RepositoryContext>, Arc<StoreLog>) {
    let (inner, mutable) = stores().await;
    let (store, log) = counting_store(inner, None);
    (
        Arc::new(RepositoryContext::new_null_context(store, mutable)),
        log,
    )
}

/// A strategy keeping leaves serves a second READ inside a leaf from memory, whether the leaf
/// was stored compressed or not.
#[tokio::test]
async fn a_second_read_inside_a_cached_list_or_leaf_loads_nothing() {
    LORE_CONTEXT
        .scope(execution(), async {
            let (repository, log) = test_repository().await;
            for content in [compressible(LEN, 91), random(LEN, 93)] {
                let address = write(&repository, &content).await;
                for strategy in strategies() {
                    if strategy == FragmentCacheStrategy::Storage {
                        continue;
                    }
                    let cache = FragmentCache::new(strategy);
                    read(&cache, &repository, 7, address, LEN, 0, 4096).await;
                    log.reset();
                    let got = read(&cache, &repository, 7, address, LEN, 100, 100).await;
                    assert!(got == content[100..200], "{strategy:?}");
                    let (lists, leaves) = log.loads();
                    assert_eq!(lists, 0, "{strategy:?} reloaded a list");
                    let keeps_leaves = strategy.leaves_kept() > 0
                        || matches!(strategy, FragmentCacheStrategy::Global { .. });
                    if keeps_leaves {
                        assert_eq!(leaves, 0, "{strategy:?} reloaded a leaf");
                    }
                }
            }
        })
        .await;
}

/// A range of a compressed leaf is copied out of the whole leaf, decoded.
#[test]
fn a_compressed_leaf_range_is_copied_from_the_decoded_leaf() {
    let plain: Vec<u8> = (0..64usize << 10).map(|index| (index / 64) as u8).collect();
    let raw = Fragment {
        flags: 0,
        size_payload: plain.len() as u32,
        size_content: plain.len() as u64,
    };
    let (fragment, stored) =
        lore_storage::compress::compress(raw, &plain, lore_storage::CompressionMode::Zstd)
            .expect("compresses");
    assert_ne!(fragment.flags & FragmentFlags::PayloadCompressed, 0);
    let mut dst = [0u8; 100];
    copy_range(fragment, &stored, 1000, &mut dst).expect("decodes");
    assert_eq!(dst[..], plain[1000..1100]);
}

/// A compressed leaf the build has no codec for answers the decoder's `NotSupported`.
#[test]
fn a_leaf_the_codec_cannot_decode_is_not_supported() {
    let fragment = Fragment {
        flags: FragmentFlags::PayloadCompressedOodle2.as_u32(),
        size_payload: 64,
        size_content: 128,
    };
    let mut dst = [0u8; 16];
    let err = copy_range(fragment, &[0u8; 64], 0, &mut dst).expect_err("nothing decodes it");
    assert!(matches!(err, StorageError::NotSupported(_)), "{err:?}");
}

/// `payload` stored under the address `content` would have, as local corruption leaves it.
async fn put_mismatched(
    repository: &Arc<RepositoryContext>,
    address: Address,
    stored_content: &[u8],
) {
    let raw = Fragment {
        flags: 0,
        size_payload: stored_content.len() as u32,
        size_content: stored_content.len() as u64,
    };
    let (fragment, payload) =
        lore_storage::compress::compress(raw, stored_content, lore_storage::CompressionMode::Zstd)
            .expect("compresses");
    repository
        .immutable_store()
        .put(repository.id, address, fragment, Some(payload), true)
        .await
        .expect("put");
}

/// A payload owner that counts its drops.
struct Owner(Vec<u8>, Arc<AtomicUsize>);

impl AsRef<[u8]> for Owner {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.1.fetch_add(1, Ordering::SeqCst);
    }
}

fn entry(seed: u64, payload: Bytes, list: bool) -> Cached {
    let len = payload.len();
    Cached {
        hash: Hash::from_u64(seed),
        fragment: Fragment {
            flags: if list {
                FragmentFlags::PayloadFragmented.as_u32()
            } else {
                0
            },
            size_payload: len as u32,
            size_content: len as u64,
        },
        payload,
    }
}

/// Keeps `cached` for `ino` as a READ of content `root` would.
fn keep_for(cache: &FragmentCache, ino: u64, root: u64, depth: usize, cached: Cached) {
    let slot = Arc::clone(&cache.slot(ino, Hash::from_u64(root)));
    cache.keep(ino, Some(&slot), depth, cached, cache.strategy());
}

/// A racing insert left state for another root in place by the time the shard's write lock
/// is taken: it is replaced, not handed out, and its bytes are counted out.
#[test]
fn a_racing_insert_for_another_root_is_replaced() {
    let cache = FragmentCache::new(FragmentCacheStrategy::PerInode(LeafPolicy::Decoded(4)));
    keep_for(&cache, 7, 1, 0, entry(1, Bytes::from(vec![0u8; 64]), true));
    let slot = cache.slot_entry(7, Hash::from_u64(2));
    assert_eq!(slot.root, Hash::from_u64(2));
    drop(slot);
    assert_eq!(cache.inodes_held(), 1);
    assert_eq!(cache.inode_held.load(Ordering::SeqCst), SLOT_OVERHEAD);
    let reused = cache.slot_entry(7, Hash::from_u64(2));
    assert_eq!(reused.root, Hash::from_u64(2));
    drop(reused);
    assert_eq!(cache.inode_held.load(Ordering::SeqCst), SLOT_OVERHEAD);
}

#[test]
fn displaced_large_payloads_are_freed_promptly() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let cache = FragmentCache::new(FragmentCacheStrategy::PerInode(LeafPolicy::Decoded(4)));
    for seed in 0..8 {
        let owner = Owner(vec![0; 128 << 10], dropped.clone());
        keep_for(
            &cache,
            7,
            1,
            1,
            entry(100 + seed, Bytes::from_owner(owner), false),
        );
    }
    cache.forget(7);
    assert_eq!(cache.held_bytes(), 0);

    let collector = std::thread::spawn({
        let dropped = dropped.clone();
        move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while dropped.load(Ordering::SeqCst) < 8 && std::time::Instant::now() < deadline {
                epoch::pin().flush();
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    });
    collector.join().expect("collector");
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        8,
        "every displaced payload is freed once no reader can see it, not held in this \
         thread's deferred bag"
    );
}

/// Per-inode state of `inodes` inodes, each holding one list of `len` bytes, installed in turn
/// under a budget that fits `fits` of them; answers slots visited per eviction.
fn visits_per_eviction(inodes: u64, fits: usize, len: usize) -> f64 {
    let budget = fits * (len + SLOT_OVERHEAD);
    let cache = FragmentCache::new(FragmentCacheStrategy::PerInode(LeafPolicy::Load))
        .with_inode_budget(budget);
    let list = Bytes::from(vec![0u8; len]);
    for ino in 0..inodes {
        keep_for(&cache, ino, ino, 0, entry(ino, list.clone(), true));
        assert!(cache.inode_held.load(Ordering::SeqCst) <= budget);
    }
    let evictions = cache.evictions.load(Ordering::SeqCst);
    assert_eq!(
        evictions as u64,
        inodes - fits as u64,
        "evicts exactly what it must"
    );
    cache.visits.load(Ordering::SeqCst) as f64 / evictions as f64
}

#[test]
fn eviction_cost_is_independent_of_the_inode_count() {
    let few = visits_per_eviction(256, 64, 1024);
    let many = visits_per_eviction(8192, 2048, 1024);
    assert!(few <= 4.0, "{few} slots visited per eviction of 64 held");
    assert!(
        many <= 4.0,
        "{many} slots visited per eviction of 2048 held"
    );
}

#[test]
fn concurrent_installs_neither_exceed_the_budget_nor_over_evict() {
    const THREADS: u64 = 8;
    const PER_THREAD: u64 = 2000;
    let len = 1024;
    let fits = 256;
    let budget = fits * (len + SLOT_OVERHEAD);
    let cache = Arc::new(
        FragmentCache::new(FragmentCacheStrategy::PerInode(LeafPolicy::Load))
            .with_inode_budget(budget),
    );
    let list = Bytes::from(vec![0u8; len]);
    let threads: Vec<_> = (0..THREADS)
        .map(|thread| {
            let cache = cache.clone();
            let list = list.clone();
            std::thread::spawn(move || {
                for i in 0..PER_THREAD {
                    let ino = thread * PER_THREAD + i;
                    keep_for(&cache, ino, ino, 0, entry(ino, list.clone(), true));
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("installer");
    }
    assert!(cache.inode_held.load(Ordering::SeqCst) <= budget);
    let evictions = cache.evictions.load(Ordering::SeqCst) as u64;
    let needed = THREADS * PER_THREAD - fits as u64;
    assert!(
        evictions <= needed + THREADS,
        "{evictions} evictions where {needed} make room"
    );
    assert!(cache.inodes_held() + THREADS as usize >= fits);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_across_compressed_leaves_loads_them_side_by_side() {
    LORE_CONTEXT
        .scope(execution(), async {
            let (inner, mutable) = stores().await;
            let (store, log) = counting_store(inner, Some(std::time::Duration::from_millis(20)));
            let repository = Arc::new(RepositoryContext::new_null_context(store, mutable));
            let content = compressible(LEN, 96);
            let address = write(&repository, &content).await;
            let cache = FragmentCache::new(DEFAULT_FRAGMENT_CACHE);
            read(&cache, &repository, 7, address, LEN, 0, 4096).await;
            log.operations.snapshot();
            let got = read(&cache, &repository, 7, address, LEN, 1 << 20, 1 << 20).await;
            assert!(got == content[1 << 20..2 << 20]);
            let [_, payloads, compressed, _, most_in_flight] = log.operations.snapshot();
            assert!(
                compressed >= 3,
                "{compressed} of {payloads} leaves compressed"
            );
            assert!(
                most_in_flight >= 2,
                "the READ's {compressed} compressed leaves loaded one after another"
            );
        })
        .await;
}

/// Per-READ store operations of S0 against the default on compressible content in an on-disk
/// store: 128 KiB READs in order and scattered over a `LORE_COUNT_MIB` MiB file (default 64).
/// Every compressed payload served is decoded once, whole, by either strategy. Tasks are the
/// shared runtime's spawns, so run this alone.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "diagnostic"]
async fn per_read_operations_of_s0_and_the_default() {
    LORE_CONTEXT
        .scope(execution(), async {
            let dir = lore_base::test_util::TempDir::new("fragment-cache-counts-");
            let inner = lore_storage::local::immutable_store::create(
                Some(dir.path()),
                lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
                false,
                lore_storage::ImmutableStoreSettings::default(),
            )
            .await
            .expect("on-disk store");
            let mutable = lore_storage::local::mutable_store::create(
                None::<&str>,
                lore_storage::MutableStoreSettings::default(),
                inner.clone(),
            )
            .await
            .expect("mutable store");
            let (store, log) = counting_store(inner, None);
            let repository = Arc::new(RepositoryContext::new_null_context(store, mutable));
            let mib = std::env::var("LORE_COUNT_MIB")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(64usize);
            let size = mib << 20;
            let content = compressible(size, 97);
            let address = write(&repository, &content).await;
            repository
                .immutable_store()
                .flush(FlushOptions::SYNC_DATA)
                .await
                .expect("flush");
            let len = 128 << 10;
            let reads = 256usize;
            let mut x = 0x9e37_79b9_7f4a_7c15u64;
            let scattered: Vec<usize> = (0..reads)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    (x as usize % (size / len)) * len
                })
                .collect();
            let in_order: Vec<usize> = (0..reads).map(|i| (i * len) % size).collect();
            println!(
                "| strategy | pattern | lookups | payloads | decodes | bytes | tasks | \
                 most in flight |"
            );
            println!("|---|---|---|---|---|---|---|---|");
            for (label, strategy) in [
                ("S0", FragmentCacheStrategy::Storage),
                ("default", DEFAULT_FRAGMENT_CACHE),
            ] {
                for (pattern, offsets) in [("in order", &in_order), ("scattered", &scattered)] {
                    let cache = FragmentCache::new(strategy);
                    read(&cache, &repository, 7, address, size, 0, 4096).await;
                    log.operations.snapshot();
                    let metrics = lore_base::runtime::runtime().metrics();
                    let spawned = metrics.spawned_tasks_count();
                    for &offset in offsets.iter() {
                        let got = read(&cache, &repository, 7, address, size, offset, len).await;
                        assert!(got == content[offset..offset + len]);
                    }
                    let tasks = metrics.spawned_tasks_count() - spawned;
                    let [lookups, payloads, decodes, bytes, most] = log.operations.snapshot();
                    let per = |count: usize| count as f64 / reads as f64;
                    println!(
                        "| {label} | {pattern} | {:.2} | {:.2} | {:.2} | {:.0} | {:.2} | {most} |",
                        per(lookups),
                        per(payloads),
                        per(decodes),
                        per(bytes),
                        per(tasks as usize),
                    );
                }
            }
        })
        .await;
}

/// Stores `refs` as the root list of `address`, content `LEN` bytes, over what it held.
async fn put_root_list(
    repository: &Arc<RepositoryContext>,
    address: Address,
    refs: &[FragmentReference],
) {
    let payload = Bytes::copy_from_slice(zerocopy::IntoBytes::as_bytes(refs));
    let fragment = Fragment {
        flags: FragmentFlags::PayloadFragmented.as_u32(),
        size_payload: payload.len() as u32,
        size_content: LEN as u64,
    };
    repository
        .immutable_store()
        .put(repository.id, address, fragment, Some(payload), true)
        .await
        .expect("put");
}

/// Content-defined chunking splits files well under [`FRAGMENT_SIZE_THRESHOLD`], so a small
/// file may be stored as a fragment list.
#[tokio::test]
async fn a_small_file_stored_as_a_list_reads_under_every_strategy() {
    LORE_CONTEXT
        .scope(execution(), async {
            let repository = repository().await;
            let chunks: Vec<Vec<u8>> = (0..3).map(|seed| random(30_000, 40 + seed)).collect();
            let mut refs = Vec::new();
            let mut offset = 0u64;
            for chunk in &chunks {
                let leaf = write(&repository, chunk).await;
                refs.push(FragmentReference {
                    hash: leaf.hash,
                    offset_content: offset,
                });
                offset += chunk.len() as u64;
            }
            let content = chunks.concat();
            let address = Address {
                hash: Hash::from_u64(0x5a11),
                context: Context::default(),
            };
            let payload = Bytes::copy_from_slice(zerocopy::IntoBytes::as_bytes(&refs[..]));
            let fragment = Fragment {
                flags: FragmentFlags::PayloadFragmented.as_u32(),
                size_payload: payload.len() as u32,
                size_content: content.len() as u64,
            };
            repository
                .immutable_store()
                .put(repository.id, address, fragment, Some(payload), true)
                .await
                .expect("put");

            for strategy in strategies() {
                for (offset, len) in [(0, 4096), (29_000, 4096), (0, content.len())] {
                    let cache = FragmentCache::new(strategy);
                    let mut out = vec![0u8; len];
                    cache
                        .read(
                            &repository,
                            7,
                            address,
                            content.len() as u64,
                            offset as u64,
                            &mut out,
                            false,
                        )
                        .await
                        .unwrap_or_else(|error| panic!("{strategy:?} at {offset}: {error}"));
                    assert_eq!(
                        out,
                        content[offset..offset + len],
                        "{strategy:?} at {offset}"
                    );
                }
            }
        })
        .await;
}

#[tokio::test]
async fn a_malformed_list_is_refused_wherever_the_read_lands() {
    LORE_CONTEXT
        .scope(execution(), async {
            let repository = repository().await;
            let content = random(LEN, 95);
            let address = write(&repository, &content).await;
            let (_, root) = lore_storage::load_fragment(
                repository.immutable_store(),
                repository.id,
                address,
                ReadOptions::default(),
                None,
            )
            .await
            .expect("root");
            let refs = root
                .to_aligned::<FragmentReference>()
                .as_type_slice::<FragmentReference>()
                .to_vec();
            assert!(refs.len() >= 4, "{} leaves", refs.len());

            let mut unordered = refs.clone();
            unordered[2].offset_content = unordered[1].offset_content;
            let mut zero_hash = refs.clone();
            zero_hash[1].hash = Hash::default();
            let far = LEN - 4096;
            for (label, malformed) in [("unordered", unordered), ("zero hash", zero_hash)] {
                put_root_list(&repository, address, &malformed).await;
                for (strategy, offset) in strategies()
                    .into_iter()
                    .filter(|strategy| *strategy != FragmentCacheStrategy::Storage)
                    .flat_map(|strategy| [(strategy, 0), (strategy, far)])
                {
                    let cache = FragmentCache::new(strategy);
                    let mut out = vec![0u8; 4096];
                    let read = cache
                        .read(
                            &repository,
                            7,
                            address,
                            LEN as u64,
                            offset as u64,
                            &mut out,
                            false,
                        )
                        .await;
                    assert!(
                        read.is_err(),
                        "{strategy:?} served a READ at {offset} through a {label} list"
                    );
                }
            }
        })
        .await;
}

#[tokio::test]
async fn local_fragments_are_served_unverified() {
    LORE_CONTEXT
        .scope(execution(), async {
            let repository = repository().await;
            // One fragment, whose stored bytes are not the content its address names.
            let expected = compressible(100_000, 81);
            let stored = compressible(100_000, 82);
            let single = Address {
                hash: lore_storage::hash::hash_slice(&expected),
                context: Context::default(),
            };
            put_mismatched(&repository, single, &stored).await;

            // A fragmented file one of whose leaves is likewise corrupt.
            let content = compressible(LEN, 83);
            let tree = write(&repository, &content).await;
            let (_, root) = lore_storage::load_fragment(
                repository.immutable_store(),
                repository.id,
                tree,
                ReadOptions::default().no_decompress(),
                None,
            )
            .await
            .expect("root");
            let mut list = root.to_aligned::<FragmentReference>();
            let mut span = LEN;
            loop {
                let refs = list.as_type_slice::<FragmentReference>();
                let first = Address {
                    hash: refs[0].hash,
                    context: Context::default(),
                };
                let next = refs.get(1).map_or(span, |r| r.offset_content as usize);
                let (fragment, payload) = lore_storage::load_fragment(
                    repository.immutable_store(),
                    repository.id,
                    first,
                    ReadOptions::default().no_decompress(),
                    None,
                )
                .await
                .expect("child");
                if is_list(&fragment) {
                    list = payload.to_aligned::<FragmentReference>();
                    span = next;
                    continue;
                }
                put_mismatched(&repository, first, &compressible(next, 84)).await;
                span = next;
                break;
            }

            for strategy in strategies() {
                let cache = FragmentCache::new(strategy);
                let got = read(&cache, &repository, 9, single, 100_000, 0, 100_000).await;
                assert!(got == stored, "{strategy:?}: the stored bytes, unverified");
                let got = read(&cache, &repository, 10, tree, LEN, 0, 4096).await;
                assert!(
                    got == compressible(span, 84)[..4096],
                    "{strategy:?}: the corrupt leaf, unverified"
                );
            }
        })
        .await;
}
