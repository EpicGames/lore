// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
use lore_base::types::FragmentReference;
use lore_base::types::typed_bytes::TypedBytes as _;
use lore_revision::fs::fuse::fragment_cache::DEFAULT_FRAGMENT_CACHE;
use lore_revision::fs::fuse::fragment_cache::FragmentCache;
use lore_revision::fs::fuse::fragment_cache::FragmentCacheStrategy;
use lore_revision::fs::fuse::fragment_cache::INODE_BUDGET;
use lore_revision::fs::fuse::fragment_cache::InodeSlot;
use lore_revision::fs::fuse::fragment_cache::LeafPolicy;
use lore_revision::fs::fuse::fragment_cache::prefetch::Stream;
use lore_revision::repository::RepositoryContext;
use lore_storage::FlushOptions;
use lore_storage::ReadOptions;
use lore_storage::WriteOptions;

use super::StoreLog;
use super::compressible;
use super::counting_store;
use super::execution;
use super::random;
use super::read;
use super::stores;
use super::write;

const FILE: usize = 8 << 20;
const READ: usize = 128 << 10;
const INO: u64 = 7;
/// Keeps lists alone, so what an inode holds beyond them is what prefetch holds.
const LISTS_ONLY: FragmentCacheStrategy = FragmentCacheStrategy::PerInode(LeafPolicy::Load);

/// A repository over a store counting what each READ asks of it, holding each get for `hold`.
async fn counted(hold: Option<Duration>) -> (Arc<RepositoryContext>, Arc<StoreLog>) {
    let (inner, mutable) = stores().await;
    let (store, log) = counting_store(inner, hold);
    (
        Arc::new(RepositoryContext::new_null_context(store, mutable)),
        log,
    )
}

/// Waits until `condition` holds.
async fn until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(std::time::Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// Waits until no prefetch task has work left before the next READ.
async fn settle(cache: &FragmentCache) {
    until("prefetch settles", || cache.prefetch_settled()).await;
}

/// Reads `content` in order in `len`-sized READs, letting prefetch settle before each, and
/// answers how many READs loaded anything themselves.
async fn in_order(
    cache: &FragmentCache,
    repository: &Arc<RepositoryContext>,
    address: Address,
    content: &[u8],
    len: usize,
    mut after: impl FnMut(&FragmentCache),
) -> usize {
    let mut loaded = 0;
    for offset in (0..content.len()).step_by(len) {
        settle(cache).await;
        let before = cache.prefetch_counts().read_loads;
        let got = read(cache, repository, INO, address, content.len(), offset, len).await;
        assert!(
            got == content[offset..offset + got.len()],
            "bytes at {offset}"
        );
        if cache.prefetch_counts().read_loads != before {
            loaded += 1;
        }
        after(cache);
    }
    settle(cache).await;
    loaded
}

fn prefetching() -> Arc<FragmentCache> {
    prefetching_under(DEFAULT_FRAGMENT_CACHE)
}

fn prefetching_under(strategy: FragmentCacheStrategy) -> Arc<FragmentCache> {
    let cache = FragmentCache::new(strategy).into_shared();
    cache.set_sequential_prefetch(true);
    cache
}

/// What the inode state of a [`LISTS_ONLY`] cache that never prefetched holds after reading
/// `content` in order.
async fn demand_held(
    repository: &Arc<RepositoryContext>,
    address: Address,
    content: &[u8],
    budget: usize,
) -> usize {
    let cache = FragmentCache::new(LISTS_ONLY).with_inode_budget(budget);
    in_order(&cache, repository, address, content, READ, |_| {}).await;
    cache.inode_held.load(Ordering::SeqCst)
}

/// The first leaf of a single-level file starting at or after content offset `offset`, and where
/// it starts.
async fn leaf_from(
    repository: &Arc<RepositoryContext>,
    address: Address,
    offset: u64,
) -> (Address, usize) {
    let (_, root) = lore_storage::load_fragment(
        repository.immutable_store(),
        repository.id,
        address,
        ReadOptions::default().no_decompress(),
        None,
    )
    .await
    .expect("root");
    let list = root.to_aligned::<FragmentReference>();
    let refs = list.as_type_slice::<FragmentReference>();
    let index = refs.partition_point(|r| r.offset_content < offset);
    let leaf = Address {
        hash: refs[index].hash,
        context: Context::default(),
    };
    (leaf, refs[index].offset_content as usize)
}

async fn remove(repository: &Arc<RepositoryContext>, address: Address) {
    repository
        .immutable_store()
        .obliterate(
            repository.id,
            address,
            Arc::new(lore_storage::StoreObliterateStats::default()),
        )
        .await
        .expect("obliterates");
}

/// Content written in `chunk`-byte leaves.
async fn write_chunked(
    repository: &Arc<RepositoryContext>,
    content: &[u8],
    chunk: usize,
) -> Address {
    lore_revision::immutable::write(
        repository.clone(),
        Context::default(),
        Bytes::copy_from_slice(content),
        WriteOptions::default().with_fixed_size_chunk(chunk),
    )
    .await
    .expect("writes")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sequential_read_finds_later_reads_prefetched() {
    LORE_CONTEXT
        .scope(execution(), async {
            let (repository, _log) = counted(None).await;
            for content in [random(FILE, 31), compressible(FILE, 32)] {
                let address = write(&repository, &content).await;
                let cache = prefetching();
                let loaded = in_order(&cache, &repository, address, &content, READ, |_| {}).await;
                let counts = cache.prefetch_counts();
                let reads = FILE / READ;
                assert_eq!(counts.streams, 1, "{counts:?}");
                assert!(
                    loaded <= 3,
                    "{loaded} of {reads} READs loaded themselves: {counts:?}"
                );
                assert!(counts.prefetch_loads > 0, "{counts:?}");
                assert!(counts.used >= FILE - (1 << 20), "{counts:?}");
            }
        })
        .await;
}

/// Starts a stream over `content` and lets its first batch land, then closes the store's gate and
/// reads on inside that batch until the task's next batch waits in the store. Answers where the
/// next READ starts.
async fn hold_the_next_batch(
    cache: &FragmentCache,
    repository: &Arc<RepositoryContext>,
    log: &StoreLog,
    address: Address,
    content: &[u8],
) -> usize {
    read(cache, repository, INO, address, FILE, 0, READ).await;
    read(cache, repository, INO, address, FILE, READ, READ).await;
    settle(cache).await;
    log.gate.close();
    let mut offset = 2 * READ;
    while log.gate.held() == 0 && offset < 6 * READ {
        let got = read(cache, repository, INO, address, FILE, offset, READ).await;
        assert!(got == content[offset..offset + READ], "bytes at {offset}");
        offset += READ;
    }
    until("the next batch waits in the store", || log.gate.held() > 0).await;
    offset
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_outrunning_the_task_load_no_passed_leaves() {
    LORE_CONTEXT
        .scope(execution(), async {
            let (repository, log) = counted(None).await;
            let content = random(FILE, 41);
            let address = write(&repository, &content).await;
            let cache = prefetching();
            let from = hold_the_next_batch(&cache, &repository, &log, address, &content).await;
            log.gate.pass_new();
            for offset in (from..24 * READ).step_by(READ) {
                let got = read(&cache, &repository, INO, address, FILE, offset, READ).await;
                assert!(got == content[offset..offset + READ]);
            }
            log.gate.open();
            settle(&cache).await;
            let counts = cache.prefetch_counts();
            assert_eq!(
                counts.behind, 0,
                "leaves the stream had passed loaded: {counts:?}"
            );
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retried_read_neither_extends_nor_breaks_the_stream() {
    LORE_CONTEXT
        .scope(execution(), async {
            const LONG: usize = 1 << 20;
            let (repository, _log) = counted(None).await;
            let content = random(FILE, 42);
            let address = write(&repository, &content).await;
            let cache = prefetching();
            let mut out = vec![0u8; LONG];
            for fetch in [false, true] {
                cache
                    .read(&repository, INO, address, FILE as u64, 0, &mut out, fetch)
                    .await
                    .expect("read");
                if !fetch {
                    let first = cache.stream_state(INO);
                    cache
                        .read(&repository, INO, address, FILE as u64, 0, &mut out, true)
                        .await
                        .expect("retry");
                    assert_eq!(cache.stream_state(INO), first, "the retry moved the stream");
                }
            }
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reordered_long_reads_from_two_tasks_keep_one_stream() {
    LORE_CONTEXT
        .scope(execution(), async {
            const LONG: usize = 1 << 20;
            let (repository, _log) = counted(None).await;
            let content = Arc::new(random(FILE, 43));
            let address = write(&repository, &content).await;
            let cache = prefetching();
            // Each group of four READs arrives as 0, 2, 1, 3: every other one out of order.
            let order: Vec<usize> = (0..FILE / LONG)
                .map(|i| (i & !3) + [0, 2, 1, 3][i & 3])
                .collect();
            for unit in order {
                settle(&cache).await;
                let (cache, repository, content) =
                    (cache.clone(), repository.clone(), content.clone());
                let task = lore_base::lore_spawn!(async move {
                    let offset = unit * LONG;
                    let got = read(&cache, &repository, INO, address, FILE, offset, LONG).await;
                    assert!(got == content[offset..offset + LONG], "bytes at {offset}");
                });
                // Each READ lands before the next is issued, from a task of its own, once the
                // prefetch task has caught up.
                task.await.expect("read task");
            }
            settle(&cache).await;
            let counts = cache.prefetch_counts();
            assert_eq!(counts.streams, 1, "{counts:?}");
            assert!(counts.prefetch_loads > 0, "{counts:?}");
            assert!(
                counts.used > 0,
                "later READs served from prefetch: {counts:?}"
            );
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_whose_own_load_fails_locally_starts_no_stream() {
    LORE_CONTEXT
        .scope(execution(), async {
            let (repository, _log) = counted(None).await;
            let content = random(FILE, 44);
            let address = write(&repository, &content).await;
            let (leaf, start) = leaf_from(&repository, address, READ as u64).await;
            remove(&repository, leaf).await;
            let cache = prefetching();
            read(&cache, &repository, INO, address, FILE, 0, start).await;
            let mut out = vec![0u8; READ];
            let failed = cache
                .read(
                    &repository,
                    INO,
                    address,
                    FILE as u64,
                    start as u64,
                    &mut out,
                    false,
                )
                .await;
            assert!(failed.is_err(), "the removed leaf is not local");
            settle(&cache).await;
            assert_eq!(cache.prefetch_counts().streams, 0);
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_prefetch_load_ends_the_task_and_resets_the_run() {
    LORE_CONTEXT
        .scope(execution(), async {
            let (repository, _log) = counted(None).await;
            let content = random(FILE, 45);
            let address = write(&repository, &content).await;
            // The first leaf past the second READ: the task's first batch starts with it.
            remove(
                &repository,
                leaf_from(&repository, address, 2 * READ as u64).await.0,
            )
            .await;
            let cache = prefetching();
            for offset in [0, READ] {
                read(&cache, &repository, INO, address, FILE, offset, READ).await;
            }
            assert_eq!(cache.prefetch_counts().streams, 1);
            settle(&cache).await;
            assert_eq!(
                cache.counters.running.load(Ordering::SeqCst),
                0,
                "the task ended"
            );
            let state = cache.stream_state(INO).expect("stream");
            assert_eq!((state.run, state.active), (0, false), "{state:?}");
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn at_the_end_of_the_file_the_task_ends_and_frees_its_leaves() {
    LORE_CONTEXT
        .scope(execution(), async {
            let (repository, _log) = counted(None).await;
            let content = random(FILE, 46);
            let address = write(&repository, &content).await;
            let demand = demand_held(&repository, address, &content, INODE_BUDGET).await;
            let cache = prefetching_under(LISTS_ONLY);
            in_order(&cache, &repository, address, &content, READ, |_| {}).await;
            assert_eq!(
                cache.counters.running.load(Ordering::SeqCst),
                0,
                "the task ended"
            );
            // What READs in flight at the end may still read: leaves within twice a READ of it.
            let kept = cache.ahead_bytes();
            assert!(
                kept <= 2 * READ + FRAGMENT_SIZE_THRESHOLD,
                "{kept} bytes kept"
            );
            let held = cache.inode_held.load(Ordering::SeqCst);
            assert_eq!(
                held,
                demand + cache.stream_bytes(INO) + kept,
                "leaves held uncounted"
            );
            cache.forget(INO);
            assert_eq!(cache.inode_held.load(Ordering::SeqCst), 0);
            assert_eq!(cache.ahead_bytes(), 0);
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_break_frees_the_prefetched_leaves() {
    LORE_CONTEXT
        .scope(execution(), async {
            let (repository, _log) = counted(None).await;
            let content = random(FILE, 47);
            let address = write(&repository, &content).await;
            let cache = prefetching_under(LISTS_ONLY);
            for offset in (0..FILE / 2).step_by(READ) {
                settle(&cache).await;
                read(&cache, &repository, INO, address, FILE, offset, READ).await;
            }
            settle(&cache).await;
            // Back to the start: the stream breaks.
            read(&cache, &repository, INO, address, FILE, 0, READ).await;
            settle(&cache).await;
            let reference = FragmentCache::new(LISTS_ONLY);
            for offset in (0..FILE / 2).step_by(READ).chain([0]) {
                read(&reference, &repository, INO, address, FILE, offset, READ).await;
            }
            let demand = reference.inode_held.load(Ordering::SeqCst);
            let held = cache.inode_held.load(Ordering::SeqCst);
            assert_eq!(
                held,
                demand + cache.stream_bytes(INO),
                "prefetched leaves still held"
            );
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn prefetch_evicts_no_other_inode_and_keeps_to_its_share() {
    LORE_CONTEXT
        .scope(execution(), async {
            const BUDGET: usize = 2 << 20;
            let (repository, _log) = counted(None).await;
            let demand = compressible(FILE, 48);
            let demand_address = write(&repository, &demand).await;
            let content = random(FILE, 49);
            let address = write(&repository, &content).await;
            let cache = FragmentCache::new(DEFAULT_FRAGMENT_CACHE)
                .with_inode_budget(BUDGET)
                .into_shared();
            cache.set_sequential_prefetch(true);
            // Another inode's decoded leaves, loaded on demand.
            for offset in [0, 1 << 20, 2 << 20] {
                read(
                    &cache,
                    &repository,
                    INO + 1,
                    demand_address,
                    FILE,
                    offset,
                    READ,
                )
                .await;
            }
            let mut most = 0;
            in_order(&cache, &repository, address, &content, READ, |cache| {
                most = most.max(cache.ahead_bytes());
            })
            .await;
            assert!(cache.prefetch_counts().prefetched > 0, "it prefetched");
            assert_eq!(
                cache.evictions.load(Ordering::SeqCst),
                0,
                "prefetch evicted demand state"
            );
            assert!(
                most <= BUDGET / 4,
                "{most} prefetched bytes over a quarter of {BUDGET}"
            );
        })
        .await;
}

/// Two levels of lists, 1 KiB leaves: sub-lists of 6553 leaves, then a trailing one of 100
/// spanning less than a fragment's content.
async fn two_level(repository: &Arc<RepositoryContext>) -> (Vec<u8>, Address) {
    let content = random((2 * 6553 + 100) << 10, 50);
    let address = write_chunked(repository, &content, 1 << 10).await;
    let (_, root) = lore_storage::load_fragment(
        repository.immutable_store(),
        repository.id,
        address,
        ReadOptions::default().no_decompress(),
        None,
    )
    .await
    .expect("root");
    let list = root.to_aligned::<FragmentReference>();
    assert_eq!(
        list.as_type_slice::<FragmentReference>().len(),
        3,
        "three sub-lists"
    );
    (content, address)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_two_level_file_streams_across_its_list_boundaries() {
    LORE_CONTEXT
        .scope(execution(), async {
            const SMALL: usize = 16 << 10;
            let (repository, _log) = counted(None).await;
            let (content, address) = two_level(&repository).await;
            let cache = prefetching();
            let loaded = in_order(&cache, &repository, address, &content, SMALL, |_| {}).await;
            let counts = cache.prefetch_counts();
            // The first READ loads the root, a sub-list and 16 leaves, the second 16 leaves; past
            // that a READ entering a sub-list loads only that list, which prefetch keeps to itself.
            assert_eq!(
                (loaded, counts.read_loads),
                (4, 18 + 16 + 2),
                "of {} READs: {counts:?}",
                content.len() / SMALL
            );
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forget_while_a_prefetch_waits_in_the_store_leaves_nothing_behind() {
    LORE_CONTEXT
        .scope(execution(), async {
            let (repository, log) = counted(None).await;
            let content = random(FILE, 51);
            let address = write(&repository, &content).await;
            let cache = prefetching();
            hold_the_next_batch(&cache, &repository, &log, address, &content).await;
            let mut out = vec![0u8; READ];
            // Another inode's READ, held in the store by its root list's load, then dropped.
            let dropped = tokio::time::timeout(
                Duration::from_millis(5),
                cache.read(
                    &repository,
                    INO + 1,
                    address,
                    FILE as u64,
                    0,
                    &mut out,
                    false,
                ),
            )
            .await;
            assert!(dropped.is_err(), "the READ was dropped mid-load");
            drop(out);
            cache.forget(INO + 1);
            cache.forget(INO);
            log.gate.open();
            settle(&cache).await;
            assert_eq!(
                cache.counters.running.load(Ordering::SeqCst),
                0,
                "the task ended"
            );
            assert_eq!(cache.inode_held.load(Ordering::SeqCst), 0);
            assert_eq!(cache.ahead_bytes(), 0);
            let got = read(&cache, &repository, INO, address, FILE, 0, FILE).await;
            assert!(got == content, "content reads back after the forget");
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_served_from_prefetched_leaves_allocate_nothing() {
    LORE_CONTEXT
        .scope(execution(), async {
            let (repository, _log) = counted(None).await;
            let content = random(FILE, 52);
            let address = write(&repository, &content).await;
            let cache = prefetching();
            let mut out = vec![0u8; READ];
            let mut served = 0;
            for offset in (0..FILE).step_by(READ) {
                settle(&cache).await;
                // A pin collects garbage every 128th time, deferring the freed queue nodes into
                // this thread's bag, and a full bag allocates a node (crossbeam-epoch
                // `Global::collect`, `Local::defer`); an empty bag cannot fill in one READ.
                crossbeam_epoch::pin().flush();
                let loads = cache.prefetch_counts().read_loads;
                lore_base::allocator::set_allocation_counting(true);
                let before = lore_base::allocator::thread_allocation_count();
                cache
                    .read(
                        &repository,
                        INO,
                        address,
                        FILE as u64,
                        offset as u64,
                        &mut out,
                        false,
                    )
                    .await
                    .expect("read");
                let made = lore_base::allocator::thread_allocation_count() - before;
                assert!(
                    out[..] == content[offset..offset + READ],
                    "bytes at {offset}"
                );
                if cache.prefetch_counts().read_loads == loads {
                    served += 1;
                    assert_eq!(
                        made, 0,
                        "the READ at {offset} served from prefetch allocated"
                    );
                }
            }
            assert!(
                served >= FILE / READ - 3,
                "{served} READs served from prefetch"
            );
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn switched_off_no_stream_state_is_allocated() {
    LORE_CONTEXT
        .scope(execution(), async {
            let (repository, _log) = counted(None).await;
            let content = random(FILE, 53);
            let address = write(&repository, &content).await;
            let cache = FragmentCache::new(DEFAULT_FRAGMENT_CACHE).into_shared();
            in_order(&cache, &repository, address, &content, READ, |_| {}).await;
            assert_eq!(cache.stream_state(INO), None);
            assert_eq!(cache.stream_bytes(INO), 0);
        })
        .await;
}

#[test]
fn a_stream_moves_on_when_it_breaks_only_while_a_task_serves_it() {
    let stream = Stream::default();
    let generation = || stream.generation.load(Ordering::SeqCst);
    assert!(!stream.note(0, READ as u64), "one READ is no run");
    assert!(
        stream.note(READ as u64, 2 * READ as u64),
        "two READs in a row are"
    );
    assert!(stream.claim(), "the first task claims the stream");
    assert!(!stream.claim(), "a second does not");
    assert!(stream.note(2 * READ as u64, 3 * READ as u64));
    let served = generation();
    assert!(
        !stream.note(FILE as u64, FILE as u64 + 1),
        "a jump breaks the run"
    );
    assert_eq!(generation(), served + 1, "and moves the served stream on");
    assert_eq!(stream.run.load(Ordering::SeqCst), 0);
    stream.active.store(false, Ordering::SeqCst);
    assert!(!stream.note(0, 1));
    assert_eq!(
        generation(),
        served + 1,
        "a stream no task serves stays put"
    );
    stream.stop();
    assert_eq!(generation(), served + 1, "and a stop leaves it alone");
    assert!(stream.claim());
    stream.stop();
    assert_eq!(generation(), served + 2, "a stop moves a served stream on");
}

#[test]
fn a_read_reaching_the_task_wakes_it() {
    struct Wakes(AtomicUsize);
    impl futures::task::ArcWake for Wakes {
        fn wake_by_ref(this: &Arc<Self>) {
            this.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let wakes = Arc::new(Wakes(AtomicUsize::new(0)));
    let waker = futures::task::waker(wakes.clone());
    let mut cx = std::task::Context::from_waker(&waker);
    let slot = InodeSlot::new(INO, lore_storage::hash::hash_slice(b"root"));
    let stream = Stream::default();
    let mut wait = std::pin::pin!(stream.wait(&slot, 0, 2 * READ as u64));
    assert!(wait.as_mut().poll(&mut cx).is_pending());
    stream.note(0, READ as u64);
    assert_eq!(
        wakes.0.load(Ordering::SeqCst),
        0,
        "short of where the task waits"
    );
    stream.note(READ as u64, 2 * READ as u64);
    assert_eq!(
        wakes.0.load(Ordering::SeqCst),
        1,
        "the READ reaching it wakes the task"
    );
    assert!(wait.as_mut().poll(&mut cx).is_ready());
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_cache_ends_its_tasks() {
    LORE_CONTEXT
        .scope(execution(), async {
            let (repository, _log) = counted(None).await;
            let content = random(FILE, 54);
            let address = write(&repository, &content).await;
            let cache = prefetching();
            for offset in [0, READ] {
                read(&cache, &repository, INO, address, FILE, offset, READ).await;
            }
            settle(&cache).await;
            assert_eq!(
                cache.counters.running.load(Ordering::SeqCst),
                1,
                "a task waits"
            );
            let slot = Arc::downgrade(&cache.inodes.get(&INO).expect("slot"));
            drop(cache);
            until("the task lets go of the inode's state", || {
                slot.upgrade().is_none()
            })
            .await;
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn new_content_or_a_strategy_switch_mid_stream_ends_the_task() {
    LORE_CONTEXT
        .scope(execution(), async {
            let (repository, _log) = counted(None).await;
            let content = random(FILE, 55);
            let address = write(&repository, &content).await;
            let other = random(FILE, 56);
            let other_address = write(&repository, &other).await;
            let cache = prefetching();
            for offset in [0, READ] {
                read(&cache, &repository, INO, address, FILE, offset, READ).await;
            }
            settle(&cache).await;
            let old = Arc::downgrade(&cache.inodes.get(&INO).expect("slot"));
            let got = read(
                &cache,
                &repository,
                INO,
                other_address,
                FILE,
                2 * READ,
                READ,
            )
            .await;
            assert!(got == other[2 * READ..3 * READ], "the new content");
            until("the old content's task ends", || old.upgrade().is_none()).await;
            settle(&cache).await;
            assert_eq!(cache.counters.running.load(Ordering::SeqCst), 0);

            for offset in [0, READ, 2 * READ] {
                read(&cache, &repository, INO, other_address, FILE, offset, READ).await;
            }
            settle(&cache).await;
            assert_eq!(
                cache.counters.running.load(Ordering::SeqCst),
                1,
                "a new task waits"
            );
            cache.set_strategy(FragmentCacheStrategy::PerInode(LeafPolicy::Load));
            settle(&cache).await;
            assert_eq!(cache.counters.running.load(Ordering::SeqCst), 0);
            assert_eq!(cache.inode_held.load(Ordering::SeqCst), 0);
            assert_eq!(cache.ahead_bytes(), 0);
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn scattered_reads_prefetch_nothing() {
    LORE_CONTEXT
        .scope(execution(), async {
            let (repository, _log) = counted(None).await;
            let content = random(FILE, 33);
            let address = write(&repository, &content).await;
            let cache = prefetching();
            let mut x = 0x2545_f491_4f6c_dd1du64;
            for _ in 0..64 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let offset = (x as usize % (FILE / READ)) * READ;
                let got = read(&cache, &repository, INO, address, FILE, offset, READ).await;
                assert!(got == content[offset..offset + READ]);
            }
            settle(&cache).await;
            let counts = cache.prefetch_counts();
            assert_eq!(
                (counts.streams, counts.prefetch_loads, counts.prefetched),
                (0, 0, 0),
                "{counts:?}"
            );
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn prefetched_bytes_stay_within_the_budget_and_forget_frees_them() {
    LORE_CONTEXT
        .scope(execution(), async {
            const BUDGET: usize = 1 << 20;
            let (repository, _log) = counted(None).await;
            let content = random(FILE, 34);
            let address = write(&repository, &content).await;
            let cache = FragmentCache::new(DEFAULT_FRAGMENT_CACHE)
                .with_inode_budget(BUDGET)
                .into_shared();
            cache.set_sequential_prefetch(true);
            let mut most = 0;
            in_order(&cache, &repository, address, &content, READ, |cache| {
                most = most.max(cache.inode_held.load(Ordering::SeqCst));
            })
            .await;
            assert!(cache.prefetch_counts().prefetched > 0, "it prefetched");
            assert!(most <= BUDGET, "{most} bytes held over a {BUDGET} budget");
            cache.forget(INO);
            settle(&cache).await;
            assert_eq!(cache.inode_held.load(Ordering::SeqCst), 0);
            assert_eq!(cache.ahead_bytes(), 0);
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn switched_off_it_loads_exactly_what_it_did_without_prefetch() {
    LORE_CONTEXT
        .scope(execution(), async {
            let (repository, log) = counted(None).await;
            let content = random(FILE, 35);
            let address = write(&repository, &content).await;
            let never = FragmentCache::new(DEFAULT_FRAGMENT_CACHE);
            let off = FragmentCache::new(DEFAULT_FRAGMENT_CACHE).into_shared();
            let toggled = prefetching();
            toggled.set_sequential_prefetch(false);
            let mut seen = Vec::new();
            for cache in [&never, &*off, &*toggled] {
                log.reset();
                let loaded = in_order(cache, &repository, address, &content, READ, |_| {}).await;
                let counts = cache.prefetch_counts();
                assert_eq!(
                    (counts.streams, counts.prefetch_loads, counts.prefetched),
                    (0, 0, 0),
                    "{counts:?}"
                );
                seen.push((log.loads(), loaded, counts.read_loads));
            }
            assert_eq!(seen[0].0.0, 1, "one list load: {seen:?}");
            assert!(seen.iter().all(|s| *s == seen[0]), "{seen:?}");
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "diagnostic"]
async fn sequential_read_counts() {
    LORE_CONTEXT
        .scope(execution(), async {
            let dir = lore_base::test_util::TempDir::new("fragment-cache-prefetch-");
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
            let content = if std::env::var("LORE_COUNT_CONTENT").as_deref() == Ok("compressible") {
                compressible(size, 37)
            } else {
                random(size, 37)
            };
            let address = write(&repository, &content).await;
            repository
                .immutable_store()
                .flush(FlushOptions::SYNC_DATA)
                .await
                .expect("flush");
            let reads = size / READ;
            println!(
                "| prefetch | READs | READs loading | READ-time loads | prefetch loads | \
                 prefetched MiB | used MiB | wasted MiB | gets | compressed payloads | \
                 allocations per READ | per READ served whole |"
            );
            println!("|---|---|---|---|---|---|---|---|---|---|---|---|");
            let mut out = vec![0u8; READ];
            for on in [false, true] {
                let cache = FragmentCache::new(DEFAULT_FRAGMENT_CACHE).into_shared();
                cache.set_sequential_prefetch(on);
                log.reset();
                log.operations.snapshot();
                let (mut loading, mut allocations, mut whole_allocations) = (0, 0, 0);
                for offset in (0..size).step_by(READ) {
                    settle(&cache).await;
                    let loads = cache.prefetch_counts().read_loads;
                    lore_base::allocator::set_allocation_counting(true);
                    let before = lore_base::allocator::thread_allocation_count();
                    cache
                        .read(
                            &repository,
                            INO,
                            address,
                            size as u64,
                            offset as u64,
                            &mut out,
                            false,
                        )
                        .await
                        .expect("read");
                    let made = lore_base::allocator::thread_allocation_count() - before;
                    lore_base::allocator::set_allocation_counting(false);
                    assert!(
                        out[..] == content[offset..offset + READ],
                        "bytes at {offset}"
                    );
                    allocations += made;
                    if cache.prefetch_counts().read_loads == loads {
                        whole_allocations += made;
                    } else {
                        loading += 1;
                    }
                }
                settle(&cache).await;
                let counts = cache.prefetch_counts();
                let (_, gets) = log.loads();
                let [_, _, compressed, _, _] = log.operations.snapshot();
                let mib = |bytes: usize| bytes as f64 / f64::from(1 << 20);
                let whole = reads - loading;
                println!(
                    "| {on} | {reads} | {loading} | {} | {} | {:.2} | {:.2} | {:.2} | \
                     {gets} | {compressed} | {:.3} | {} |",
                    counts.read_loads,
                    counts.prefetch_loads,
                    mib(counts.prefetched),
                    mib(counts.used),
                    mib(counts.prefetched - counts.used),
                    allocations as f64 / reads as f64,
                    if whole == 0 {
                        "-".to_string()
                    } else {
                        format!("{:.3}", whole_allocations as f64 / whole as f64)
                    },
                );
            }
        })
        .await;
}
