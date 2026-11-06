// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use lore_base::test_util::TempDir;
use lore_base::types::KeyType;
use lore_storage::Address;
use lore_storage::CallerBuffer;
use lore_storage::Context;
use lore_storage::Fragment;
use lore_storage::Hash;
use lore_storage::Partition;
use lore_storage::error::StorageError;
use lore_storage::fragment_flags::FragmentFlags;
use lore_storage::hash;
use lore_storage::immutable_store::CopyBehavior;
use lore_storage::immutable_store::ImmutableStore;
use lore_storage::immutable_store::StoreError;
use lore_storage::local::immutable_store::ImmutableStoreSettings;
use lore_storage::local::immutable_store::LocalImmutableStore;
use lore_storage::mutable_store::MutableStore;
use lore_storage::options::ReadOptions;
use lore_storage::read::*;
use lore_storage::store_types::PayloadRead;
use lore_storage::write::try_acquire_in_flight;
use lore_transport::ProtocolError;
use lore_transport::StorageSession;

async fn make_test_store() -> (TempDir, Arc<dyn ImmutableStore>) {
    let dir = TempDir::new("lore-storage-read-test-");
    let store = LocalImmutableStore::new(
        Some(PathBuf::from(dir.as_ref())),
        ImmutableStoreSettings::default(),
    )
    .await
    .expect("create test store");
    (dir, store)
}

fn make_input(seed: u8) -> (Partition, Address, Fragment, Bytes) {
    let payload = vec![seed; 64];
    let hash_value = hash::hash_slice(&payload);
    let partition = Partition::from([seed; 16]);
    let address = Address {
        hash: hash_value,
        context: Context::from([seed; 16]),
    };
    let fragment = Fragment {
        flags: FragmentFlags::PayloadStoredLocal.bits(),
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    };
    (partition, address, fragment, Bytes::from(payload))
}

async fn store_with_isolation(isolate_partitions: bool) -> (TempDir, Arc<dyn ImmutableStore>) {
    let dir = TempDir::new("lore-storage-isolation-test-");
    let store = LocalImmutableStore::new(
        Some(PathBuf::from(dir.as_ref())),
        ImmutableStoreSettings {
            isolate_partitions,
            ..Default::default()
        },
    )
    .await
    .expect("create test store");
    (dir, store)
}

/// Store `payload` as one unfragmented, uncompressed fragment addressed by its own hash.
async fn put_whole(
    store: &Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
    payload: &Bytes,
) -> Address {
    let address = Address {
        hash: hash::hash_slice(payload.as_ref()),
        context,
    };
    store
        .clone()
        .put(
            partition,
            address,
            Fragment {
                flags: FragmentFlags::PayloadStoredLocal.bits(),
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            },
            Some(payload.clone()),
            false,
        )
        .await
        .expect("put content");
    address
}

/// Store `size` bytes of compressible content as one compressed fragment, reporting its address
/// and the content it expands to.
async fn put_compressed(
    store: &Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
    size: usize,
) -> (Address, Vec<u8>) {
    let content: Vec<u8> = (0..size).map(|index| (index / 64) as u8).collect();
    let plain = Fragment {
        flags: 0,
        size_payload: content.len() as u32,
        size_content: content.len() as u64,
    };
    let (fragment, payload) = lore_storage::compress::compress(
        plain,
        &content,
        lore_storage::compress::CompressionMode::Lz4,
    )
    .expect("compress test content");
    assert!(
        (payload.len() as u64) < fragment.size_content,
        "test needs a payload shorter than its content, got {} of {}",
        payload.len(),
        fragment.size_content,
    );

    let address = Address {
        hash: hash::hash_slice(&content),
        context,
    };
    store
        .clone()
        .put(partition, address, fragment, Some(payload), false)
        .await
        .expect("put compressed fragment");
    (address, content)
}

/// Store content spanning two leaves and the list naming them, reporting the list's address and
/// the content the two leaves reassemble to.
async fn put_two_leaf_list(
    store: &Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
) -> (Address, Vec<u8>) {
    use lore_storage::FragmentReference;
    use zerocopy::IntoBytes;

    let first = Bytes::from((0u8..64).collect::<Vec<u8>>());
    let second = Bytes::from((64u8..128).collect::<Vec<u8>>());
    let first_address = put_whole(store, partition, context, &first).await;
    let second_address = put_whole(store, partition, context, &second).await;

    let refs_payload = Bytes::copy_from_slice(
        [
            FragmentReference {
                hash: first_address.hash,
                offset_content: 0,
            },
            FragmentReference {
                hash: second_address.hash,
                offset_content: first.len() as u64,
            },
        ]
        .as_bytes(),
    );
    let content_size = (first.len() + second.len()) as u64;
    let root_address = Address {
        hash: hash::hash_slice(refs_payload.as_ref()),
        context,
    };
    store
        .clone()
        .put(
            partition,
            root_address,
            Fragment {
                flags: FragmentFlags::PayloadFragmented.bits(),
                size_payload: refs_payload.len() as u32,
                size_content: content_size,
            },
            Some(refs_payload),
            false,
        )
        .await
        .expect("put the fragment list");

    let mut content = first.to_vec();
    content.extend_from_slice(&second);
    (root_address, content)
}

/// Read through [`read_into_buffer`] into a `capacity` byte buffer, reporting the buffer
/// alongside the number of bytes written.
async fn read_into_vec(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    range: Option<Range<usize>>,
    capacity: usize,
    options: ReadOptions,
) -> Result<(Vec<u8>, usize), StorageError> {
    let mut buffer = vec![0u8; capacity];
    // SAFETY: the buffer outlives the read and nothing else touches it.
    let mut dst = unsafe { CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
    let (_fragment, written) =
        read_into_buffer(store, partition, address, range, &mut dst, options, None).await?;
    Ok((buffer, written))
}

/// Delegating store counting how each payload reached the reader: `get` hands back a buffer it
/// allocated, `get_into` places the bytes where the reader asked for them.
///
/// The bytes delivered are the same either way, so a read that stopped landing in the caller's
/// buffer would still return the right content. The counts are what tells the two apart.
///
/// After its first read, it notifies its release and lands its landing, where it was given them.
struct CountingReadStore {
    inner: Arc<dyn ImmutableStore>,
    gets: Arc<std::sync::atomic::AtomicUsize>,
    gets_into: Arc<std::sync::atomic::AtomicUsize>,
    hooks: Arc<std::sync::Mutex<ReadHooks>>,
    /// Every payload it hands back carries a byte past the length its fragment gives, as a store
    /// that does not check a payload's length against its fragment could answer it.
    pad: bool,
}

/// What a [`CountingReadStore`] does around its first read: answer it `placement_error` where it is
/// a placement and one is set, then notify `release`, and land `landing` by putting its payload and
/// dropping its in-flight guard, as a tracker leader finishing during a reader's probe would.
#[derive(Default)]
struct ReadHooks {
    release: Option<Arc<tokio::sync::Notify>>,
    landing: Option<Landing>,
    placement_error: Option<StoreError>,
}

type Landing = (
    Partition,
    Address,
    Fragment,
    Bytes,
    lore_storage::write::StoreInFlightGuard,
);

impl CountingReadStore {
    fn wrap(inner: Arc<dyn ImmutableStore>) -> (Arc<dyn ImmutableStore>, Self) {
        Self::with_hooks(inner, ReadHooks::default())
    }

    /// A store padding every payload it hands back ([`Self::pad`]).
    fn padding(inner: Arc<dyn ImmutableStore>) -> Arc<dyn ImmutableStore> {
        let (_, counts) = Self::wrap(inner);
        Arc::new(CountingReadStore {
            pad: true,
            ..counts
        })
    }

    fn with_hooks(
        inner: Arc<dyn ImmutableStore>,
        hooks: ReadHooks,
    ) -> (Arc<dyn ImmutableStore>, Self) {
        let counts = CountingReadStore {
            inner,
            gets: Arc::default(),
            gets_into: Arc::default(),
            hooks: Arc::new(std::sync::Mutex::new(hooks)),
            pad: false,
        };
        let store: Arc<dyn ImmutableStore> = Arc::new(CountingReadStore {
            inner: counts.inner.clone(),
            gets: counts.gets.clone(),
            gets_into: counts.gets_into.clone(),
            hooks: counts.hooks.clone(),
            pad: false,
        });
        (store, counts)
    }

    async fn after_read(&self) {
        let ReadHooks {
            release, landing, ..
        } = std::mem::take(&mut *self.hooks.lock().expect("hooks"));
        if let Some(release) = release {
            release.notify_one();
        }
        if let Some((partition, address, fragment, payload, guard)) = landing {
            self.inner
                .clone()
                .put(partition, address, fragment, Some(payload), false)
                .await
                .expect("the landing store writes");
            drop(guard);
        }
    }

    fn gets(&self) -> usize {
        self.gets.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn gets_into(&self) -> usize {
        self.gets_into.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Every time the store was asked for the payload, by either read.
    fn probes(&self) -> usize {
        self.gets() + self.gets_into()
    }

    /// `payload`, padded where the store pads ([`Self::pad`]).
    fn padded(&self, payload: Bytes) -> Bytes {
        if !self.pad {
            return payload;
        }
        let mut padded = payload.to_vec();
        padded.push(0);
        Bytes::from(padded)
    }
}

#[async_trait::async_trait]
impl ImmutableStore for CountingReadStore {
    /// Joins the claim of the store this one wraps.
    async fn hold_for_command(
        self: std::sync::Arc<Self>,
    ) -> std::result::Result<
        Option<lore_storage::local::store_lock::StoreGuard>,
        lore_storage::StoreError,
    > {
        lore_storage::ImmutableStore::hold_for_command(self.inner.clone()).await
    }

    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<lore_storage::store_types::StoreGetData, StoreError> {
        self.gets.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let got = self.inner.clone().get(partition, address).await;
        self.after_read().await;
        got.map(|mut data| {
            data.payload = data.payload.map(|payload| self.padded(payload));
            data
        })
    }

    async fn get_into(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        dst: &mut lore_storage::CallerBuffer,
    ) -> Result<(Fragment, PayloadRead), StoreError> {
        self.gets_into
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let placement_error = self.hooks.lock().expect("hooks").placement_error.clone();
        let got = match placement_error {
            Some(err) => Err(err),
            None => self.inner.clone().get_into(partition, address, dst).await,
        };
        self.after_read().await;
        got.map(|(fragment, read)| match read {
            PayloadRead::Returned(payload) => {
                (fragment, PayloadRead::Returned(self.padded(payload)))
            }
            PayloadRead::IntoBuffer => (fragment, PayloadRead::IntoBuffer),
        })
    }

    fn is_local(&self) -> bool {
        self.inner.clone().is_local()
    }

    async fn get_metadata(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<lore_storage::store_types::StoreGetData, StoreError> {
        self.inner.clone().get_metadata(partition, address).await
    }

    async fn query(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        results: &mut [lore_storage::store_types::StoreMatchResult],
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .query(partition, addresses, results)
            .await
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
        stats: Arc<lore_storage::store_types::StoreObliterateStats>,
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
        None
    }

    async fn flush(self: Arc<Self>, sync_data: bool) -> Result<(), StoreError> {
        self.inner.clone().flush(sync_data).await
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
        behavior: CopyBehavior,
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

/// A payload that is the content reaches the caller's buffer without a buffer being allocated
/// for it anywhere: the store places it, and nothing asks for it a second time.
#[tokio::test]
async fn a_whole_read_allocates_no_buffer_for_the_payload() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x81; 16]);
    let payload = Bytes::from((0u8..64).collect::<Vec<u8>>());
    let address = put_whole(&store, partition, Context::from([0x81; 16]), &payload).await;

    let (counting, counts) = CountingReadStore::wrap(store);
    let (buffer, written) = read_into_vec(
        counting,
        partition,
        address,
        None,
        payload.len(),
        ReadOptions::default().no_remote(),
    )
    .await
    .expect("read into the caller buffer");

    assert_eq!(buffer.as_slice(), payload.as_ref());
    assert_eq!(written, payload.len());
    assert_eq!(
        counts.gets_into(),
        1,
        "the payload was placed more than once"
    );
    assert_eq!(
        counts.gets(),
        0,
        "a buffer was allocated for a payload the store could have placed"
    );
}

/// A compressed payload is read once and expanded into the caller's buffer, so nothing reads it
/// again to expand it elsewhere.
#[tokio::test]
async fn a_compressed_read_reads_its_payload_once() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x82; 16]);
    let (address, content) =
        put_compressed(&store, partition, Context::from([0x82; 16]), 4096).await;

    let (counting, counts) = CountingReadStore::wrap(store);
    let (buffer, written) = read_into_vec(
        counting,
        partition,
        address,
        None,
        content.len(),
        ReadOptions::default().no_remote(),
    )
    .await
    .expect("read compressed content into the caller buffer");

    assert_eq!(buffer, content);
    assert_eq!(written, content.len());
    assert_eq!(counts.gets_into(), 1, "the payload was read more than once");
    assert_eq!(
        counts.gets(),
        0,
        "the compressed payload was fetched a second time to expand it"
    );
}

/// Read `address` whole through [`read_into`] into a slice of `size` bytes, reporting the slice
/// and how many times the store was asked for the payload.
async fn read_into_counted(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    size: usize,
    options: ReadOptions,
) -> (Result<Vec<u8>, StorageError>, usize) {
    let (counting, counts) = CountingReadStore::wrap(store);
    let mut slice = vec![0u8; size];
    let result = read_into(
        counting, partition, address, None, &mut slice, options, None,
    )
    .await
    .map(|()| slice);
    (result, counts.probes())
}

/// A whole read of a compressed fragment delivers its content from one load.
#[tokio::test]
async fn a_whole_compressed_read_into_a_slice_loads_its_payload_once() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x91; 16]);
    let (address, content) =
        put_compressed(&store, partition, Context::from([0x91; 16]), 4096).await;

    let (slice, probes) = read_into_counted(
        store,
        partition,
        address,
        content.len(),
        ReadOptions::default().no_remote(),
    )
    .await;

    assert_eq!(slice.expect("read compressed content"), content);
    assert_eq!(probes, 1, "the payload was loaded more than once");
}

/// A whole read into a slice reads each payload it needs from the local store once, a payload that
/// is the content and a list with its leaves alike, into memory of its own rather than the slice:
/// the list's leaves are placed in the buffer the read assembles the content in.
#[tokio::test]
async fn a_whole_read_into_a_slice_reads_each_local_payload_once() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x9a; 16]);
    let context = Context::from([0x9a; 16]);
    let payload = Bytes::from((0u8..64).rev().collect::<Vec<u8>>());
    let whole = put_whole(&store, partition, context, &payload).await;
    let (list, content) = put_two_leaf_list(&store, partition, context).await;

    for (address, expected, reads) in [(whole, payload.to_vec(), (0, 1)), (list, content, (2, 1))] {
        let (counting, counts) = CountingReadStore::wrap(store.clone());
        let mut slice = vec![0u8; expected.len()];
        read_into(
            counting,
            partition,
            address,
            None,
            &mut slice,
            ReadOptions::default().no_remote(),
            None,
        )
        .await
        .expect("read into the slice");

        assert_eq!(slice, expected);
        assert_eq!((counts.gets_into(), counts.gets()), reads);
    }
}

/// A stream and a file read, by address and resolved, refuse content larger than their cap, as
/// every other read does.
#[tokio::test]
async fn stream_and_file_reads_refuse_content_over_the_cap() {
    let (dir, store) = make_test_store().await;
    let partition = Partition::from([0x9b; 16]);
    let (address, _) = put_compressed(&store, partition, Context::from([0x9b; 16]), 4096).await;
    let capped = ReadOptions::default()
        .no_remote()
        .with_max_content_size(1024);
    let mutable = test_mutable_store(&dir, &store).await;
    let key = hash::hash_slice(b"a key naming capped content");
    mutable
        .clone()
        .store(partition, key, address.hash, KeyType::Resolve)
        .await
        .expect("publish the mapping");

    let (sender, _receiver) = tokio::sync::mpsc::channel(1);
    let streamed = read_stream(
        store.clone(),
        partition,
        address,
        None,
        capped,
        sender,
        None,
    )
    .await;
    let target = PathBuf::from(dir.as_ref()).join("capped.bin");
    let written = read_into_file(
        store.clone(),
        partition,
        address,
        target.as_path(),
        ".~loretemp",
        None,
        capped,
        None,
    )
    .await;
    let resolved_target = PathBuf::from(dir.as_ref()).join("capped-resolved.bin");
    let resolved = read_resolved_into_file(
        store,
        mutable,
        partition,
        key,
        address.context,
        0,
        resolved_target.as_path(),
        ".~loretemp",
        None,
        capped,
        None,
    )
    .await;

    assert!(
        matches!(streamed, Err(StorageError::Oversized(_))),
        "got {streamed:?}"
    );
    assert!(
        matches!(written, Err(StorageError::Oversized(_))),
        "got {written:?}"
    );
    assert!(
        matches!(resolved, Err(StorageError::Oversized(_))),
        "got {resolved:?}"
    );
    assert!(!target.exists(), "the capped content reached the file");
    assert!(
        !resolved_target.exists(),
        "the capped resolved content reached the file"
    );
}

/// A read into a buffer or a slice refuses local content larger than its cap from the one probe
/// that found it, whatever form the store holds it in.
#[tokio::test]
async fn a_read_into_caller_memory_refuses_local_content_over_the_cap_from_one_probe() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x9d; 16]);
    let context = Context::from([0x9d; 16]);
    let whole = put_whole(&store, partition, context, &Bytes::from(vec![0x9d; 2048])).await;
    let (compressed, _) = put_compressed(&store, partition, context, 4096).await;

    for address in [whole, compressed] {
        let (counting, counts) = CountingReadStore::wrap(store.clone());
        let read = read_into_vec(
            counting,
            partition,
            address,
            None,
            4096,
            ReadOptions::default()
                .no_remote()
                .with_max_content_size(1024),
        )
        .await;

        assert!(
            matches!(read, Err(StorageError::Oversized(_))),
            "got {read:?}"
        );
        assert_eq!(counts.probes(), 1, "the capped content was read again");

        lore_storage::test_util::reset_counts();
        let (sliced, probes) = read_into_counted(
            store.clone(),
            partition,
            address,
            4096,
            ReadOptions::default()
                .no_remote()
                .with_max_content_size(1024),
        )
        .await;
        assert!(
            matches!(sliced, Err(StorageError::Oversized(_))),
            "got {sliced:?}"
        );
        assert_eq!(
            probes, 1,
            "the capped content was read again into the slice"
        );
        assert_eq!(
            lore_storage::test_util::counts().1,
            0,
            "the capped content was decoded"
        );
    }
}

/// A read into a slice shorter than the content is `Oversized` from the root's fragment alone,
/// whatever form the store holds it in: nothing is decoded.
#[tokio::test]
async fn a_read_into_a_short_slice_is_oversized() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x9e; 16]);
    let context = Context::from([0x9e; 16]);
    let whole = put_whole(&store, partition, context, &Bytes::from(vec![0x9e; 64])).await;
    let (compressed, _) = put_compressed(&store, partition, context, 4096).await;
    let (list, _) = put_two_leaf_list(&store, partition, context).await;

    for address in [whole, compressed, list] {
        lore_storage::test_util::reset_counts();
        let (read, probes) = read_into_counted(
            store.clone(),
            partition,
            address,
            32,
            ReadOptions::default().no_remote(),
        )
        .await;

        let (_, decodes, _) = lore_storage::test_util::counts();
        assert!(
            matches!(read, Err(StorageError::Oversized(_))),
            "got {read:?}"
        );
        assert_eq!(probes, 1, "more than the root was read");
        assert_eq!(decodes, 0, "the root was decoded");
    }
}

/// A read into a slice of content spread across fragments assembles it from the list.
#[tokio::test]
async fn a_fragmented_read_into_a_slice_assembles_the_content() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x93; 16]);
    let (address, content) = put_two_leaf_list(&store, partition, Context::from([0x93; 16])).await;

    let (slice, _) = read_into_counted(
        store,
        partition,
        address,
        content.len(),
        ReadOptions::default().no_remote(),
    )
    .await;

    assert_eq!(slice.expect("read fragmented content"), content);
}

/// A payload the local store does not hold is looked for through the load that falls back to
/// the remote, and with no remote it is reported missing.
#[tokio::test]
async fn a_payload_the_local_store_lacks_is_reported_missing() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x94; 16]);
    let address = Address {
        hash: hash::hash_slice(b"never stored"),
        context: Context::from([0x94; 16]),
    };

    let (slice, probes) =
        read_into_counted(store, partition, address, 16, ReadOptions::default()).await;

    assert!(
        matches!(slice, Err(StorageError::AddressNotFound(_))),
        "a payload no store holds was not reported missing"
    );
    assert_eq!(
        probes, 1,
        "the local store was asked again for a payload it lacks"
    );
}

/// A range of a compressed fragment is delivered into a slice of the range's size.
#[tokio::test]
async fn a_ranged_compressed_read_into_a_slice_delivers_the_range() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x96; 16]);
    let (address, content) =
        put_compressed(&store, partition, Context::from([0x96; 16]), 4096).await;

    let mut slice = vec![0u8; 100];
    read_into(
        store,
        partition,
        address,
        Some(1000..1100),
        &mut slice,
        ReadOptions::default().no_remote(),
        None,
    )
    .await
    .expect("read a range of compressed content");

    assert_eq!(slice, content[1000..1100]);
}

/// A range covering the whole of a compressed fragment is read as the whole content.
#[tokio::test]
async fn a_range_covering_compressed_content_reads_it_whole() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x98; 16]);
    let (address, content) =
        put_compressed(&store, partition, Context::from([0x98; 16]), 4096).await;
    let (counting, counts) = CountingReadStore::wrap(store);

    let mut slice = vec![0u8; content.len()];
    read_into(
        counting,
        partition,
        address,
        Some(0..content.len() + 8),
        &mut slice,
        ReadOptions::default().no_remote(),
        None,
    )
    .await
    .expect("read compressed content through a covering range");

    assert_eq!(slice, content);
    assert_eq!((counts.gets_into(), counts.gets()), (0, 1));
}

/// A slice the content does not fill is refused rather than reported filled.
#[tokio::test]
async fn a_slice_longer_than_compressed_content_is_refused() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x97; 16]);
    let (address, content) =
        put_compressed(&store, partition, Context::from([0x97; 16]), 4096).await;

    let (slice, _) = read_into_counted(
        store,
        partition,
        address,
        content.len() + 8,
        ReadOptions::default().no_remote(),
    )
    .await;

    assert!(
        slice.is_err(),
        "a slice longer than its content was reported filled"
    );
}

/// The zero hash names empty content, which leaves the slice untouched without a store lookup.
#[tokio::test]
async fn a_read_of_the_zero_hash_into_a_slice_leaves_it_untouched() {
    let (_dir, store) = make_test_store().await;
    let address = Address {
        hash: Hash::default(),
        context: Context::from([0x95; 16]),
    };

    let (slice, probes) = read_into_counted(
        store,
        Partition::from([0x95; 16]),
        address,
        8,
        ReadOptions::default().no_remote(),
    )
    .await;

    assert_eq!(slice.expect("read the zero hash"), vec![0u8; 8]);
    assert_eq!(probes, 0, "the store was asked for the zero hash");
}

/// [`a_read_of_the_zero_hash_into_a_slice_leaves_it_untouched`], for a read into a buffer.
#[tokio::test]
async fn a_buffer_read_of_the_zero_hash_writes_nothing_and_asks_no_store() {
    let (_dir, inner) = make_test_store().await;
    let (store, counts) = CountingReadStore::wrap(inner);
    let address = Address {
        hash: Hash::default(),
        context: Context::from([0x95; 16]),
    };

    let read = read_into_vec(
        store,
        Partition::from([0x95; 16]),
        address,
        None,
        8,
        ReadOptions::default().no_remote(),
    )
    .await
    .expect("read the zero hash");

    assert_eq!(read, (vec![0u8; 8], 0));
    assert_eq!(counts.probes(), 0, "the store was asked for the zero hash");
}

/// The list the walk starts from is the one the lookup read, and each leaf is read into its own
/// place in the caller's buffer, so a fragmented read hands no payload back in a buffer of its
/// own.
#[tokio::test]
async fn a_fragmented_read_places_every_payload() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x83; 16]);
    let context = Context::from([0x83; 16]);
    let (root_address, content) = put_two_leaf_list(&store, partition, context).await;

    let (counting, counts) = CountingReadStore::wrap(store);
    let (buffer, written) = read_into_vec(
        counting,
        partition,
        root_address,
        None,
        content.len(),
        ReadOptions::default().no_remote(),
    )
    .await
    .expect("read fragmented content into the caller buffer");

    assert_eq!(buffer, content);
    assert_eq!(written, content.len());
    assert_eq!(
        counts.gets_into(),
        3,
        "the list and its two leaves are each read once"
    );
    assert_eq!(
        counts.gets(),
        0,
        "a buffer was allocated for a payload the store could have placed"
    );
}

/// A fragmented read assembling into a buffer of its own reads each whole leaf into its place
/// there, so only the list is handed back in a buffer of its own. A read into a slice assembles the
/// same way, see [`a_whole_read_into_a_slice_reads_each_local_payload_once`].
#[tokio::test]
async fn an_assembled_fragmented_read_places_every_whole_leaf() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x8d; 16]);
    let context = Context::from([0x8d; 16]);
    let (root_address, content) = put_two_leaf_list(&store, partition, context).await;

    let (counting, counts) = CountingReadStore::wrap(store);
    let (_fragment, bytes) = read(
        counting,
        partition,
        root_address,
        None,
        ReadOptions::default().no_remote(),
        None,
    )
    .await
    .expect("read fragmented content");
    assert_eq!(bytes.as_ref(), content.as_slice());
    assert_eq!(counts.gets_into(), 2, "a leaf was not read into place");
    assert_eq!(
        counts.gets(),
        1,
        "a buffer was allocated for a leaf the store could have placed"
    );
}

/// Read `address` into a lent buffer, drop the read once the store holds its memory for `held`,
/// and check the loan stays out until the holder lets go, after the holder's write.
async fn drop_a_held_read(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    held: Address,
    size: usize,
) -> Vec<u8> {
    let (holding, mut holds) = lore_storage::test_util::HoldingStore::wrap(
        store,
        held,
        lore_storage::test_util::HeldAnswer::Never,
    );

    let mut buffer = vec![0u8; size];
    // SAFETY: the buffer outlives the loan, which is awaited before the buffer is read.
    let mut dst = unsafe { CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
    let loan = dst.loan();
    let mut piece = tokio::select! {
        _ = read_into_buffer(
            holding,
            partition,
            address,
            None,
            &mut dst,
            ReadOptions::default().no_remote(),
            None,
        ) => panic!("a held read answered"),
        piece = holds.recv() => piece.expect("the store holds the read's memory"),
    };
    drop(dst);
    assert!(
        !loan.is_returned(),
        "the loan came back while the read's memory was still held"
    );

    piece.as_mut_slice().fill(0x5a);
    drop(piece);
    tokio::time::timeout(Duration::from_secs(5), loan.returned())
        .await
        .expect("the loan comes back once the holder lets go");
    buffer
}

/// A read dropped while an operation it started still holds the caller's memory, as an I/O
/// operation does past its future, keeps the loan out until that operation lets go.
#[tokio::test]
async fn a_dropped_read_keeps_its_loan_until_its_memory_is_let_go() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x8e; 16]);
    let payload = Bytes::from(vec![0x11u8; 64]);
    let address = put_whole(&store, partition, Context::from([0x8e; 16]), &payload).await;

    let buffer = drop_a_held_read(store, partition, address, address, payload.len()).await;
    assert!(
        buffer.iter().all(|byte| *byte == 0x5a),
        "the holder's write did not land before the loan came back"
    );
}

/// Dropping a fragmented read aborts its leaves' tasks, and the loan still stays out until a leaf's
/// memory, held past its task, is let go.
#[tokio::test]
async fn a_dropped_fragmented_read_keeps_its_loan_until_a_leaf_is_let_go() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x8f; 16]);
    let context = Context::from([0x8f; 16]);
    let (root_address, content) = put_two_leaf_list(&store, partition, context).await;
    let second_leaf = Address {
        hash: hash::hash_slice(&content[64..]),
        context,
    };

    let buffer = drop_a_held_read(store, partition, root_address, second_leaf, content.len()).await;
    assert!(
        buffer[64..].iter().all(|byte| *byte == 0x5a),
        "the leaf holder's write did not land before the loan came back"
    );
}

/// A range spanning leaves is walked into the caller's buffer, which holds the range from its
/// first byte rather than the content the range was cut from. Both leaves are clipped, so both
/// are loaded and cut.
#[tokio::test]
async fn a_ranged_fragmented_read_assembles_into_the_caller_buffer() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x88; 16]);
    let context = Context::from([0x88; 16]);
    let (root_address, content) = put_two_leaf_list(&store, partition, context).await;

    let (buffer, written) = read_into_vec(
        store,
        partition,
        root_address,
        Some(32..96),
        64,
        ReadOptions::default().no_remote(),
    )
    .await
    .expect("read a range of fragmented content into the caller buffer");

    assert_eq!(written, 64);
    assert_eq!(buffer.as_slice(), &content[32..96]);
}

/// A range covering a whole leaf reads that leaf into place, and reads no leaf the range does
/// not cover.
#[tokio::test]
async fn a_ranged_fragmented_read_places_a_whole_leaf() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x8a; 16]);
    let context = Context::from([0x8a; 16]);
    let (root_address, content) = put_two_leaf_list(&store, partition, context).await;

    let (counting, counts) = CountingReadStore::wrap(store);
    let (buffer, written) = read_into_vec(
        counting,
        partition,
        root_address,
        Some(64..128),
        64,
        ReadOptions::default().no_remote(),
    )
    .await
    .expect("read a whole leaf of fragmented content into the caller buffer");

    assert_eq!(written, 64);
    assert_eq!(buffer.as_slice(), &content[64..128]);
    assert_eq!(
        counts.gets_into(),
        1,
        "the leaf the range covers was not read into place"
    );
    assert_eq!(
        counts.gets(),
        1,
        "only the list itself is handed back in a buffer of its own"
    );
}

/// A range of content one fragment holds is cut from that fragment once it is expanded, and the
/// buffer holds the range from its first byte.
#[tokio::test]
async fn a_ranged_compressed_read_delivers_only_the_range() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x89; 16]);
    let (address, content) =
        put_compressed(&store, partition, Context::from([0x89; 16]), 4096).await;

    let (buffer, written) = read_into_vec(
        store,
        partition,
        address,
        Some(1000..1200),
        200,
        ReadOptions::default().no_remote(),
    )
    .await
    .expect("read a range of compressed content into the caller buffer");

    assert_eq!(written, 200);
    assert_eq!(buffer.as_slice(), &content[1000..1200]);
}

/// The payload of an unfragmented, uncompressed fragment is the content, so a whole read lands
/// in the caller's buffer directly.
#[tokio::test]
async fn a_whole_read_lands_in_the_caller_buffer() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x11; 16]);
    let payload = Bytes::from((0u8..64).collect::<Vec<u8>>());
    let address = put_whole(&store, partition, Context::from([0x11; 16]), &payload).await;

    let (buffer, written) = read_into_vec(
        store,
        partition,
        address,
        None,
        payload.len(),
        ReadOptions::default().no_remote(),
    )
    .await
    .expect("read into the caller buffer");

    assert_eq!(written, payload.len());
    assert_eq!(buffer.as_slice(), payload.as_ref());
}

/// A buffer short of the content is refused rather than filled with a prefix of it.
#[tokio::test]
async fn a_buffer_shorter_than_the_content_is_refused() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x22; 16]);
    let payload = Bytes::from((0u8..64).collect::<Vec<u8>>());
    let address = put_whole(&store, partition, Context::from([0x22; 16]), &payload).await;

    let mut buffer = vec![0u8; payload.len() - 1];
    // SAFETY: the buffer outlives the read and nothing else touches it.
    let mut dst = unsafe { CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
    let result = read_into_buffer(
        store,
        partition,
        address,
        None,
        &mut dst,
        ReadOptions::default().no_remote(),
        None,
    )
    .await;

    assert!(
        matches!(result, Err(StorageError::Oversized(_))),
        "a buffer short of the content was not refused"
    );
    assert!(
        buffer.iter().all(|byte| *byte == 0),
        "a refused read wrote a prefix into the buffer"
    );
}

/// A range is cut from the content, so the buffer holds the range rather than the whole.
#[tokio::test]
async fn a_ranged_read_delivers_only_the_range() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x33; 16]);
    let payload = Bytes::from((0u8..64).collect::<Vec<u8>>());
    let address = put_whole(&store, partition, Context::from([0x33; 16]), &payload).await;

    let (buffer, written) = read_into_vec(
        store,
        partition,
        address,
        Some(8..24),
        16,
        ReadOptions::default().no_remote(),
    )
    .await
    .expect("read a range into the caller buffer");

    assert_eq!(written, 16);
    assert_eq!(buffer.as_slice(), &payload[8..24]);
}

/// A compressed payload is not the content, so it comes back as its own buffer and expands
/// straight into the caller's, without a second buffer for the content in between.
#[tokio::test]
async fn a_compressed_read_expands_into_the_caller_buffer() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x77; 16]);
    let (address, content) =
        put_compressed(&store, partition, Context::from([0x77; 16]), 4096).await;

    let (buffer, written) = read_into_vec(
        store,
        partition,
        address,
        None,
        content.len(),
        ReadOptions::default().no_remote(),
    )
    .await
    .expect("read compressed content into the caller buffer");

    assert_eq!(written, content.len());
    assert_eq!(buffer, content);
}

/// Content spread across leaves is written into the caller's buffer in place, each leaf at its
/// own offset, so the buffer holds the whole content in content order.
#[tokio::test]
async fn a_fragmented_read_is_assembled_into_the_caller_buffer() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x44; 16]);
    let context = Context::from([0x44; 16]);
    let (root_address, content) = put_two_leaf_list(&store, partition, context).await;

    let (buffer, written) = read_into_vec(
        store,
        partition,
        root_address,
        None,
        content.len(),
        ReadOptions::default().no_remote(),
    )
    .await
    .expect("read fragmented content into the caller buffer");

    assert_eq!(written, content.len());
    assert_eq!(buffer, content);
}

/// A local read into a caller's buffer delivers the stored bytes without hashing them: bytes that
/// do not hash to their address are delivered as stored.
#[tokio::test]
async fn a_local_read_into_a_buffer_delivers_the_stored_bytes_unhashed() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x55; 16]);
    let payload = Bytes::from_static(b"bytes that do not hash to their address");
    let address = Address {
        hash: hash::hash_slice(b"other content"),
        context: Context::from([0x55; 16]),
    };
    store
        .clone()
        .put(
            partition,
            address,
            Fragment {
                flags: FragmentFlags::PayloadStoredLocal.bits(),
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            },
            Some(payload.clone()),
            false,
        )
        .await
        .expect("put content under a mismatched address");

    let (buffer, written) = read_into_vec(
        store,
        partition,
        address,
        None,
        payload.len(),
        ReadOptions::default().no_remote(),
    )
    .await
    .expect("a local read delivers the stored bytes");

    assert_eq!(written, payload.len());
    assert_eq!(buffer.as_slice(), payload.as_ref());
}

/// A local load serves the stored bytes unhashed, under debug assertions too: a local payload that
/// does not hash to its address is served as stored.
#[tokio::test]
async fn a_local_load_serves_the_stored_bytes_unhashed() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x56; 16]);
    let payload = Bytes::from_static(b"local bytes that do not hash to their address");
    let address = Address {
        hash: hash::hash_slice(b"different content"),
        context: Context::from([0x56; 16]),
    };
    store
        .clone()
        .put(
            partition,
            address,
            Fragment {
                flags: FragmentFlags::PayloadStoredLocal.bits(),
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            },
            Some(payload.clone()),
            false,
        )
        .await
        .expect("put content under a mismatched address");

    let (_, loaded) = load_fragment(
        store,
        partition,
        address,
        ReadOptions::default().no_remote(),
        None,
    )
    .await
    .expect("a local load serves the stored bytes");

    assert_eq!(loaded, payload);
}

/// The capacity is measured against the content a compressed payload expands to, not against
/// the payload, so a buffer that only fits the compressed form is refused before it expands.
#[tokio::test]
async fn a_buffer_shorter_than_expanded_content_is_refused() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x86; 16]);
    let (address, content) =
        put_compressed(&store, partition, Context::from([0x86; 16]), 4096).await;

    let mut buffer = vec![0u8; content.len() - 1];
    // SAFETY: the buffer outlives the read and nothing else touches it.
    let mut dst = unsafe { CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
    let result = read_into_buffer(
        store,
        partition,
        address,
        None,
        &mut dst,
        ReadOptions::default().no_remote(),
        None,
    )
    .await;

    assert!(
        matches!(result, Err(StorageError::Oversized(_))),
        "a buffer short of the expanded content was not refused"
    );
    assert!(
        buffer.iter().all(|byte| *byte == 0),
        "a refused read expanded into the buffer"
    );
}

/// A list is refused against the content it reassembles to, before any leaf is fetched for a
/// buffer that cannot hold them.
#[tokio::test]
async fn a_buffer_shorter_than_assembled_content_is_refused() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x87; 16]);
    let context = Context::from([0x87; 16]);
    let (root_address, content) = put_two_leaf_list(&store, partition, context).await;

    let mut buffer = vec![0u8; content.len() - 1];
    // SAFETY: the buffer outlives the read and nothing else touches it.
    let mut dst = unsafe { CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
    let result = read_into_buffer(
        store,
        partition,
        root_address,
        None,
        &mut dst,
        ReadOptions::default().no_remote(),
        None,
    )
    .await;

    assert!(
        matches!(result, Err(StorageError::Oversized(_))),
        "a buffer short of the assembled content was not refused"
    );
    assert!(
        buffer.iter().all(|byte| *byte == 0),
        "a refused read assembled into the buffer"
    );
}

/// A key resolving locally to a payload that is already the content reads straight into the
/// caller's buffer, reporting the hash it resolved to.
#[tokio::test]
async fn a_resolved_read_lands_in_the_caller_buffer() {
    use lore_storage::local::mutable_store::LocalMutableStore;
    use lore_storage::local::mutable_store::MutableStoreSettings;

    let (dir, store) = make_test_store().await;
    let partition = Partition::from([0x66; 16]);
    let context = Context::from([0x66; 16]);
    let payload = Bytes::from((0u8..64).collect::<Vec<u8>>());
    let address = put_whole(&store, partition, context, &payload).await;

    let mutable: Arc<dyn MutableStore> = Arc::new(
        LocalMutableStore::new(
            Some(PathBuf::from(dir.as_ref())),
            MutableStoreSettings::default(),
            store.clone(),
        )
        .await
        .expect("create mutable store"),
    );
    let key = hash::hash_slice(b"a key naming the content");
    mutable
        .clone()
        .store(partition, key, address.hash, KeyType::Resolve)
        .await
        .expect("publish the mapping");

    let mut buffer = vec![0u8; payload.len()];
    // SAFETY: the buffer outlives the read and nothing else touches it.
    let mut dst = unsafe { CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
    let (resolved, written) = read_resolved_into_buffer(
        store,
        mutable,
        partition,
        key,
        context,
        0,
        &mut dst,
        ReadOptions::default().no_remote(),
        None,
    )
    .await
    .expect("read the resolved content into the caller buffer");

    assert_eq!(resolved, address.hash);
    assert_eq!(written, payload.len());
    assert_eq!(buffer.as_slice(), payload.as_ref());
}

/// A local resolved stream without `decompress` hands a leaf over as stored: a zstd payload that
/// does not decode arrives untouched, where hashing or expanding it would fail.
#[tokio::test]
async fn a_raw_resolved_stream_neither_hashes_nor_expands() {
    use lore_storage::local::mutable_store::LocalMutableStore;
    use lore_storage::local::mutable_store::MutableStoreSettings;

    let (dir, store) = make_test_store().await;
    let partition = Partition::from([0x67; 16]);
    let context = Context::from([0x67; 16]);
    let payload = Bytes::from(vec![0x5a; 64]);
    let address = Address {
        hash: hash::hash_slice(b"content the payload does not decode to"),
        context,
    };
    let fragment = Fragment {
        flags: FragmentFlags::PayloadCompressedZstd.bits(),
        size_payload: payload.len() as u32,
        size_content: 128,
    };
    store
        .clone()
        .put(partition, address, fragment, Some(payload.clone()), false)
        .await
        .expect("put a leaf that does not decode");

    let mutable: Arc<dyn MutableStore> = Arc::new(
        LocalMutableStore::new(
            Some(PathBuf::from(dir.as_ref())),
            MutableStoreSettings::default(),
            store.clone(),
        )
        .await
        .expect("create mutable store"),
    );
    let key = hash::hash_slice(b"a key naming a leaf that does not decode");
    mutable
        .clone()
        .store(partition, key, address.hash, KeyType::Resolve)
        .await
        .expect("publish the mapping");

    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let (resolved, size_content) = read_resolved_stream(
        store,
        mutable,
        partition,
        key,
        context,
        0,
        ReadOptions::default().no_decompress().no_remote(),
        tx,
        None,
    )
    .await
    .expect("a raw resolved stream neither hashes nor expands");

    assert_eq!(resolved, address.hash);
    assert_eq!(size_content, fragment.size_content);
    let (delivered_fragment, delivered) = rx.recv().await.expect("a leaf").expect("delivered");
    assert_eq!(
        delivered_fragment.flags & FragmentFlags::PayloadCompressed,
        FragmentFlags::PayloadCompressedZstd.bits()
    );
    assert_eq!(delivered_fragment.size_content, fragment.size_content);
    assert_eq!(delivered, payload);
}

/// Partitions are content namespacing, so the same bytes written by two tenants land on one
/// address. Whether reading it back under a partition that never wrote it succeeds is the
/// store's decision, not the caller's: a single-tenant client serves it, and a store holding
/// content for everyone must not.
#[tokio::test]
async fn a_cross_partition_read_is_refused_only_by_an_isolated_store() {
    let stored_under = Partition::from([0x01; 16]);
    let asked_under = Partition::from([0x02; 16]);
    let payload = Bytes::from_static(b"content addressed by hash alone");
    let address = Address {
        hash: hash::hash_slice(payload.as_ref()),
        context: Context::from([0x03; 16]),
    };
    let fragment = Fragment {
        flags: FragmentFlags::PayloadStoredLocal.bits(),
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    };

    for isolate_partitions in [false, true] {
        let (_dir, store) = store_with_isolation(isolate_partitions).await;
        store
            .clone()
            .put(
                stored_under,
                address,
                fragment,
                Some(payload.clone()),
                false,
            )
            .await
            .expect("put under the owning partition");

        let result = load_fragment(
            store,
            asked_under,
            address,
            ReadOptions::default().no_remote(),
            None,
        )
        .await;

        if isolate_partitions {
            assert!(
                matches!(result, Err(StorageError::AddressNotFound(_))),
                "an isolated store served content from another partition"
            );
        } else {
            let (_fragment, served) = result.expect("a non-isolated store serves by hash");
            assert_eq!(served, payload);
        }
    }
}

/// A defragment that fails part-way must not leave its temporary behind. The temporary is
/// sized to the whole content before any of it arrives and is excluded from staging, so an
/// orphan is a full-size file that no `status` will ever mention.
#[tokio::test]
async fn a_failed_defragment_leaves_no_temporary_file() {
    use lore_storage::FragmentReference;
    use zerocopy::IntoBytes;

    let (dir, store) = make_test_store().await;
    let partition = Partition::from([0xA1; 16]);
    let context = Context::from([0xA1; 16]);

    // A list naming content that was never stored: the walk fails once it tries to load it.
    let missing = FragmentReference {
        hash: hash::hash_slice(b"never stored"),
        offset_content: 0,
    };
    let refs_payload = Bytes::copy_from_slice([missing].as_bytes());
    let root_address = Address {
        hash: hash::hash_slice(refs_payload.as_ref()),
        context,
    };
    store
        .clone()
        .put(
            partition,
            root_address,
            Fragment {
                flags: FragmentFlags::PayloadFragmented.bits(),
                size_payload: refs_payload.len() as u32,
                size_content: 64,
            },
            Some(refs_payload),
            false,
        )
        .await
        .expect("put root list");

    let target = PathBuf::from(dir.as_ref()).join("content.bin");
    let result = read_into_file(
        store,
        partition,
        root_address,
        target.as_path(),
        ".~loretemp",
        None,
        ReadOptions::default().no_remote(),
        None,
    )
    .await;

    assert!(result.is_err(), "a list naming missing content cannot read");

    let leftovers: Vec<String> = std::fs::read_dir(dir.as_ref())
        .expect("read temp dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".~loretemp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temporary files left behind: {leftovers:?}"
    );
}

/// The four shapes a file read writes: one fragment, the empty content, a list staged beside the
/// target, and a list written in place. Each is a name, an address, the content it reads to, and
/// `direct_write`.
async fn file_read_shapes(
    store: &Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
) -> [(&'static str, Address, Vec<u8>, bool); 4] {
    let whole = Bytes::from((0u8..=255).collect::<Vec<u8>>());
    let whole_address = put_whole(store, partition, context, &whole).await;
    let (list_address, list_content) = put_two_leaf_list(store, partition, context).await;
    let empty = Address {
        hash: Hash::default(),
        context,
    };
    [
        ("whole", whole_address, whole.to_vec(), false),
        ("empty", empty, Vec::new(), false),
        ("staged", list_address, list_content.clone(), false),
        ("in-place", list_address, list_content, true),
    ]
}

/// A file read asked to create its parent lands below directories that are not there, one level
/// or several deep, and leaves no temporary file.
#[tokio::test]
async fn a_file_read_creating_its_parent_creates_the_missing_directories() {
    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-read-missing-parent-");
    let partition = Partition::from([0xA2; 16]);
    let context = Context::from([0xA2; 16]);

    for (shape, address, content, direct_write) in
        file_read_shapes(&store, partition, context).await
    {
        for parent in ["one", "two/levels/deep"] {
            let directory = PathBuf::from(target_dir.as_ref()).join(shape).join(parent);
            let target = directory.join("content.bin");
            let options = ReadOptions {
                direct_write,
                ..no_remote().creating_parent()
            };
            read_into_file(
                store.clone(),
                partition,
                address,
                target.as_path(),
                ".~loretemp",
                None,
                options,
                None,
            )
            .await
            .unwrap_or_else(|err| panic!("{shape} below {parent}: {err}"));

            assert_eq!(
                std::fs::read(&target).expect("read the target"),
                content,
                "{shape} below {parent} holds the content"
            );
            let names: Vec<_> = std::fs::read_dir(&directory)
                .expect("list the created directory")
                .map(|entry| entry.expect("an entry").file_name())
                .collect();
            assert_eq!(
                names,
                ["content.bin"],
                "{shape} below {parent} leaves no temporary file"
            );
        }
    }
}

/// A file read asked to create its parent below a file, over which no directory can be created,
/// fails at once rather than retrying, and leaves the file as it was.
#[tokio::test]
async fn a_file_read_creating_its_parent_below_a_file_fails_at_once() {
    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-read-file-parent-");
    let partition = Partition::from([0xA3; 16]);
    let context = Context::from([0xA3; 16]);
    let blocker = PathBuf::from(target_dir.as_ref()).join("blocker");
    std::fs::write(&blocker, b"kept").expect("write the blocking file");
    let target = blocker.join("content.bin");

    for (shape, address, _content, direct_write) in
        file_read_shapes(&store, partition, context).await
    {
        let options = ReadOptions {
            direct_write,
            ..no_remote().creating_parent()
        };
        let read = read_into_file(
            store.clone(),
            partition,
            address,
            target.as_path(),
            ".~loretemp",
            None,
            options,
            None,
        );
        let result = tokio::time::timeout(Duration::from_secs(5), read)
            .await
            .unwrap_or_else(|_| panic!("{shape} fails without waiting out the retries"));

        assert!(result.is_err(), "{shape} cannot land below a file");
        assert_eq!(
            std::fs::read(&blocker).expect("read the blocking file"),
            b"kept",
            "{shape} leaves the file as it was"
        );
    }
}

/// A file read not asked to create its parent creates no directory: it keeps retrying the open
/// of the missing one.
#[tokio::test]
async fn a_file_read_leaves_a_missing_parent_missing() {
    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-read-no-parent-");
    let partition = Partition::from([0xA4; 16]);
    let context = Context::from([0xA4; 16]);
    let whole = Bytes::from_static(b"whole content");
    let address = put_whole(&store, partition, context, &whole).await;
    let parent = PathBuf::from(target_dir.as_ref()).join("missing");
    let target = parent.join("content.bin");

    let read = read_into_file(
        store,
        partition,
        address,
        target.as_path(),
        ".~loretemp",
        None,
        no_remote(),
        None,
    );
    tokio::time::timeout(Duration::from_millis(300), read)
        .await
        .expect_err("the read keeps retrying the open of a missing directory");

    assert!(!parent.exists(), "the read created the missing directory");
}

/// Reads asked to create their parent that run at once into one missing directory each land
/// there, whichever of them creates it, and leave no temporary file.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_file_reads_creating_one_parent_all_land() {
    const READS: usize = 16;

    let (_store_dir, store) = make_test_store().await;
    let target_dir = TempDir::new("lore-storage-read-shared-parent-");
    let partition = Partition::from([0xA5; 16]);
    let context = Context::from([0xA5; 16]);
    let whole = Bytes::from((0u8..=255).collect::<Vec<u8>>());
    let whole_address = put_whole(&store, partition, context, &whole).await;
    let (list_address, list_content) = put_two_leaf_list(&store, partition, context).await;
    let directory = PathBuf::from(target_dir.as_ref())
        .join("shared")
        .join("parent");

    let mut reads = tokio::task::JoinSet::new();
    for index in 0..READS {
        let (address, content) = if index % 2 == 0 {
            (whole_address, whole.to_vec())
        } else {
            (list_address, list_content.clone())
        };
        let store = store.clone();
        let target = directory.join(format!("content-{index}.bin"));
        lore_base::lore_spawn!(reads, async move {
            let read = read_into_file(
                store,
                partition,
                address,
                target.as_path(),
                ".~loretemp",
                None,
                no_remote().creating_parent(),
                None,
            )
            .await;
            (target, content, read)
        });
    }
    while let Some(joined) = reads.join_next().await {
        let (target, content, read) = joined.expect("the read task completes");
        read.unwrap_or_else(|err| panic!("{}: {err}", target.display()));
        assert_eq!(
            std::fs::read(&target).expect("read the target"),
            content,
            "{} holds the content",
            target.display()
        );
    }

    let mut names: Vec<String> = std::fs::read_dir(&directory)
        .expect("list the shared directory")
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    let mut expected: Vec<String> = (0..READS)
        .map(|index| format!("content-{index}.bin"))
        .collect();
    expected.sort();
    assert_eq!(
        names, expected,
        "the directory holds every target and no temporary file"
    );
}

/// Regression for the tracker-dispatched read-after-write race: a reader
/// that arrives while a leader holds the in-flight guard must wait for the
/// terminal store entry instead of returning `AddressNotFound`. This mirrors
/// the path that `weave_history` takes when it loads the delta block that
/// `generate_delta_block` just handed to the tracker.
#[tokio::test(flavor = "multi_thread")]
async fn load_fragment_waits_for_in_flight_leader() {
    let (_dir, store) = make_test_store().await;
    let (partition, address, fragment, payload) = make_input(0xDE);

    let guard = try_acquire_in_flight(partition, address).expect("no contention in fresh test");

    let reader_store = store.clone();
    let reader = lore_base::lore_spawn!(async move {
        load_fragment(
            reader_store,
            partition,
            address,
            ReadOptions::default(),
            None,
        )
        .await
    });

    // Give the reader a real chance to observe the in-flight entry and
    // park itself on the cancellation token rather than blaze through.
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        !reader.is_finished(),
        "reader must not finish before the leader writes and drops its guard"
    );

    store
        .clone()
        .put(partition, address, fragment, Some(payload.clone()), false)
        .await
        .expect("leader writes terminal entry");
    drop(guard);

    let (loaded_fragment, loaded_payload) = reader
        .await
        .expect("reader task joined")
        .expect("reader observes terminal entry after leader completes");
    assert_eq!(loaded_fragment.size_payload, fragment.size_payload);
    assert_eq!(loaded_payload.as_ref(), payload.as_ref());
}

/// A load whose content is local returns it at once while a store of the same address is in
/// flight: the store publishes an entry only once its payload is written, and the address
/// names the content.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_hit_never_waits_on_a_store_in_flight() {
    let (_dir, store) = make_test_store().await;
    let (partition, address, fragment, payload) = make_input(0xC3);
    store
        .clone()
        .put(partition, address, fragment, Some(payload.clone()), false)
        .await
        .expect("put the payload locally");
    let guard = try_acquire_in_flight(partition, address).expect("no contention in fresh test");

    let (_, loaded) = tokio::time::timeout(
        Duration::from_secs(2),
        load_fragment(store, partition, address, ReadOptions::default(), None),
    )
    .await
    .expect("the local hit waited on the store in flight")
    .expect("the local hit");
    assert_eq!(loaded, payload);
    drop(guard);
}

/// A load that must never wait answers a local miss at once while a store of the same address
/// is in flight.
#[tokio::test(flavor = "multi_thread")]
async fn a_never_waiting_miss_answers_at_once_while_a_store_is_in_flight() {
    let (_dir, store) = make_test_store().await;
    let (partition, address, _fragment, _payload) = make_input(0xC4);
    let guard = try_acquire_in_flight(partition, address).expect("no contention in fresh test");

    let loaded = tokio::time::timeout(
        Duration::from_secs(2),
        load_fragment(
            store,
            partition,
            address,
            ReadOptions::default().no_remote().never_wait(),
            None,
        ),
    )
    .await
    .expect("the miss waited on the store in flight");
    assert!(
        matches!(loaded, Err(StorageError::AddressNotFound(_))),
        "a never-waiting miss answers AddressNotFound, got {loaded:?}"
    );
    drop(guard);
}

/// A miss probes the local store once when no store of the address lands meanwhile. Another
/// test's store in the same bucket can move the count; a run where it does is tried again.
#[tokio::test(flavor = "multi_thread")]
async fn a_miss_with_no_store_landing_probes_once() {
    let (_dir, inner) = make_test_store().await;
    let (partition, address, _fragment, _payload) = make_input(0xA5);
    for _ in 0..100 {
        let (store, counts) = CountingReadStore::wrap(inner.clone());
        let landed = lore_storage::write::stores_landed(address);
        let loaded = load_fragment(
            store,
            partition,
            address,
            ReadOptions::default().no_remote(),
            None,
        )
        .await;
        assert!(
            matches!(loaded, Err(StorageError::AddressNotFound(_))),
            "got {loaded:?}"
        );
        if lore_storage::write::stores_landed(address) == landed {
            assert_eq!(
                counts.gets(),
                1,
                "a miss with no store landing probed again"
            );
            return;
        }
    }
    panic!("a store in the address's bucket landed during every one of 100 loads");
}

/// A store of the address that lands while a reader's first probe runs, after that probe
/// missed and before the reader looks for it in flight, is found by a second probe.
#[tokio::test(flavor = "multi_thread")]
async fn a_store_landing_during_the_probe_is_found_by_a_second_probe() {
    let (_dir, inner) = make_test_store().await;
    let (partition, address, fragment, payload) = make_input(0xA6);
    let guard = try_acquire_in_flight(partition, address).expect("no contention in fresh test");
    let (store, counts) = CountingReadStore::with_hooks(
        inner,
        ReadHooks {
            landing: Some((partition, address, fragment, payload.clone(), guard)),
            ..ReadHooks::default()
        },
    );

    let (_, loaded) = tokio::time::timeout(
        Duration::from_secs(2),
        load_fragment(
            store,
            partition,
            address,
            ReadOptions::default().no_remote(),
            None,
        ),
    )
    .await
    .expect("the load waited on a store its own probe lands")
    .expect("the second probe finds the store that landed");
    assert_eq!(loaded, payload);
    assert_eq!(counts.gets(), 2);
}

/// [`a_miss_with_no_store_landing_probes_once`], for a read into a slice.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_into_missing_with_no_store_landing_probes_once() {
    let (_dir, inner) = make_test_store().await;
    let (partition, address, _fragment, payload) = make_input(0xC5);
    for _ in 0..100 {
        let (store, counts) = CountingReadStore::wrap(inner.clone());
        let landed = lore_storage::write::stores_landed(address);
        let mut slice = vec![0u8; payload.len()];
        let read = read_into(
            store,
            partition,
            address,
            None,
            &mut slice,
            ReadOptions::default().no_remote(),
            None,
        )
        .await;
        assert!(
            matches!(read, Err(StorageError::AddressNotFound(_))),
            "got {read:?}"
        );
        if lore_storage::write::stores_landed(address) == landed {
            assert_eq!(
                counts.probes(),
                1,
                "a miss with no store landing probed again"
            );
            return;
        }
    }
    panic!("a store in the address's bucket landed during every one of 100 reads");
}

/// [`a_miss_with_no_store_landing_probes_once`], for a read into a buffer.
#[tokio::test(flavor = "multi_thread")]
async fn a_buffer_read_missing_with_no_store_landing_probes_once() {
    let (_dir, inner) = make_test_store().await;
    let (partition, address, _fragment, payload) = make_input(0xC7);
    for _ in 0..100 {
        let (store, counts) = CountingReadStore::wrap(inner.clone());
        let landed = lore_storage::write::stores_landed(address);
        let read = read_into_vec(
            store,
            partition,
            address,
            None,
            payload.len(),
            ReadOptions::default().no_remote(),
        )
        .await;
        assert!(
            matches!(read, Err(StorageError::AddressNotFound(_))),
            "got {read:?}"
        );
        if lore_storage::write::stores_landed(address) == landed {
            assert_eq!(
                counts.probes(),
                1,
                "a miss with no store landing probed again"
            );
            return;
        }
    }
    panic!("a store in the address's bucket landed during every one of 100 reads");
}

/// [`a_store_landing_during_the_probe_is_found_by_a_second_probe`], for a read into a slice.
#[tokio::test(flavor = "multi_thread")]
async fn a_store_landing_during_a_read_into_probe_is_found_by_a_second_probe() {
    let (_dir, inner) = make_test_store().await;
    let (partition, address, fragment, payload) = make_input(0xC6);
    let guard = try_acquire_in_flight(partition, address).expect("no contention in fresh test");
    let (store, counts) = CountingReadStore::with_hooks(
        inner,
        ReadHooks {
            landing: Some((partition, address, fragment, payload.clone(), guard)),
            ..ReadHooks::default()
        },
    );

    let mut slice = vec![0u8; payload.len()];
    tokio::time::timeout(
        Duration::from_secs(2),
        read_into(
            store,
            partition,
            address,
            None,
            &mut slice,
            ReadOptions::default().no_remote(),
            None,
        ),
    )
    .await
    .expect("the read waited on a store its own probe lands")
    .expect("the second probe finds the store that landed");
    assert_eq!(slice, payload.as_ref());
    assert_eq!(counts.probes(), 2);
}

/// [`a_store_landing_during_the_probe_is_found_by_a_second_probe`], for a read into a buffer with
/// no remote in reach.
#[tokio::test(flavor = "multi_thread")]
async fn a_store_landing_during_a_buffer_read_probe_is_found_by_a_second_probe() {
    let (_dir, inner) = make_test_store().await;
    let (partition, address, fragment, payload) = make_input(0xC8);
    let guard = try_acquire_in_flight(partition, address).expect("no contention in fresh test");
    let (store, counts) = CountingReadStore::with_hooks(
        inner,
        ReadHooks {
            landing: Some((partition, address, fragment, payload.clone(), guard)),
            ..ReadHooks::default()
        },
    );

    let mut buffer = vec![0u8; payload.len()];
    // SAFETY: the buffer outlives the read and nothing else touches it.
    let mut dst = unsafe { CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
    tokio::time::timeout(
        Duration::from_secs(2),
        read_into_buffer(
            store,
            partition,
            address,
            None,
            &mut dst,
            ReadOptions::default().no_remote(),
            None,
        ),
    )
    .await
    .expect("the read waited on a store its own probe lands")
    .expect("the second probe finds the store that landed");
    assert_eq!(buffer.as_slice(), payload.as_ref());
    assert_eq!(counts.probes(), 2);
}

/// A placement the store answers `SlowDown` is no miss: the read probes again through the load that
/// retries, rather than reporting the payload missing or fetching it from a remote in reach.
#[tokio::test]
async fn a_placement_answered_slow_down_is_read_through_the_retrying_load() {
    let (_dir, inner) = make_test_store().await;
    let partition = Partition::from([0x9c; 16]);
    let payload = Bytes::from((0u8..64).map(|byte| byte ^ 0x9c).collect::<Vec<u8>>());
    let address = put_whole(&inner, partition, Context::from([0x9c; 16]), &payload).await;

    for session in [None, stand_in_session()] {
        let (store, counts) = CountingReadStore::with_hooks(
            inner.clone(),
            ReadHooks {
                placement_error: Some(StoreError::from(lore_storage::errors::SlowDown)),
                ..ReadHooks::default()
            },
        );
        let mut buffer = vec![0u8; payload.len()];
        // SAFETY: the buffer outlives the read and nothing else touches it.
        let mut dst = unsafe { CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
        read_into_buffer(
            store,
            partition,
            address,
            None,
            &mut dst,
            ReadOptions::default(),
            session,
        )
        .await
        .expect("the retrying load reads the payload");

        assert_eq!(buffer.as_slice(), payload.as_ref());
        assert_eq!((counts.gets_into(), counts.gets()), (1, 1));
    }
}

/// A placement that fails for any other reason than `SlowDown`, `Oversized` or `NotSupported` is a
/// miss: the read neither probes the local store again nor reports anything but the miss, with no
/// remote in reach.
#[tokio::test]
async fn a_placement_failing_otherwise_is_a_miss() {
    let (_dir, inner) = make_test_store().await;
    let partition = Partition::from([0xa0; 16]);
    let payload = Bytes::from((0u8..64).map(|byte| byte ^ 0xa0).collect::<Vec<u8>>());
    let address = put_whole(&inner, partition, Context::from([0xa0; 16]), &payload).await;
    let (store, counts) = CountingReadStore::with_hooks(
        inner,
        ReadHooks {
            placement_error: Some(StoreError::internal("a store failing the placement")),
            ..ReadHooks::default()
        },
    );

    let read = read_into_vec(
        store,
        partition,
        address,
        None,
        payload.len(),
        ReadOptions::default().no_remote(),
    )
    .await;

    assert!(
        matches!(read, Err(StorageError::AddressNotFound(_))),
        "got {read:?}"
    );
    assert_eq!(counts.probes(), 1, "the failed placement was probed again");
}

/// [`a_never_waiting_miss_answers_at_once_while_a_store_is_in_flight`], for a read into a slice.
#[tokio::test(flavor = "multi_thread")]
async fn a_never_waiting_read_into_answers_a_miss_at_once_while_a_store_is_in_flight() {
    let (_dir, store) = make_test_store().await;
    let (partition, address, _fragment, payload) = make_input(0xE5);
    let guard = try_acquire_in_flight(partition, address).expect("no contention in fresh test");

    let mut slice = vec![0u8; payload.len()];
    let read = tokio::time::timeout(
        Duration::from_secs(2),
        read_into(
            store,
            partition,
            address,
            None,
            &mut slice,
            ReadOptions::default().no_remote().never_wait(),
            None,
        ),
    )
    .await
    .expect("the miss waited on the store in flight");
    assert!(
        matches!(read, Err(StorageError::AddressNotFound(_))),
        "a never-waiting miss answers AddressNotFound, got {read:?}"
    );
    drop(guard);
}

/// When the leader drops its guard without writing (upload failed, task
/// aborted), the reader must not hang — it should surface the same
/// `AddressNotFound` it would have seen without the in-flight wait.
#[tokio::test(flavor = "multi_thread")]
async fn load_fragment_returns_not_found_when_leader_drops_without_writing() {
    let (_dir, store) = make_test_store().await;
    let (partition, address, _fragment, _payload) = make_input(0xAD);

    let guard = try_acquire_in_flight(partition, address).expect("no contention in fresh test");

    let reader_store = store.clone();
    let reader = lore_base::lore_spawn!(async move {
        load_fragment(
            reader_store,
            partition,
            address,
            ReadOptions::default(),
            None,
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(20)).await;
    drop(guard);

    let err = reader
        .await
        .expect("reader task joined")
        .expect_err("reader must not invent a fragment when leader wrote nothing");
    assert!(
        matches!(err, StorageError::AddressNotFound(_)),
        "expected AddressNotFound, got {err:?}"
    );
}

mod resolve_range {
    use super::*;

    #[test]
    fn none_is_the_whole_content() {
        assert_eq!(resolve_content_range(None, 100), 0..100);
    }

    #[test]
    fn an_inside_range_is_passed_through() {
        assert_eq!(resolve_content_range(Some(10..50), 100), 10..50);
    }

    #[test]
    fn an_end_past_the_content_is_clamped() {
        assert_eq!(resolve_content_range(Some(80..1000), 100), 80..100);
    }

    /// A start past the end is empty rather than an error: the storage layer has no way to
    /// tell a caller apart from a mistaken one, so it serves what exists and leaves the
    /// judgement to the API boundary, which knows what was asked for.
    #[test]
    fn a_start_past_the_content_is_empty() {
        assert_eq!(resolve_content_range(Some(200..300), 100), 100..100);
    }

    /// An inverted range would panic `Bytes::slice`, so it resolves to empty instead. It
    /// cannot arrive from the C API — `offset`/`length` can only describe a forward range —
    /// but `read` is a Rust entry point of its own.
    #[test]
    #[allow(clippy::reversed_empty_ranges, reason = "the input under test")]
    fn an_inverted_range_is_empty_rather_than_a_panic() {
        let resolved = resolve_content_range(Some(60..20), 100);
        assert!(resolved.is_empty());
        assert!(resolved.start <= resolved.end);
        assert_eq!(Bytes::from_static(&[0u8; 100]).slice(resolved).len(), 0);
    }
}

/// A two-level fragment tree over four 100-byte leaves, for the pruning tests.
///
/// Returns the root address and every leaf payload concatenated. `store_all` false leaves
/// the second subtree — its list *and* its leaves — out of the store, so a read that
/// touches it fails and one that prunes it succeeds. That is the difference between
/// fetching less and walking less, and only the absent subtree can tell them apart.
mod tree {
    use lore_storage::FragmentReference;
    use zerocopy::IntoBytes;

    use super::*;

    pub(super) const LEAF: usize = 100;
    pub(super) const LEAVES: usize = 4;
    pub(super) const CONTENT: usize = LEAF * LEAVES;

    pub(super) async fn put_leaf(
        store: &Arc<dyn ImmutableStore>,
        partition: Partition,
        context: Context,
        payload: Vec<u8>,
    ) -> Address {
        let address = Address {
            hash: hash::hash_slice(&payload),
            context,
        };
        let fragment = Fragment {
            flags: 0,
            size_payload: payload.len() as u32,
            size_content: payload.len() as u64,
        };
        store
            .clone()
            .put(
                partition,
                address,
                fragment,
                Some(Bytes::from(payload)),
                false,
            )
            .await
            .expect("put leaf");
        address
    }

    /// Build a list fragment. Returns its address whether or not it was stored, so a
    /// caller can reference a list the store does not hold.
    pub(super) async fn put_list(
        store: &Arc<dyn ImmutableStore>,
        partition: Partition,
        context: Context,
        refs: &[FragmentReference],
        size_content: u64,
        store_it: bool,
    ) -> Address {
        let payload = Bytes::copy_from_slice(refs.as_bytes());
        let address = Address {
            hash: hash::hash_slice(payload.as_ref()),
            context,
        };
        if store_it {
            store
                .clone()
                .put(
                    partition,
                    address,
                    Fragment {
                        flags: FragmentFlags::PayloadFragmented.bits(),
                        size_payload: payload.len() as u32,
                        size_content,
                    },
                    Some(payload),
                    false,
                )
                .await
                .expect("put list");
        }
        address
    }

    pub(super) async fn build(
        store: &Arc<dyn ImmutableStore>,
        partition: Partition,
        context: Context,
        store_second_subtree: bool,
    ) -> (Address, Vec<u8>) {
        let payloads: Vec<Vec<u8>> = (0..LEAVES)
            .map(|leaf| vec![0xA0u8 + leaf as u8; LEAF])
            .collect();

        let mut leaves = Vec::with_capacity(LEAVES);
        for (leaf, payload) in payloads.iter().enumerate() {
            let in_second_subtree = leaf >= LEAVES / 2;
            if in_second_subtree && !store_second_subtree {
                // Referenced but absent: reaching it is a read error.
                leaves.push(Address {
                    hash: hash::hash_slice(payload),
                    context,
                });
                continue;
            }
            leaves.push(put_leaf(store, partition, context, payload.clone()).await);
        }

        let reference = |index: usize| FragmentReference {
            hash: leaves[index].hash,
            offset_content: (index * LEAF) as u64,
        };

        let sub_a = put_list(
            store,
            partition,
            context,
            &[reference(0), reference(1)],
            (2 * LEAF) as u64,
            true,
        )
        .await;
        let sub_b = put_list(
            store,
            partition,
            context,
            &[reference(2), reference(3)],
            (2 * LEAF) as u64,
            store_second_subtree,
        )
        .await;

        let root = put_list(
            store,
            partition,
            context,
            &[
                FragmentReference {
                    hash: sub_a.hash,
                    offset_content: 0,
                },
                FragmentReference {
                    hash: sub_b.hash,
                    offset_content: (2 * LEAF) as u64,
                },
            ],
            CONTENT as u64,
            true,
        )
        .await;

        (root, payloads.concat())
    }
}

/// A fragment list whose offsets are out of order fails the read: one leaves part of the content
/// in no entry, which a read that succeeded would hand back as bytes nothing wrote, and one
/// starting past zero ends in an entry covering the whole range, which would hand back the bytes
/// of the wrong leaf.
#[tokio::test(flavor = "multi_thread")]
async fn a_fragment_list_with_offsets_out_of_order_fails_the_read() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x3a; 16]);
    let context = Context::from([0x3a; 16]);
    let lists: [[(u8, usize, u64); 3]; 2] = [
        [(0xB0, 100, 0), (0xB1, 100, 100), (0xB2, 100, 50)],
        [(0xC0, 50, 100), (0xC1, 50, 150), (0xC2, 150, 100)],
    ];
    for entries in lists {
        let mut references = vec![];
        for (fill, size, offset_content) in entries {
            let leaf = tree::put_leaf(&store, partition, context, vec![fill; size]).await;
            references.push(lore_storage::FragmentReference {
                hash: leaf.hash,
                offset_content,
            });
        }
        let list = tree::put_list(&store, partition, context, &references, 150, true).await;

        let result = read(store.clone(), partition, list, None, no_remote(), None).await;
        assert!(result.is_err(), "{entries:?}: {result:?}");
    }
}

/// A three-level tree over eight 100-byte leaves, built but not stored.
///
/// ```text
/// root ─┬─ mid[0] ─┬─ sub[0] ─┬─ leaf[0]   0..100
///       │          │          └─ leaf[1] 100..200
///       │          └─ sub[1] ─┬─ leaf[2] 200..300
///       │                     └─ leaf[3] 300..400
///       └─ mid[1] ─┬─ sub[2] ─┬─ leaf[4] 400..500
///                  │          └─ leaf[5] 500..600
///                  └─ sub[3] ─┬─ leaf[6] 600..700
///                             └─ leaf[7] 700..800
/// ```
///
/// Handing every piece back unstored is what lets a test put exactly the fragments a range
/// should reach and nothing else: a walk that reached past them fails the read outright
/// rather than merely doing more work than it needed to.
mod three_level {
    use lore_storage::FragmentReference;
    use zerocopy::IntoBytes;

    use super::*;

    pub(super) const LEAF: usize = 100;
    pub(super) const CONTENT: usize = LEAF * 8;

    pub(super) struct Piece {
        pub(super) address: Address,
        fragment: Fragment,
        payload: Bytes,
    }

    impl Piece {
        fn leaf(context: Context, payload: &[u8]) -> Self {
            let payload = Bytes::copy_from_slice(payload);
            Self {
                address: Address {
                    hash: hash::hash_slice(payload.as_ref()),
                    context,
                },
                fragment: Fragment {
                    flags: 0,
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                },
                payload,
            }
        }

        fn list(context: Context, children: &[(Address, u64)], size_content: u64) -> Self {
            let entries: Vec<FragmentReference> = children
                .iter()
                .map(|(address, offset_content)| FragmentReference {
                    hash: address.hash,
                    offset_content: *offset_content,
                })
                .collect();
            let payload = Bytes::copy_from_slice(entries.as_bytes());
            Self {
                address: Address {
                    hash: hash::hash_slice(payload.as_ref()),
                    context,
                },
                fragment: Fragment {
                    flags: FragmentFlags::PayloadFragmented.bits(),
                    size_payload: payload.len() as u32,
                    size_content,
                },
                payload,
            }
        }

        pub(super) async fn put(&self, store: &Arc<dyn ImmutableStore>, partition: Partition) {
            store
                .clone()
                .put(
                    partition,
                    self.address,
                    self.fragment,
                    Some(self.payload.clone()),
                    false,
                )
                .await
                .expect("put piece");
        }
    }

    pub(super) struct Tree {
        pub(super) root: Piece,
        pub(super) mid: Vec<Piece>,
        pub(super) sub: Vec<Piece>,
        pub(super) leaf: Vec<Piece>,
        pub(super) content: Vec<u8>,
    }

    pub(super) fn build(context: Context) -> Tree {
        let content: Vec<u8> = (0..CONTENT)
            .map(|byte| 0xA0 + (byte / LEAF) as u8)
            .collect();

        let leaf: Vec<Piece> = (0..8)
            .map(|index| Piece::leaf(context, &content[index * LEAF..(index + 1) * LEAF]))
            .collect();

        let sub: Vec<Piece> = (0..4)
            .map(|index| {
                let first = 2 * index;
                Piece::list(
                    context,
                    &[
                        (leaf[first].address, (first * LEAF) as u64),
                        (leaf[first + 1].address, ((first + 1) * LEAF) as u64),
                    ],
                    (2 * LEAF) as u64,
                )
            })
            .collect();

        let mid: Vec<Piece> = (0..2)
            .map(|index| {
                let first = 2 * index;
                Piece::list(
                    context,
                    &[
                        (sub[first].address, (first * 2 * LEAF) as u64),
                        (sub[first + 1].address, ((first + 1) * 2 * LEAF) as u64),
                    ],
                    (4 * LEAF) as u64,
                )
            })
            .collect();

        let root = Piece::list(
            context,
            &[(mid[0].address, 0), (mid[1].address, (4 * LEAF) as u64)],
            CONTENT as u64,
        );

        Tree {
            root,
            mid,
            sub,
            leaf,
            content,
        }
    }
}

fn no_remote() -> ReadOptions {
    ReadOptions::default().no_remote()
}

/// `read` reports the whole content's fragment alongside the range's bytes. A caller
/// cannot derive `size_content` from a ranged buffer, so the fragment is how it learns
/// what it read part of.
#[tokio::test(flavor = "multi_thread")]
async fn read_reports_the_whole_size_alongside_a_ranged_buffer() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x31; 16]);
    let context = Context::from([0x31; 16]);
    let (root, content) = tree::build(&store, partition, context, true).await;

    let (fragment, bytes) = read(
        store,
        partition,
        Address {
            hash: root.hash,
            context,
        },
        Some(150..250),
        no_remote(),
        None,
    )
    .await
    .expect("ranged read");

    assert_eq!(fragment.size_content, tree::CONTENT as u64);
    assert_eq!(bytes.as_ref(), &content[150..250]);
}

/// The subtree the range misses is never walked, so a tree missing it entirely still
/// reads. The control below is what makes this a claim about pruning rather than about
/// the tree happening to be readable.
#[tokio::test(flavor = "multi_thread")]
async fn a_ranged_read_never_walks_a_subtree_outside_the_range() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x32; 16]);
    let context = Context::from([0x32; 16]);
    let (root, content) = tree::build(&store, partition, context, false).await;
    let address = Address {
        hash: root.hash,
        context,
    };

    let (_fragment, bytes) = read(
        store.clone(),
        partition,
        address,
        Some(50..150),
        no_remote(),
        None,
    )
    .await
    .expect("a range inside the stored subtree reads");
    assert_eq!(bytes.as_ref(), &content[50..150]);

    read(store, partition, address, None, no_remote(), None)
        .await
        .expect_err("the whole content is not readable, so the range really was pruned");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ranged_stream_delivers_exactly_the_range() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x33; 16]);
    let context = Context::from([0x33; 16]);
    let (root, content) = tree::build(&store, partition, context, true).await;

    let (sender, mut receiver) = tokio::sync::mpsc::channel::<Result<Bytes, StorageError>>(16);
    let (fragment, streamed) = read_stream(
        store,
        partition,
        Address {
            hash: root.hash,
            context,
        },
        Some(120..330),
        no_remote(),
        sender,
        None,
    )
    .await
    .expect("ranged stream");

    assert_eq!(fragment.size_content, tree::CONTENT as u64);
    assert_eq!(streamed, 120..330);

    let mut delivered = Vec::new();
    while let Some(chunk) = receiver.recv().await {
        let chunk = chunk.expect("stream chunk");
        delivered.extend_from_slice(chunk.as_ref());
    }
    assert_eq!(delivered, content[120..330]);
}

/// **A stream read keeps its store claimed past the span it ran in, until the stream ends.** The
/// read returns once it has spawned the task that delivers the stream, so it claims the store
/// before the spawn: the span's end then leaves the flock held rather than releasing it before
/// the task first runs, which on a single-threaded runtime is after the span has ended.
#[tokio::test]
async fn a_stream_read_keeps_the_flock_past_its_span_until_the_stream_ends() {
    let dir = TempDir::new("lore-storage-read-claim-");
    let local = LocalImmutableStore::new(
        Some(PathBuf::from(dir.as_ref())),
        ImmutableStoreSettings::default(),
    )
    .await
    .expect("create test store");
    let lock = local.lock.clone().expect("a disk-backed store has a lock");
    let store: Arc<dyn ImmutableStore> = local;
    let partition = Partition::from([0x3a; 16]);
    let context = Context::from([0x3a; 16]);
    let (root, content) = tree::build(&store, partition, context, true).await;
    store.clone().flush(true).await.expect("flush");
    assert!(!lock.is_held(), "nothing holds the flock before the read");

    let span = store.clone().hold_for_command().await.expect("span");
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<Result<Bytes, StorageError>>(1);
    read_stream(
        store,
        partition,
        Address {
            hash: root.hash,
            context,
        },
        None,
        no_remote(),
        sender,
        None,
    )
    .await
    .expect("stream");
    drop(span);
    assert!(
        lock.is_held(),
        "the task delivering the stream keeps the flock past the span"
    );

    let mut delivered = Vec::new();
    while let Some(chunk) = receiver.recv().await {
        delivered.extend_from_slice(chunk.expect("stream chunk").as_ref());
    }
    assert_eq!(delivered, content);
    tokio::time::timeout(Duration::from_secs(10), async {
        while lock.is_held() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the flock goes once the stream has ended");
}

/// The streaming path prunes the same way the buffered one does — it is a different sink
/// over the same walk, and this is the test that says so.
#[tokio::test(flavor = "multi_thread")]
async fn a_ranged_stream_never_walks_a_subtree_outside_the_range() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x34; 16]);
    let context = Context::from([0x34; 16]);
    let (root, content) = tree::build(&store, partition, context, false).await;

    let (sender, mut receiver) = tokio::sync::mpsc::channel::<Result<Bytes, StorageError>>(16);
    let (_fragment, streamed) = read_stream(
        store,
        partition,
        Address {
            hash: root.hash,
            context,
        },
        Some(0..200),
        no_remote(),
        sender,
        None,
    )
    .await
    .expect("a range inside the stored subtree streams");
    assert_eq!(streamed, 0..200);

    let mut delivered = Vec::new();
    while let Some(chunk) = receiver.recv().await {
        let chunk = chunk.expect("stream chunk");
        delivered.extend_from_slice(chunk.as_ref());
    }
    assert_eq!(delivered, content[0..200]);
}

/// Chunk boundaries follow the leaves, and the offsets a caller reconstructs from them
/// have to tile the range from its own start.
#[tokio::test(flavor = "multi_thread")]
async fn a_ranged_stream_clips_only_its_first_and_last_chunk() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x35; 16]);
    let context = Context::from([0x35; 16]);
    let (root, _content) = tree::build(&store, partition, context, true).await;

    let (sender, mut receiver) = tokio::sync::mpsc::channel::<Result<Bytes, StorageError>>(16);
    let (_fragment, streamed) = read_stream(
        store,
        partition,
        Address {
            hash: root.hash,
            context,
        },
        Some(50..350),
        no_remote(),
        sender,
        None,
    )
    .await
    .expect("ranged stream");

    let mut sizes = Vec::new();
    while let Some(chunk) = receiver.recv().await {
        let chunk = chunk.expect("stream chunk");
        sizes.push(chunk.len());
    }
    // Leaves are 100 bytes at 0/100/200/300; 50..350 clips the first and last.
    assert_eq!(sizes, vec![50, 100, 100, 50]);
    assert_eq!(
        sizes.iter().sum::<usize>() as u64,
        streamed.end - streamed.start
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_starting_past_the_content_delivers_nothing() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x36; 16]);
    let context = Context::from([0x36; 16]);
    let (root, _content) = tree::build(&store, partition, context, true).await;

    let (sender, mut receiver) = tokio::sync::mpsc::channel::<Result<Bytes, StorageError>>(16);
    let (fragment, streamed) = read_stream(
        store,
        partition,
        Address {
            hash: root.hash,
            context,
        },
        Some(tree::CONTENT..tree::CONTENT + 10),
        no_remote(),
        sender,
        None,
    )
    .await
    .expect("an empty range is not an error here");

    assert_eq!(fragment.size_content, tree::CONTENT as u64);
    assert!(streamed.is_empty());
    assert!(
        receiver.recv().await.is_none(),
        "nothing may be sent for an empty range, and the channel must close"
    );
}

/// The file holds the range and is sized to it, rather than being a sparse copy of the
/// content with the range in place.
#[tokio::test(flavor = "multi_thread")]
async fn a_ranged_read_into_file_writes_only_the_range() {
    let (dir, store) = make_test_store().await;
    let partition = Partition::from([0x37; 16]);
    let context = Context::from([0x37; 16]);
    let (root, content) = tree::build(&store, partition, context, true).await;

    let target = PathBuf::from(dir.as_ref()).join("ranged.bin");
    let (fragment, _metadata) = read_into_file(
        store,
        partition,
        Address {
            hash: root.hash,
            context,
        },
        target.as_path(),
        ".~loretemp",
        Some(120..330),
        no_remote(),
        None,
    )
    .await
    .expect("ranged read into file");

    assert_eq!(fragment.size_content, tree::CONTENT as u64);
    let on_disk = std::fs::read(&target).expect("read target");
    assert_eq!(on_disk, content[120..330]);
}

/// A range counts content bytes, not stored bytes. The two are the same for everything
/// else in this module, and differ exactly when a fragment is compressed — so this is the
/// one shape that can tell a content offset from a payload offset.
#[tokio::test(flavor = "multi_thread")]
async fn a_range_on_a_compressed_fragment_counts_content_bytes() {
    use lore_storage::compress::CompressionMode;

    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x39; 16]);
    let context = Context::from([0x39; 16]);

    // Compressible enough that the payload is meaningfully shorter than the content,
    // which is what makes the two offset bases distinguishable.
    let content: Vec<u8> = (0..4096).map(|index| (index / 64) as u8).collect();
    let plain = Fragment {
        flags: 0,
        size_payload: content.len() as u32,
        size_content: content.len() as u64,
    };
    let (fragment, payload) =
        lore_storage::compress::compress(plain, &content, CompressionMode::Lz4)
            .expect("compress test content");
    assert!(
        (payload.len() as u64) < fragment.size_content,
        "test needs a payload shorter than its content, got {} of {}",
        payload.len(),
        fragment.size_content,
    );

    let address = Address {
        hash: hash::hash_slice(&content),
        context,
    };
    store
        .clone()
        .put(partition, address, fragment, Some(payload), false)
        .await
        .expect("put compressed fragment");

    let (read_fragment, bytes) = read(
        store,
        partition,
        address,
        Some(1000..1200),
        no_remote(),
        None,
    )
    .await
    .expect("ranged read of compressed content");

    assert_eq!(read_fragment.size_content, content.len() as u64);
    assert_eq!(bytes.as_ref(), &content[1000..1200]);
}

/// A ranged read fetches the spine down to the leaves it needs and nothing else, three
/// levels deep.
///
/// The store holds exactly the five fragments the range reaches out of the tree's fifteen,
/// so this is not a claim that the walk *tends* to skip work — anything it reached for
/// beyond them is a missing address and a failed read. `250..320` lives in `leaf[2]`
/// (200..300) and `leaf[3]` (300..400), so the spine is root → `mid[0]` → `sub[1]`.
///
/// Both read paths are driven from the one sparse store because they agree on the set:
/// `read` prunes in `read_defragment`, `read_stream` and `read_into_file` prune in the
/// tree walker, and the level peeks the walker adds always land on entries the range
/// already wanted.
#[tokio::test(flavor = "multi_thread")]
async fn a_ranged_read_of_a_three_level_tree_touches_only_its_own_spine() {
    let (dir, store) = make_test_store().await;
    let partition = Partition::from([0x3A; 16]);
    let context = Context::from([0x3A; 16]);
    let tree = three_level::build(context);

    for piece in [
        &tree.root,
        &tree.mid[0],
        &tree.sub[1],
        &tree.leaf[2],
        &tree.leaf[3],
    ] {
        piece.put(&store, partition).await;
    }
    let address = tree.root.address;
    let expected = &tree.content[250..320];

    let (fragment, bytes) = read(
        store.clone(),
        partition,
        address,
        Some(250..320),
        no_remote(),
        None,
    )
    .await
    .expect("the spine the range needs is all it needs");
    assert_eq!(fragment.size_content, three_level::CONTENT as u64);
    assert_eq!(bytes.as_ref(), expected);

    let (sender, mut receiver) = tokio::sync::mpsc::channel::<Result<Bytes, StorageError>>(8);
    let (_fragment, streamed) = read_stream(
        store.clone(),
        partition,
        address,
        Some(250..320),
        no_remote(),
        sender,
        None,
    )
    .await
    .expect("the streaming walk prunes to the same spine");
    assert_eq!(streamed, 250..320);
    let mut delivered = Vec::new();
    while let Some(chunk) = receiver.recv().await {
        let chunk = chunk.expect("stream chunk");
        delivered.extend_from_slice(chunk.as_ref());
    }
    assert_eq!(delivered, expected);

    let target = PathBuf::from(dir.as_ref()).join("three-level.bin");
    read_into_file(
        store.clone(),
        partition,
        address,
        target.as_path(),
        ".~loretemp",
        Some(250..320),
        no_remote(),
        None,
    )
    .await
    .expect("the file walk prunes to the same spine");
    assert_eq!(std::fs::read(&target).expect("read target"), expected);

    // The controls: the pieces left out really are missing, so the successes above are
    // pruning rather than a tree that happens to be wholly readable.
    read(store.clone(), partition, address, None, no_remote(), None)
        .await
        .expect_err("the whole content needs subtrees the store does not hold");

    read(store, partition, address, Some(650..700), no_remote(), None)
        .await
        .expect_err("a range under the absent subtree cannot read");
}

/// Content small enough to live in one fragment takes the direct-write path, which sizes
/// the file from the buffer rather than from the sink.
#[tokio::test(flavor = "multi_thread")]
async fn a_ranged_read_into_file_writes_only_the_range_for_one_fragment() {
    let (dir, store) = make_test_store().await;
    let (partition, address, fragment, payload) = make_input(0x38);
    store
        .clone()
        .put(partition, address, fragment, Some(payload.clone()), false)
        .await
        .expect("put single fragment");

    let target = PathBuf::from(dir.as_ref()).join("ranged-single.bin");
    read_into_file(
        store,
        partition,
        address,
        target.as_path(),
        ".~loretemp",
        Some(8..24),
        no_remote(),
        None,
    )
    .await
    .expect("ranged read into file");

    let on_disk = std::fs::read(&target).expect("read target");
    assert_eq!(on_disk, payload[8..24]);
}

#[tokio::test(flavor = "multi_thread")]
async fn read_into_single_fragment_respects_range() {
    let (_dir, store) = make_test_store().await;

    let mut payload = vec![0u8; 100];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = i as u8;
    }

    let hash_value = hash::hash_slice(&payload);
    let partition = Partition::from([0; 16]);
    let address = Address {
        hash: hash_value,
        context: Context::from([0; 16]),
    };
    let fragment = Fragment {
        flags: FragmentFlags::PayloadStoredLocal.bits(),
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    };

    store
        .clone()
        .put(
            partition,
            address,
            fragment,
            Some(Bytes::from(payload.clone())),
            false,
        )
        .await
        .expect("put test data");

    let mut out = [0u8; 40];
    read_into(
        store,
        partition,
        address,
        Some(10..50),
        &mut out,
        ReadOptions::default(),
        None,
    )
    .await
    .expect("read_into should respect range");

    assert_eq!(&out[..], &payload[10..50]);
}

/// With caching off the leader stores nothing locally, so followers must be handed the
/// fetched payload rather than re-probing the local store.
#[tokio::test(flavor = "multi_thread")]
async fn followers_get_the_leaders_bytes_when_nothing_is_cached() {
    let (_dir, store) = make_test_store().await;
    let (partition, address, fragment, payload) = make_input(0xD7);
    let fetches = Arc::new(AtomicU64::new(0));
    let gate = Arc::new(tokio::sync::Barrier::new(9));

    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let store = store.clone();
            let fetches = fetches.clone();
            let payload = payload.clone();
            let gate = gate.clone();
            lore_base::lore_spawn!(async move {
                gate.wait().await;
                fetch_single_flight(
                    store,
                    partition,
                    address,
                    ReadOptions::default(),
                    || {
                        let fetches = fetches.clone();
                        let payload = payload.clone();
                        async move {
                            fetches.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                            Ok(Loaded {
                                content: Some((fragment, payload.clone())),
                                raw: (fragment, payload),
                            })
                        }
                    },
                    || {},
                )
                .await
                .and_then(|flown| flown.take(true))
            })
        })
        .collect();
    gate.wait().await;

    for task in tasks {
        let (got_fragment, bytes) = task
            .await
            .expect("single-flight task panicked")
            .expect("every caller gets the content");
        assert_eq!(got_fragment.size_content, fragment.size_content);
        assert_eq!(bytes, payload);
    }
    assert_eq!(fetches.load(Ordering::SeqCst), 1, "one fetch serves all");
    assert!(
        read_raw(store, partition, address).await.is_err(),
        "nothing was cached"
    );
}

/// Concurrent misses of one content under different addresses, other contexts and other
/// partitions, share one fetch where the store does not isolate partitions.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_misses_of_one_content_under_different_addresses_fetch_once() {
    let (_dir, store) = make_test_store().await;
    let (partition, address, fragment, payload) = make_input(0xE8);
    let fetches = Arc::new(AtomicU64::new(0));
    let gate = Arc::new(tokio::sync::Barrier::new(9));

    let tasks: Vec<_> = (0..8u8)
        .map(|caller| {
            let store = store.clone();
            let fetches = fetches.clone();
            let payload = payload.clone();
            let gate = gate.clone();
            let partition = if caller % 2 == 0 {
                partition
            } else {
                lore_storage::Partition::from([caller; 16])
            };
            let address = Address {
                context: lore_storage::Context::from([caller; 16]),
                ..address
            };
            lore_base::lore_spawn!(async move {
                gate.wait().await;
                fetch_single_flight(
                    store,
                    partition,
                    address,
                    ReadOptions::default(),
                    || {
                        let fetches = fetches.clone();
                        let payload = payload.clone();
                        async move {
                            fetches.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                            Ok(Loaded {
                                content: Some((fragment, payload.clone())),
                                raw: (fragment, payload),
                            })
                        }
                    },
                    || {},
                )
                .await
                .and_then(|flown| flown.take(true))
            })
        })
        .collect();
    gate.wait().await;

    for task in tasks {
        let (_, bytes) = task
            .await
            .expect("single-flight task panicked")
            .expect("every caller gets the content");
        assert_eq!(bytes, payload);
    }
    assert_eq!(fetches.load(Ordering::SeqCst), 1, "one fetch serves all");
}

/// A follower takes the leader's error only for the leader's own address: one under another
/// context fetches for itself, and one under the same address answers the error.
#[tokio::test(flavor = "multi_thread")]
async fn only_a_follower_of_the_leaders_address_takes_its_error() {
    use lore_storage::read_flight::ReadFlight;
    use lore_storage::read_flight::join_read_flight;

    let (_dir, store) = make_test_store().await;
    for (seed, same_address) in [(0xE9, false), (0xEA, true)] {
        let (partition, address, fragment, payload) = make_input(seed);
        let ReadFlight::Leader(leader) = join_read_flight(partition, address, false, false) else {
            panic!("the first caller leads");
        };
        let follower_address = if same_address {
            address
        } else {
            Address {
                context: lore_storage::Context::from([!seed; 16]),
                ..address
            }
        };
        let fetches = Arc::new(AtomicU64::new(0));
        let follower = {
            let (store, fetches, raw) =
                (store.clone(), fetches.clone(), (fragment, payload.clone()));
            lore_base::lore_spawn!(async move {
                fetch_single_flight(
                    store,
                    partition,
                    follower_address,
                    ReadOptions::default(),
                    || {
                        fetches.fetch_add(1, Ordering::SeqCst);
                        let raw = raw.clone();
                        async move {
                            Ok(Loaded {
                                content: Some(raw.clone()),
                                raw,
                            })
                        }
                    },
                    || {},
                )
                .await
                .and_then(|flown| flown.take(true))
            })
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while !leader.has_followers() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the follower joined");
        leader.complete(&Err(StorageError::from(
            lore_storage::errors::AddressNotFound::from(address),
        )));

        let read = follower.await.expect("follower task");
        if same_address {
            assert!(
                matches!(read, Err(StorageError::AddressNotFound(_))),
                "a follower of the leader's address answers its error, got {read:?}"
            );
            assert_eq!(fetches.load(Ordering::SeqCst), 0);
        } else {
            let (_, bytes) = read.expect("a follower of another address fetches itself");
            assert_eq!(bytes, payload);
            assert_eq!(fetches.load(Ordering::SeqCst), 1);
        }
    }
}

/// A fetch that does not trust the remote never follows a flight that trusts it, so it never takes
/// a payload checked less than it asks.
#[tokio::test(flavor = "multi_thread")]
async fn an_untrusting_fetch_never_follows_a_trusting_flight() {
    use lore_storage::read_flight::ReadFlight;
    use lore_storage::read_flight::join_read_flight;

    let (_dir, store) = make_test_store().await;
    let (partition, address, fragment, payload) = make_input(0xEC);
    let ReadFlight::Leader(leader) = join_read_flight(partition, address, false, true) else {
        panic!("the first caller leads");
    };
    let fetches = Arc::new(AtomicU64::new(0));
    let (_, bytes) = tokio::time::timeout(
        Duration::from_secs(2),
        fetch_single_flight(
            store,
            partition,
            address,
            ReadOptions::default(),
            || {
                fetches.fetch_add(1, Ordering::SeqCst);
                let raw = (fragment, payload.clone());
                async move {
                    Ok(Loaded {
                        content: Some(raw.clone()),
                        raw,
                    })
                }
            },
            || {},
        ),
    )
    .await
    .expect("the untrusting fetch followed the trusting flight")
    .expect("the untrusting fetch fetches itself")
    .take(true)
    .expect("the fetched content");
    assert_eq!(bytes, payload);
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
    drop(leader);
}

/// Where the store isolates partitions, a miss in another partition fetches for itself rather
/// than following a flight for the same content held in another.
#[tokio::test(flavor = "multi_thread")]
async fn an_isolating_store_never_shares_a_flight_across_partitions() {
    use lore_storage::read_flight::ReadFlight;
    use lore_storage::read_flight::join_read_flight;

    let (_dir, store) = store_with_isolation(true).await;
    let (partition, address, fragment, payload) = make_input(0xEB);
    let ReadFlight::Leader(leader) = join_read_flight(partition, address, true, false) else {
        panic!("the first caller leads");
    };
    let other_partition = lore_storage::Partition::from([!0xEBu8; 16]);
    let fetches = Arc::new(AtomicU64::new(0));
    let (_, bytes) = tokio::time::timeout(
        Duration::from_secs(2),
        fetch_single_flight(
            store,
            other_partition,
            address,
            ReadOptions::default(),
            || {
                fetches.fetch_add(1, Ordering::SeqCst);
                let raw = (fragment, payload.clone());
                async move {
                    Ok(Loaded {
                        content: Some(raw.clone()),
                        raw,
                    })
                }
            },
            || {},
        ),
    )
    .await
    .expect("the miss in another partition followed the held flight")
    .expect("the miss fetches itself")
    .take(true)
    .expect("the fetched content");
    assert_eq!(bytes, payload);
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
    drop(leader);
}

/// A local-only load that misses while a remote flight for the same address is held answers
/// the miss at once rather than following the flight: with no session, and with a session
/// but the remote forbidden.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_only_miss_never_follows_a_remote_flight() {
    let (_dir, store) = make_test_store().await;
    for (seed, options, session) in [
        (0xC1, ReadOptions::default(), None),
        (0xC2, ReadOptions::default().no_remote(), stand_in_session()),
    ] {
        let (partition, address, fragment, payload) = make_input(seed);
        let (started, released) = (
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(tokio::sync::Notify::new()),
        );
        let leader = {
            let (store, started, released) = (store.clone(), started.clone(), released.clone());
            lore_base::lore_spawn!(async move {
                fetch_single_flight(
                    store,
                    partition,
                    address,
                    ReadOptions::default(),
                    || {
                        let (started, released, payload) =
                            (started.clone(), released.clone(), payload.clone());
                        async move {
                            started.notify_one();
                            released.notified().await;
                            Ok(Loaded {
                                content: Some((fragment, payload.clone())),
                                raw: (fragment, payload),
                            })
                        }
                    },
                    || {},
                )
                .await
                .and_then(|flown| flown.take(true))
            })
        };
        started.notified().await;

        let local = tokio::time::timeout(
            Duration::from_secs(2),
            load_fragment(store.clone(), partition, address, options, session),
        )
        .await
        .unwrap_or_else(|_| {
            panic!("the local-only load ({seed:#x}) followed the held remote flight")
        });
        assert!(
            matches!(local, Err(StorageError::AddressNotFound(_))),
            "a local-only miss answers AddressNotFound, got {local:?}"
        );

        released.notify_one();
        leader
            .await
            .expect("leader task")
            .expect("the leader fetches");
    }
}

/// A session whose resolve, and so a leader's upload, waits for `release` and then fails, so
/// the leader keeps the payload locally. `entered` fires as the upload starts.
fn held_upload_session(
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
) -> Arc<StorageSession> {
    Arc::new(StorageSession::pending(move || {
        let (entered, release) = (entered.clone(), release.clone());
        async move {
            entered.notify_one();
            release.notified().await;
            Err(lore_transport::ProtocolError::internal(
                "the held upload fails",
            ))
        }
    }))
}

/// A tracker-dispatched store, held in its upload with nothing local yet. A
/// reader holding a session misses, waits for the leader, and reads the leader's local write
/// rather than starting a flight before the upload is done.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_reader_waits_out_a_dispatched_upload_then_reads_local() {
    let (_dir, store) = make_test_store().await;
    let (partition, address, fragment, payload) = make_input(0xB7);
    let (entered, release) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    let tracker = Arc::new(lore_storage::WriteTracker::new());
    lore_storage::write::store_fragment(
        store.clone(),
        partition,
        address,
        Fragment {
            flags: 0,
            ..fragment
        },
        payload.clone(),
        false,
        Some(held_upload_session(entered.clone(), release.clone())),
        lore_storage::WriteContext::tracked(Some(tracker.clone()), None),
        None,
    )
    .await
    .expect("dispatch the store");
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("the leader reached its upload");

    let reader = lore_base::lore_spawn!(load_fragment(
        store.clone(),
        partition,
        address,
        ReadOptions::default(),
        stand_in_session(),
    ));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !reader.is_finished(),
        "the reader finished before the leader's upload completed"
    );
    release.notify_one();
    let (_, loaded) = tokio::time::timeout(Duration::from_secs(5), reader)
        .await
        .expect("the reader hung after the leader finished")
        .expect("reader task")
        .expect("the reader reads the leader's local write");
    assert_eq!(loaded, payload);
    tracker.await_all().await.expect("the tracker drains");
}

/// Content already local while a dispatched store of it is held in its upload:
/// a reader with a session hits local at once.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_reader_hits_local_during_a_dispatched_upload() {
    let (_dir, store) = make_test_store().await;
    let (partition, address, fragment, payload) = make_input(0xB8);
    store
        .clone()
        .put(
            partition,
            address,
            Fragment {
                flags: 0,
                ..fragment
            },
            Some(payload.clone()),
            false,
        )
        .await
        .expect("put locally, not durable");
    let (entered, release) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    let tracker = Arc::new(lore_storage::WriteTracker::new());
    lore_storage::write::store_fragment(
        store.clone(),
        partition,
        address,
        Fragment {
            flags: 0,
            ..fragment
        },
        payload.clone(),
        false,
        Some(held_upload_session(entered.clone(), release.clone())),
        lore_storage::WriteContext::tracked(Some(tracker.clone()), None),
        None,
    )
    .await
    .expect("dispatch the store");
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("the leader reached its upload");

    let (_, loaded) = tokio::time::timeout(
        Duration::from_secs(2),
        load_fragment(
            store.clone(),
            partition,
            address,
            ReadOptions::default(),
            stand_in_session(),
        ),
    )
    .await
    .expect("the local hit waited on the dispatched upload")
    .expect("the local hit");
    assert_eq!(loaded, payload);
    release.notify_one();
    tracker.await_all().await.expect("the tracker drains");
}

/// `never_wait` with the remote allowed and a session: a miss answers
/// `AddressNotFound` with no flight, while a remote flight for the address is held.
#[tokio::test(flavor = "multi_thread")]
async fn never_wait_with_a_session_never_follows_a_flight() {
    let (_dir, store) = make_test_store().await;
    let (partition, address, fragment, payload) = make_input(0xB9);
    let (started, released) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    let leader = {
        let (store, started, released) = (store.clone(), started.clone(), released.clone());
        lore_base::lore_spawn!(async move {
            fetch_single_flight(
                store,
                partition,
                address,
                ReadOptions::default(),
                || {
                    let (started, released, payload) =
                        (started.clone(), released.clone(), payload.clone());
                    async move {
                        started.notify_one();
                        released.notified().await;
                        Ok(Loaded {
                            content: Some((fragment, payload.clone())),
                            raw: (fragment, payload),
                        })
                    }
                },
                || {},
            )
            .await
            .and_then(|flown| flown.take(true))
        })
    };
    started.notified().await;
    let local = tokio::time::timeout(
        Duration::from_secs(2),
        load_fragment(
            store.clone(),
            partition,
            address,
            ReadOptions::default().never_wait(),
            stand_in_session(),
        ),
    )
    .await
    .expect("never_wait with a session followed the held flight");
    assert!(
        matches!(local, Err(StorageError::AddressNotFound(_))),
        "got {local:?}"
    );
    released.notify_one();
    leader
        .await
        .expect("leader task")
        .expect("the leader fetches");
}

/// A whole read into a buffer that misses while a tracker-dispatched store of the address is
/// held in its upload reads the store's local write once it lands, rather than fetching it:
/// the read's own probe releases the upload.
#[tokio::test(flavor = "multi_thread")]
async fn a_whole_read_into_a_buffer_reads_a_dispatched_store_it_missed_locally() {
    let (_dir, inner) = make_test_store().await;
    let (partition, address, fragment, payload) = make_input(0xBA);
    let (entered, release) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    let tracker = Arc::new(lore_storage::WriteTracker::new());
    lore_storage::write::store_fragment(
        inner.clone(),
        partition,
        address,
        Fragment {
            flags: 0,
            ..fragment
        },
        payload.clone(),
        false,
        Some(held_upload_session(entered.clone(), release.clone())),
        lore_storage::WriteContext::tracked(Some(tracker.clone()), None),
        None,
    )
    .await
    .expect("dispatch the store");
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("the leader reached its upload");
    let (store, counts) = CountingReadStore::with_hooks(
        inner,
        ReadHooks {
            release: Some(release),
            ..ReadHooks::default()
        },
    );

    let mut buffer = vec![0u8; 4096];
    // SAFETY: the buffer outlives the read and nothing else touches it.
    let mut dst = unsafe { CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
    let (_, written) = tokio::time::timeout(
        Duration::from_secs(2),
        read_into_buffer(
            store,
            partition,
            address,
            None,
            &mut dst,
            ReadOptions::default(),
            stand_in_session(),
        ),
    )
    .await
    .expect("the whole read hung on the dispatched store")
    .expect("the whole read reads the store's local write");
    assert!(
        counts.gets() + counts.gets_into() >= 1,
        "the whole read probed the local store"
    );
    assert_eq!(&buffer[..written], payload.as_ref());
    tracker.await_all().await.expect("the tracker drains");
}

/// A whole read into a buffer with `never_wait` misses at once while a remote flight for the
/// address is held, rather than following it.
#[tokio::test(flavor = "multi_thread")]
async fn a_never_waiting_whole_read_into_a_buffer_never_follows_a_flight() {
    let (_dir, store) = make_test_store().await;
    let (partition, address, fragment, payload) = make_input(0xBB);
    let (started, released) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    let leader = {
        let (store, started, released) = (store.clone(), started.clone(), released.clone());
        lore_base::lore_spawn!(async move {
            fetch_single_flight(
                store,
                partition,
                address,
                ReadOptions::default(),
                || {
                    let (started, released, payload) =
                        (started.clone(), released.clone(), payload.clone());
                    async move {
                        started.notify_one();
                        released.notified().await;
                        Ok(Loaded {
                            content: Some((fragment, payload.clone())),
                            raw: (fragment, payload),
                        })
                    }
                },
                || {},
            )
            .await
            .and_then(|flown| flown.take(true))
        })
    };
    started.notified().await;

    let mut buffer = vec![0u8; 4096];
    // SAFETY: the buffer outlives the read and nothing else touches it.
    let mut dst = unsafe { CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
    let read = tokio::time::timeout(
        Duration::from_secs(2),
        read_into_buffer(
            store.clone(),
            partition,
            address,
            None,
            &mut dst,
            ReadOptions::default().never_wait(),
            stand_in_session(),
        ),
    )
    .await
    .expect("the never-waiting whole read followed the held flight");
    assert!(
        matches!(read, Err(StorageError::AddressNotFound(_))),
        "got {read:?}"
    );
    released.notify_one();
    leader
        .await
        .expect("leader task")
        .expect("the leader fetches");
}

/// [`a_never_waiting_miss_answers_at_once_while_a_store_is_in_flight`], for a whole read into a
/// buffer.
#[tokio::test(flavor = "multi_thread")]
async fn a_never_waiting_whole_read_into_a_buffer_answers_a_miss_at_once_while_a_store_is_in_flight()
 {
    let (_dir, store) = make_test_store().await;
    let (partition, address, _fragment, _payload) = make_input(0xE7);
    let guard = try_acquire_in_flight(partition, address).expect("no contention in fresh test");

    let mut buffer = vec![0u8; 4096];
    // SAFETY: the buffer outlives the read and nothing else touches it.
    let mut dst = unsafe { CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
    let read = tokio::time::timeout(
        Duration::from_secs(2),
        read_into_buffer(
            store,
            partition,
            address,
            None,
            &mut dst,
            ReadOptions::default().no_remote().never_wait(),
            None,
        ),
    )
    .await
    .expect("the miss waited on the store in flight");
    assert!(
        matches!(read, Err(StorageError::AddressNotFound(_))),
        "a never-waiting miss answers AddressNotFound, got {read:?}"
    );
    drop(guard);
}

/// A stand-in remote serving `size` bytes of compressible content as one compressed fragment, and
/// a stand-in session whose fetches the served fragment answers. Nothing is stored locally and the
/// counts start at zero.
///
/// The content follows from `context`'s first byte, so tests that pass different contexts serve
/// different content: reads of the same content coalesce across the process, and a test counting
/// its own fetches must not share a flight with another.
fn serve_compressed(
    context: Context,
    size: usize,
) -> (Address, Vec<u8>, Option<Arc<StorageSession>>) {
    let seed = context.as_ref()[0];
    let content: Vec<u8> = (0..size).map(|index| (index / 64) as u8 ^ seed).collect();
    let plain = Fragment {
        flags: 0,
        size_payload: content.len() as u32,
        size_content: content.len() as u64,
    };
    let (fragment, payload) = lore_storage::compress::compress(
        plain,
        &content,
        lore_storage::compress::CompressionMode::Lz4,
    )
    .expect("compress test content");
    let address = Address {
        hash: hash::hash_slice(&content),
        context,
    };
    lore_storage::test_util::serve_remote(address, fragment, payload);
    lore_storage::test_util::reset_counts();
    (address, content, stand_in_session())
}

/// A session for the stand-in remote: fetches never reach it, and a heal it is asked for fails.
fn stand_in_session() -> Option<Arc<StorageSession>> {
    Some(Arc::new(StorageSession::pending(|| async {
        Err(lore_transport::ProtocolError::internal(
            "the stand-in remote needs no session",
        ))
    })))
}

/// `size` bytes of content, the plain fragment describing it, and its address.
fn plain_content(context: Context, size: usize, seed: u8) -> (Address, Fragment, Bytes) {
    let content = Bytes::from(
        (0..size)
            .map(|index| (index as u8).wrapping_mul(seed))
            .collect::<Vec<u8>>(),
    );
    let fragment = Fragment {
        flags: 0,
        size_payload: size as u32,
        size_content: size as u64,
    };
    let address = Address {
        hash: hash::hash_slice(&content),
        context,
    };
    (address, fragment, content)
}

/// Remote payloads whose sizes disagree: a plain payload claiming less content than it holds, and
/// one whose length disagrees with its size. The content follows from `context`'s first byte.
fn sizes_disagreeing(context: Context) -> [(&'static str, Address, Fragment, Bytes); 2] {
    let (address, fragment, content) = plain_content(context, 4096, context.as_ref()[0]);
    let short_content = Fragment {
        size_content: 4000,
        ..fragment
    };
    let long_payload = Fragment {
        size_payload: 4095,
        ..fragment
    };
    [
        (
            "a wrong content size",
            address,
            short_content,
            content.clone(),
        ),
        ("a payload length mismatch", address, long_payload, content),
    ]
}

/// Remote payloads that fail receipt: those whose sizes disagree ([`sizes_disagreeing`]), and one
/// whose bytes do not hash to its address.
fn failing_receipts(context: Context) -> [(&'static str, Address, Fragment, Bytes); 3] {
    let [short_content, long_payload] = sizes_disagreeing(context);
    let (address, fragment, content) = plain_content(context, 4096, context.as_ref()[0]);
    let mut other = content.to_vec();
    other[0] ^= 0xff;
    [
        short_content,
        ("a hash mismatch", address, fragment, Bytes::from(other)),
        long_payload,
    ]
}

/// Local copies that do not load: a compressed payload that does not decode, and a plain payload
/// claiming more content than it holds, whose sizes disagree. Each payload is 64 bytes.
fn broken_copies(size_content: u64) -> [(&'static str, Fragment); 2] {
    [
        (
            "an undecodable payload",
            Fragment {
                flags: FragmentFlags::PayloadCompressedLZ4.bits(),
                size_payload: 64,
                size_content,
            },
        ),
        (
            "a payload claiming more content than it holds",
            Fragment {
                flags: 0,
                size_payload: 64,
                size_content,
            },
        ),
    ]
}

async fn test_mutable_store(
    dir: &TempDir,
    store: &Arc<dyn ImmutableStore>,
) -> Arc<dyn MutableStore> {
    use lore_storage::local::mutable_store::LocalMutableStore;
    use lore_storage::local::mutable_store::MutableStoreSettings;
    Arc::new(
        LocalMutableStore::new(
            Some(PathBuf::from(dir.as_ref())),
            MutableStoreSettings::default(),
            store.clone(),
        )
        .await
        .expect("create mutable store"),
    )
}

/// A load's future does not hold the remote fetch it may make, which only a miss reaching the
/// remote takes: it is smaller than the fetch alone.
#[tokio::test]
async fn a_load_does_not_hold_its_remote_fetch() {
    let (_dir, store) = make_test_store().await;
    let address = Address {
        hash: hash::hash_slice(b"content no load here reads"),
        context: Context::from([0x9f; 16]),
    };
    let session = stand_in_session().expect("a stand-in session");
    let options = ReadOptions::default();

    let load = load_fragment(
        store,
        Partition::from([0x9f; 16]),
        address,
        options,
        Some(session.clone()),
    );
    let fetch = fetch_remote(&session, address, options);

    assert!(
        size_of_val(&load) < size_of_val(&fetch),
        "a load of {} bytes holds its fetch of {} bytes",
        size_of_val(&load),
        size_of_val(&fetch)
    );
}

/// The broken copies ([`broken_copies`]) and the reads that take them, each with a seed of its own:
/// the undecodable payload read for its content, and the payload whose sizes disagree read from a
/// trusted remote, for its content and as stored.
fn broken_copy_reads() -> [(u8, (&'static str, Fragment), ReadOptions); 3] {
    let [undecodable, claims_more] = broken_copies(4096);
    let trusting = ReadOptions::default().trusting_remote();
    [
        (6, undecodable, ReadOptions::default()),
        (7, claims_more, trusting),
        (8, claims_more, trusting.no_decompress()),
    ]
}

/// A local payload that does not load is missing to a read with no remote in reach, and is fetched
/// again and overwritten otherwise, so the next read is answered locally. One whose sizes disagree
/// fails though the remote is trusted and the read takes the payload as stored.
#[tokio::test]
async fn a_local_payload_that_does_not_load_is_fetched_again_and_overwritten() {
    for (seed, (label, broken), options) in broken_copy_reads() {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([seed; 16]);
        let (address, content, session) = serve_compressed(Context::from([seed; 16]), 4096);
        let (_, served) = lore_storage::test_util::remote_payload(address).expect("the payload");
        lore_storage::test_util::accepting_unvalidated_puts(store.clone().put(
            partition,
            address,
            broken,
            Some(Bytes::from(vec![0xff; 64])),
            false,
        ))
        .await
        .expect("put the broken copy");
        let expected = if options.decompress {
            Bytes::from(content)
        } else {
            served
        };

        let missing =
            load_fragment(store.clone(), partition, address, options.no_remote(), None).await;
        let (_, fetched) = load_fragment(store.clone(), partition, address, options, session)
            .await
            .expect(label);
        let (_, local) = load_fragment(store, partition, address, options.no_remote(), None)
            .await
            .expect(label);

        assert!(
            missing
                .as_ref()
                .is_err_and(|err| err.is_address_not_found()),
            "{label}: {missing:?}"
        );
        assert_eq!(fetched, expected, "{label} is fetched again");
        assert_eq!(local, expected, "{label} is overwritten");
    }
}

/// A resolve whose local root does not load takes the remote's root and overwrites the local copy
/// with it, though the read does not cache, so the next resolve is answered locally. A root whose
/// sizes disagree fails though the remote is trusted and the read takes the root as stored.
#[tokio::test]
async fn a_local_root_that_does_not_load_is_resolved_again_and_overwritten() {
    for (seed, (label, broken), options) in broken_copy_reads() {
        let seed = seed + 3;
        let (dir, store) = make_test_store().await;
        let mutable = test_mutable_store(&dir, &store).await;
        let partition = Partition::from([seed; 16]);
        let context = Context::from([seed; 16]);
        let (address, content, session) = serve_compressed(context, 4096);
        let (fragment, payload) =
            lore_storage::test_util::remote_payload(address).expect("the root");
        let key = hash::hash_slice(label.as_bytes());
        lore_storage::test_util::serve_remote_resolved(
            key,
            address.hash,
            fragment,
            payload.clone(),
        );
        lore_storage::test_util::accepting_unvalidated_puts(store.clone().put(
            partition,
            address,
            broken,
            Some(Bytes::from(vec![0xff; 64])),
            false,
        ))
        .await
        .expect("put the broken copy");
        mutable
            .clone()
            .store(partition, key, address.hash, KeyType::Resolve)
            .await
            .expect("map the key locally");
        let expected = if options.decompress {
            Bytes::from(content)
        } else {
            payload
        };

        let resolved = resolve_root(
            store.clone(),
            mutable.clone(),
            partition,
            key,
            context,
            0,
            options,
            session,
        )
        .await
        .expect(label);
        let local = resolve_root(
            store,
            mutable,
            partition,
            key,
            context,
            0,
            options.no_remote(),
            None,
        )
        .await
        .expect(label);

        assert_eq!(resolved.buffer, expected, "{label} is resolved again");
        assert_eq!(local.buffer, expected, "{label} is overwritten");
    }
}

/// The content at `address` read into caller memory of `len` bytes, by [`read_into_buffer`] where
/// `into_buffer` is set and by [`read_into`] otherwise.
async fn read_into_memory(
    store: Arc<dyn ImmutableStore>,
    (partition, address, len): (Partition, Address, usize),
    into_buffer: bool,
    options: ReadOptions,
    session: Option<Arc<StorageSession>>,
) -> Result<Vec<u8>, StorageError> {
    let mut memory = vec![0u8; len];
    if into_buffer {
        // SAFETY: the memory outlives the read and nothing else touches it.
        let mut dst = unsafe { CallerBuffer::new(memory.as_mut_ptr(), memory.len()) };
        read_into_buffer(store, partition, address, None, &mut dst, options, session).await?;
    } else {
        read_into(
            store,
            partition,
            address,
            None,
            &mut memory,
            options,
            session,
        )
        .await?;
    }
    Ok(memory)
}

/// A read into caller memory of a local payload that does not load, whose sizes disagree or that
/// does not decode, fetches the content again, though the remote is trusted, and overwrites the
/// local copy, so the next read is answered locally.
#[tokio::test]
async fn a_read_into_caller_memory_fetches_and_overwrites_a_local_payload_that_does_not_load() {
    let [undecodable, claims_more] = broken_copies(4096);
    for (seed, (label, broken), into_buffer) in [
        (12u8, claims_more, false),
        (13, claims_more, true),
        (40, undecodable, false),
        (41, undecodable, true),
    ] {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([seed; 16]);
        let (address, content, session) = serve_compressed(Context::from([seed; 16]), 4096);
        lore_storage::test_util::accepting_unvalidated_puts(store.clone().put(
            partition,
            address,
            broken,
            Some(Bytes::from(vec![0xff; 64])),
            false,
        ))
        .await
        .expect("put the broken copy");
        let at = (partition, address, content.len());
        let options = ReadOptions::default().trusting_remote();

        let fetched = read_into_memory(store.clone(), at, into_buffer, options, session)
            .await
            .expect(label);
        let local = read_into_memory(store, at, into_buffer, options.no_remote(), None)
            .await
            .expect(label);

        assert_eq!(
            fetched, content,
            "{label}, read into a buffer: {into_buffer}"
        );
        assert_eq!(
            local, content,
            "{label} is overwritten, read into a buffer: {into_buffer}"
        );
    }
}

/// A read into caller memory of a local compressed payload longer than its fragment gives fetches
/// the content again rather than decoding the payload, which the decoder would read only as far as
/// the fragment gives.
#[tokio::test]
async fn a_read_into_caller_memory_fetches_a_local_compressed_payload_longer_than_its_fragment() {
    for (seed, into_buffer) in [(14u8, false), (15, true)] {
        let (_dir, inner) = make_test_store().await;
        let partition = Partition::from([seed; 16]);
        let (address, content, session) = serve_compressed(Context::from([seed; 16]), 4096);
        let (fragment, payload) =
            lore_storage::test_util::remote_payload(address).expect("the payload");
        inner
            .clone()
            .put(partition, address, fragment, Some(payload), false)
            .await
            .expect("store the payload");
        lore_storage::test_util::reset_counts();
        let store = CountingReadStore::padding(inner);

        let read = read_into_memory(
            store,
            (partition, address, content.len()),
            into_buffer,
            ReadOptions::default(),
            session,
        )
        .await
        .expect("read into caller memory");

        let (fetches, _, _) = lore_storage::test_util::counts();
        assert_eq!(read, content, "read into a buffer: {into_buffer}");
        assert_eq!(fetches, 1, "read into a buffer: {into_buffer}");
    }
}

/// A fetched payload that fails receipt is refused and never reaches the local store.
#[tokio::test]
async fn a_remote_payload_failing_receipt_is_refused_and_not_stored() {
    for (index, (label, address, fragment, payload)) in failing_receipts(Context::from([96; 16]))
        .into_iter()
        .enumerate()
    {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([96 + index as u8; 16]);
        lore_storage::test_util::serve_remote(address, fragment, payload);

        let loaded = load_fragment(
            store.clone(),
            partition,
            address,
            ReadOptions::default().with_cache(),
            stand_in_session(),
        )
        .await;

        assert!(loaded.is_err(), "{label} was accepted");
        assert!(
            read_raw(store, partition, address).await.is_err(),
            "{label} was stored"
        );
    }
}

/// A resolved root that fails receipt is refused, and neither it nor the mapping to it is
/// stored.
#[tokio::test]
async fn a_resolved_root_failing_receipt_is_refused_and_not_stored() {
    for (index, (label, address, fragment, payload)) in failing_receipts(Context::from([97; 16]))
        .into_iter()
        .enumerate()
    {
        let (dir, store) = make_test_store().await;
        let mutable = test_mutable_store(&dir, &store).await;
        let partition = Partition::from([97 + index as u8; 16]);
        let key = hash::hash_slice(label.as_bytes());
        lore_storage::test_util::serve_remote_resolved(key, address.hash, fragment, payload);

        let resolved = resolve_root(
            store.clone(),
            mutable.clone(),
            partition,
            key,
            address.context,
            0,
            ReadOptions::default().with_cache(),
            stand_in_session(),
        )
        .await;

        assert!(resolved.is_err(), "{label} was accepted");
        assert!(
            read_raw(store, partition, address).await.is_err(),
            "{label} was stored"
        );
        assert!(
            mutable
                .load(partition, key, KeyType::Resolve)
                .await
                .map_or(true, |hash| hash.is_zero()),
            "the mapping to {label} was stored"
        );
    }
}

/// A trusted remote's payload whose sizes disagree is refused and never reaches the local store,
/// whether the read takes its content or the payload as stored: its sizes are checked though it is
/// not hashed.
#[tokio::test]
async fn a_trusted_remote_payload_whose_sizes_disagree_is_refused_and_not_stored() {
    for (index, (label, address, fragment, payload)) in sizes_disagreeing(Context::from([14; 16]))
        .into_iter()
        .enumerate()
    {
        for decompress in [true, false] {
            let (_dir, store) = make_test_store().await;
            let partition = Partition::from([14 + 2 * index as u8 + decompress as u8; 16]);
            lore_storage::test_util::serve_remote(address, fragment, payload.clone());

            let loaded = load_fragment(
                store.clone(),
                partition,
                address,
                ReadOptions {
                    decompress,
                    ..ReadOptions::default().trusting_remote().with_cache()
                },
                stand_in_session(),
            )
            .await;

            assert!(loaded.is_err(), "{label} was accepted");
            assert!(
                read_raw(store, partition, address).await.is_err(),
                "{label} was stored"
            );
        }
    }
}

/// A trusted remote's resolved root whose sizes disagree is refused, and neither it nor the mapping
/// to it is stored, whether or not the root is expanded.
#[tokio::test]
async fn a_trusted_resolved_root_whose_sizes_disagree_is_refused_and_not_stored() {
    for (index, (label, address, fragment, payload)) in sizes_disagreeing(Context::from([18; 16]))
        .into_iter()
        .enumerate()
    {
        for decompress in [true, false] {
            let (dir, store) = make_test_store().await;
            let mutable = test_mutable_store(&dir, &store).await;
            let partition = Partition::from([18 + 2 * index as u8 + decompress as u8; 16]);
            let key = hash::hash_slice(format!("{label}, decompressed: {decompress}").as_bytes());
            lore_storage::test_util::serve_remote_resolved(
                key,
                address.hash,
                fragment,
                payload.clone(),
            );

            let resolved = resolve_root(
                store.clone(),
                mutable.clone(),
                partition,
                key,
                address.context,
                0,
                ReadOptions {
                    decompress,
                    ..ReadOptions::default().trusting_remote().with_cache()
                },
                stand_in_session(),
            )
            .await;

            assert!(resolved.is_err(), "{label} was accepted");
            assert!(
                read_raw(store, partition, address).await.is_err(),
                "{label} was stored"
            );
            assert!(
                mutable
                    .load(partition, key, KeyType::Resolve)
                    .await
                    .map_or(true, |hash| hash.is_zero()),
                "the mapping to {label} was stored"
            );
        }
    }
}

/// One fetch, one decode, one hash: the root a remote resolve verified is the root handed back.
#[tokio::test]
async fn remote_resolve_decodes_and_hashes_once() {
    let (dir, store) = make_test_store().await;
    let mutable = test_mutable_store(&dir, &store).await;
    let partition = Partition::from([98; 16]);
    let context = Context::from([98; 16]);
    let (address, content, session) = serve_compressed(context, 4096);
    let (fragment, payload) =
        lore_storage::test_util::remote_payload(address).expect("the compressed root");
    let key = hash::hash_slice(b"a key naming compressed content");
    lore_storage::test_util::serve_remote_resolved(key, address.hash, fragment, payload);
    lore_storage::test_util::reset_counts();

    let resolved = resolve_root(
        store,
        mutable,
        partition,
        key,
        context,
        0,
        ReadOptions::default().with_cache(),
        session,
    )
    .await
    .expect("resolve the remote root");

    assert_eq!(resolved.buffer.as_ref(), content.as_slice());
    assert_eq!(lore_storage::test_util::counts(), (1, 1, 1));
}

/// A remote resolve that does not ask to expand hands the root back as fetched.
#[tokio::test]
async fn a_remote_resolve_without_decompress_hands_the_root_back_as_fetched() {
    let (dir, store) = make_test_store().await;
    let mutable = test_mutable_store(&dir, &store).await;
    let partition = Partition::from([95; 16]);
    let context = Context::from([95; 16]);
    let (address, _content, session) = serve_compressed(context, 4096);
    let (fragment, payload) =
        lore_storage::test_util::remote_payload(address).expect("the compressed root");
    let key = hash::hash_slice(b"a key naming a compressed root read as fetched");
    lore_storage::test_util::serve_remote_resolved(key, address.hash, fragment, payload.clone());

    let resolved = resolve_root(
        store,
        mutable,
        partition,
        key,
        context,
        0,
        ReadOptions::default().no_decompress(),
        session,
    )
    .await
    .expect("resolve the remote root");

    assert_eq!(resolved.buffer, payload);
    assert_ne!(
        resolved.fragment.flags & FragmentFlags::PayloadCompressed,
        0
    );
}

/// A remote resolve of a root larger than the cap is refused, and neither the root nor the mapping
/// to it is stored.
#[tokio::test]
async fn a_remote_resolve_over_the_cap_is_refused_and_not_stored() {
    let (dir, store) = make_test_store().await;
    let mutable = test_mutable_store(&dir, &store).await;
    let partition = Partition::from([88; 16]);
    let context = Context::from([88; 16]);
    let (address, _content, session) = serve_compressed(context, 4096);
    let (fragment, payload) =
        lore_storage::test_util::remote_payload(address).expect("the compressed root");
    let key = hash::hash_slice(b"a key naming a root over the cap");
    lore_storage::test_util::serve_remote_resolved(key, address.hash, fragment, payload);

    let refused = resolve_root(
        store.clone(),
        mutable.clone(),
        partition,
        key,
        context,
        0,
        ReadOptions::default()
            .with_cache()
            .with_max_content_size(1024),
        session,
    )
    .await
    .err();

    assert!(
        matches!(refused, Some(StorageError::Oversized(_))),
        "got {refused:?}"
    );
    assert!(
        read_raw(store, partition, address).await.is_err(),
        "the root was stored"
    );
    assert!(
        mutable
            .load(partition, key, KeyType::Resolve)
            .await
            .map_or(true, |hash| hash.is_zero()),
        "the mapping was stored"
    );
}

/// A fetch into a caller's buffer that fails is the read's failure: the assembling path does
/// not fetch, heal and retry the same address again.
#[tokio::test]
async fn a_failing_remote_fetch_into_a_buffer_fetches_once() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([99; 16]);
    let [_, (_, address, fragment, payload), _] = failing_receipts(Context::from([99; 16]));
    lore_storage::test_util::serve_remote(address, fragment, payload);
    lore_storage::test_util::reset_counts();

    let mut buffer = vec![0u8; 4096];
    // SAFETY: the buffer outlives the read and nothing else touches it.
    let mut dst = unsafe { CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
    let read = read_into_buffer(
        store,
        partition,
        address,
        None,
        &mut dst,
        ReadOptions::default(),
        stand_in_session(),
    )
    .await;

    assert!(read.is_err());
    assert_eq!(lore_storage::test_util::counts().0, 1, "fetches");
}

/// Whether a storage error reports a given refusal.
type ReportsRefusal = fn(&StorageError) -> bool;

/// The refusals a remote answers, each with a reason on its trace as the transport gives one, and
/// the predicate a storage error reporting it meets.
fn refusals() -> [(ProtocolError, ReportsRefusal); 2] {
    use lore_error_set::ErrorSet;
    let with_reason = |mut refusal: ProtocolError| {
        refusal.push_trace(lore_error_set::Location::with_context(
            file!(),
            line!(),
            column!(),
            Arc::from("the token expired"),
        ));
        refusal
    };
    [
        (
            with_reason(ProtocolError::from(lore_base::error::NotAuthorized)),
            StorageError::is_not_authorized,
        ),
        (
            with_reason(ProtocolError::from(lore_base::error::NotAuthenticated)),
            StorageError::is_not_authenticated,
        ),
    ]
}

/// A session that answers every resolution with `error`, and the count of its resolutions. A
/// failed resolution is held until the session is invalidated, so each resolution after the first
/// is a request made on a fresh session.
fn failing_session(error: ProtocolError) -> (Arc<StorageSession>, Arc<AtomicU64>) {
    let resolutions = Arc::new(AtomicU64::new(0));
    let counted = resolutions.clone();
    let session = Arc::new(StorageSession::pending(move || {
        counted.fetch_add(1, Ordering::Relaxed);
        let error = error.clone();
        async move { Err(error) }
    }));
    (session, resolutions)
}

/// A fetch the remote refuses is retried once, at once, on a fresh session, since an expired token
/// is refused and a fresh session can carry a fresh one, and then fails with the refusal and its
/// trace.
#[tokio::test(start_paused = true)]
async fn a_refused_remote_get_is_retried_once() {
    let (_dir, store) = make_test_store().await;
    let (partition, address, _, _) = make_input(0xF3);
    for (refusal, reports_refusal) in refusals() {
        let source = refusal.trace().locations().to_vec();
        let (session, resolutions) = failing_session(refusal);
        let start = tokio::time::Instant::now();

        let err = load_fragment(
            store.clone(),
            partition,
            address,
            ReadOptions::default(),
            Some(session),
        )
        .await
        .expect_err("the remote refuses the fetch");

        assert_eq!(
            resolutions.load(Ordering::Relaxed),
            2,
            "gets, failing with {err:?}"
        );
        assert_eq!(start.elapsed(), Duration::ZERO, "the refusal backed off");
        assert!(reports_refusal(&err), "{err:?}");
        assert!(err.trace().locations().starts_with(&source), "{err:?}");
    }
}

/// A fetch failing with `NotConnected` is retried five times on a fresh session, backing off
/// before each retry, and then fails with `NotConnected`.
#[tokio::test(start_paused = true)]
async fn a_not_connected_remote_get_is_retried_five_times_with_back_off() {
    let (_dir, store) = make_test_store().await;
    let (partition, address, _, _) = make_input(0xF5);
    let (session, resolutions) =
        failing_session(ProtocolError::internal("the server knows no such session"));
    let start = tokio::time::Instant::now();

    let err = load_fragment(
        store,
        partition,
        address,
        ReadOptions::default(),
        Some(session),
    )
    .await
    .expect_err("the session never resolves");

    assert_eq!(
        resolutions.load(Ordering::Relaxed),
        6,
        "gets, failing with {err:?}"
    );
    assert!(
        start.elapsed() > Duration::ZERO,
        "the retries did not back off"
    );
    assert!(err.is_not_connected(), "{err:?}");
}

/// A resolve the remote refuses is retried once, at once, on a fresh session, as a fetch is, and
/// then fails with the refusal and its trace.
#[tokio::test(start_paused = true)]
async fn a_refused_remote_resolve_is_retried_once() {
    let (dir, store) = make_test_store().await;
    let mutable = test_mutable_store(&dir, &store).await;
    let partition = Partition::from([0xF4; 16]);
    let context = Context::from([0xF4; 16]);
    let key = hash::hash_slice(b"a key the remote refuses to resolve");
    for (refusal, reports_refusal) in refusals() {
        let source = refusal.trace().locations().to_vec();
        let (session, resolutions) = failing_session(refusal);
        let start = tokio::time::Instant::now();

        let Err(err) = resolve_root(
            store.clone(),
            mutable.clone(),
            partition,
            key,
            context,
            0,
            ReadOptions::default(),
            Some(session),
        )
        .await
        else {
            panic!("the remote refuses the resolve");
        };

        assert_eq!(
            resolutions.load(Ordering::Relaxed),
            2,
            "resolves, failing with {err:?}"
        );
        assert_eq!(start.elapsed(), Duration::ZERO, "the refusal backed off");
        assert!(reports_refusal(&err), "{err:?}");
        assert!(err.trace().locations().starts_with(&source), "{err:?}");
    }
}

/// A resolve failing with `NotConnected` is retried five times on a fresh session, backing off
/// before each retry, as a fetch is, and then fails with `NotConnected`.
#[tokio::test(start_paused = true)]
async fn a_not_connected_remote_resolve_is_retried_five_times_with_back_off() {
    let (dir, store) = make_test_store().await;
    let mutable = test_mutable_store(&dir, &store).await;
    let key = hash::hash_slice(b"a key resolved on a session the server does not know");
    let (session, resolutions) =
        failing_session(ProtocolError::internal("the server knows no such session"));
    let start = tokio::time::Instant::now();

    let Err(err) = resolve_root(
        store,
        mutable,
        Partition::from([0xF6; 16]),
        key,
        Context::from([0xF6; 16]),
        0,
        ReadOptions::default(),
        Some(session),
    )
    .await
    else {
        panic!("the session never resolves");
    };

    assert_eq!(
        resolutions.load(Ordering::Relaxed),
        6,
        "resolves, failing with {err:?}"
    );
    assert!(
        start.elapsed() > Duration::ZERO,
        "the retries did not back off"
    );
    assert!(err.is_not_connected(), "{err:?}");
}

/// One fetch, one decode, one hash: the content a remote fetch verified is the content handed
/// back.
#[tokio::test]
async fn remote_content_load_decodes_and_hashes_once() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([91; 16]);
    let (address, content, session) = serve_compressed(Context::from([91; 16]), 4096);

    let (_, loaded) = load_fragment(
        store,
        partition,
        address,
        ReadOptions::default().with_cache(),
        session,
    )
    .await
    .expect("load the remote content");

    assert_eq!(loaded.as_ref(), content.as_slice());
    assert_eq!(lore_storage::test_util::counts(), (1, 1, 1));
}

/// A caller asking for the stored form gets it as fetched, verified once on receipt.
#[tokio::test]
async fn remote_stored_load_decodes_and_hashes_once() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([92; 16]);
    let (address, _content, session) = serve_compressed(Context::from([92; 16]), 4096);

    let (fragment, _) = load_fragment(
        store,
        partition,
        address,
        ReadOptions::default().no_decompress(),
        session,
    )
    .await
    .expect("load the remote payload");

    assert_ne!(fragment.flags & FragmentFlags::PayloadCompressed, 0);
    assert_eq!(lore_storage::test_util::counts(), (1, 1, 1));
}

/// A payload fetched from the remote adds its length to the process-wide byte count.
#[tokio::test]
async fn a_remote_fetch_counts_its_payload_bytes() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([231; 16]);
    let (address, _content, session) = serve_compressed(Context::from([231; 16]), 8192);
    let before = remote_fetch_bytes();

    let (fragment, _) = load_fragment(
        store,
        partition,
        address,
        ReadOptions::default().no_decompress(),
        session,
    )
    .await
    .expect("load the remote payload");

    assert!(remote_fetch_bytes() - before >= u64::from(fragment.size_payload));
}

/// A fetch from a trusted remote is never hashed, and is decoded only where the caller asks for
/// content: once then, and not at all for the stored form.
#[tokio::test]
async fn a_trusted_remote_fetch_is_decoded_only_for_content_and_never_hashed() {
    for (seed, decompress, decodes) in [(81u8, false, 0), (82, true, 1)] {
        let (_dir, store) = make_test_store().await;
        let partition = Partition::from([seed; 16]);
        let (address, content, session) = serve_compressed(Context::from([seed; 16]), 4096);

        let (fragment, loaded) = load_fragment(
            store,
            partition,
            address,
            ReadOptions {
                decompress,
                ..ReadOptions::default().trusting_remote()
            },
            session,
        )
        .await
        .expect("load from the trusted remote");

        assert_eq!(
            fragment.flags & FragmentFlags::PayloadCompressed == 0,
            decompress
        );
        if decompress {
            assert_eq!(loaded.as_ref(), content.as_slice());
        }
        assert_eq!(lore_storage::test_util::counts(), (1, decodes, 0));
    }
}

/// A trusted remote resolve is never hashed, and decodes the root only where it is to be expanded.
#[tokio::test]
async fn a_trusted_remote_resolve_is_decoded_only_to_expand_and_never_hashed() {
    for (seed, decompress, decodes) in [(86u8, false, 0), (87, true, 1)] {
        let (dir, store) = make_test_store().await;
        let mutable = test_mutable_store(&dir, &store).await;
        let context = Context::from([seed; 16]);
        let (address, content, session) = serve_compressed(context, 4096);
        let (fragment, payload) =
            lore_storage::test_util::remote_payload(address).expect("the compressed root");
        let key = hash::hash_slice(&[seed]);
        lore_storage::test_util::serve_remote_resolved(
            key,
            address.hash,
            fragment,
            payload.clone(),
        );
        lore_storage::test_util::reset_counts();

        let resolved = resolve_root(
            store,
            mutable,
            Partition::from([seed; 16]),
            key,
            context,
            0,
            ReadOptions {
                decompress,
                ..ReadOptions::default().trusting_remote()
            },
            session,
        )
        .await
        .expect("resolve the trusted remote root");

        let expected = if decompress {
            content.as_slice()
        } else {
            payload.as_ref()
        };
        assert_eq!(resolved.buffer.as_ref(), expected);
        assert_eq!(lore_storage::test_util::counts(), (1, decodes, 0));
    }
}

/// A fetch the check on receipt did not expand, as from a trusted remote for a caller wanting the
/// stored form, is expanded where it is taken for its content, and handed back as fetched
/// otherwise.
#[test]
fn a_fetch_not_expanded_on_receipt_expands_where_its_content_is_taken() {
    let content: Vec<u8> = (0..4096).map(|index| (index / 64) as u8).collect();
    let plain = Fragment {
        flags: 0,
        size_payload: content.len() as u32,
        size_content: content.len() as u64,
    };
    let raw = lore_storage::compress::compress(
        plain,
        &content,
        lore_storage::compress::CompressionMode::Lz4,
    )
    .expect("compress test content");
    let fetched = Loaded {
        content: None,
        raw: raw.clone(),
    };

    let (_, expanded) = fetched.clone().take(true).expect("the fetch expands");
    let (_, stored) = fetched.take(false).expect("the fetch as fetched");

    assert_eq!(expanded.as_ref(), content.as_slice());
    assert_eq!(stored, raw.1);
}

/// A trusted remote's payload is served as fetched: one that does not hash to its address is
/// accepted, where a fetch that does not trust the remote refuses it.
#[tokio::test]
async fn a_trusted_remote_payload_is_served_unhashed() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([83; 16]);
    let (address, fragment, content) = plain_content(Context::from([83; 16]), 4096, 11);
    let mut altered = content.to_vec();
    altered[0] ^= 0xff;
    let altered = Bytes::from(altered);
    lore_storage::test_util::serve_remote(address, fragment, altered.clone());

    let refused = load_fragment(
        store.clone(),
        partition,
        address,
        ReadOptions::default(),
        stand_in_session(),
    )
    .await;
    let (_, served) = load_fragment(
        store,
        partition,
        address,
        ReadOptions::default().trusting_remote(),
        stand_in_session(),
    )
    .await
    .expect("the trusted remote's payload is served");

    assert!(refused.is_err(), "an untrusting fetch accepted the payload");
    assert_eq!(served, altered);
}

#[tokio::test]
async fn remote_read_into_decodes_and_hashes_once() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([93; 16]);
    let (address, content, session) = serve_compressed(Context::from([93; 16]), 4096);

    let mut slice = vec![0u8; 100];
    read_into(
        store,
        partition,
        address,
        Some(1000..1100),
        &mut slice,
        ReadOptions::default(),
        session,
    )
    .await
    .expect("read the remote range");

    assert_eq!(slice.as_slice(), &content[1000..1100]);
    assert_eq!(lore_storage::test_util::counts(), (1, 1, 1));
}

#[tokio::test]
async fn remote_read_into_buffer_decodes_and_hashes_once() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([94; 16]);
    let (address, content, session) = serve_compressed(Context::from([94; 16]), 4096);

    let mut buffer = vec![0u8; content.len()];
    // SAFETY: the buffer outlives the read and nothing else touches it.
    let mut dst = unsafe { CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
    let (_, written) = read_into_buffer(
        store,
        partition,
        address,
        None,
        &mut dst,
        ReadOptions::default(),
        session,
    )
    .await
    .expect("read the remote content");

    assert_eq!(written, content.len());
    assert_eq!(buffer, content);
    assert_eq!(lore_storage::test_util::counts(), (1, 1, 1));
}

/// A whole read into a buffer of fetched content larger than the caller's cap fails with the
/// one fetch it made, rather than leaving the read to a path that fetches it again.
#[tokio::test]
async fn a_remote_whole_read_over_the_cap_fails_after_one_fetch() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([84; 16]);
    let (address, content, session) = serve_compressed(Context::from([84; 16]), 4096);

    let mut buffer = vec![0u8; content.len()];
    // SAFETY: the buffer outlives the read and nothing else touches it.
    let mut dst = unsafe { CallerBuffer::new(buffer.as_mut_ptr(), buffer.len()) };
    let result = read_into_buffer(
        store,
        partition,
        address,
        None,
        &mut dst,
        ReadOptions::default().with_max_content_size(1024),
        session,
    )
    .await;

    assert!(
        matches!(result, Err(StorageError::Oversized(_))),
        "got {result:?}"
    );
    assert_eq!(lore_storage::test_util::counts().0, 1, "fetches");
}

/// A caller joining a fetch in flight takes the content the leader already decoded.
#[tokio::test]
async fn concurrent_remote_misses_decode_and_hash_once() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([85; 16]);
    let (address, content, session) = serve_compressed(Context::from([85; 16]), 4096);
    let gate = Arc::new(tokio::sync::Notify::new());
    lore_storage::test_util::hold_remote_fetches(gate.clone());

    let load = || {
        load_fragment(
            store.clone(),
            partition,
            address,
            ReadOptions::default(),
            session.clone(),
        )
    };
    let release = async {
        while lore_storage::test_util::followers() == 0 {
            tokio::task::yield_now().await;
        }
        gate.notify_one();
    };
    let (first, second, ()) = tokio::join!(load(), load(), release);

    assert_eq!(first.expect("leader").1.as_ref(), content.as_slice());
    assert_eq!(second.expect("follower").1.as_ref(), content.as_slice());
    assert_eq!(lore_storage::test_util::counts(), (1, 1, 1));
}

#[cfg(not(feature = "oodle"))]
mod load_fragment_of_undecodable_oodle {
    use lore_transport::ProtocolError;
    use lore_transport::StorageSession;

    use super::*;

    /// Store a payload under flags claiming it is Oodle-encoded, durably stored or not. A build
    /// without Oodle cannot decode it.
    async fn put_oodle(
        store: &Arc<dyn ImmutableStore>,
        seed: u8,
        durable: bool,
    ) -> (Partition, Address) {
        let (partition, address, fragment, payload) = make_input(seed);
        let mut flags = fragment.flags | FragmentFlags::PayloadCompressedOodle2.bits();
        if durable {
            flags |= FragmentFlags::PayloadStoredDurable.bits();
        }
        store
            .clone()
            .put(
                partition,
                address,
                Fragment { flags, ..fragment },
                Some(payload),
                false,
            )
            .await
            .expect("put the Oodle-flagged entry");
        (partition, address)
    }

    /// A session whose remote holds nothing, so a read that asks it reports a miss.
    fn empty_remote() -> Arc<StorageSession> {
        Arc::new(StorageSession::pending(|| async {
            Err(ProtocolError::from(lore_base::error::NotFound))
        }))
    }

    /// With no remote to replace it from, the payload is reported as undecodable rather than
    /// missing, since it is held.
    #[tokio::test]
    async fn is_reported_not_supported_without_a_remote() {
        let (_dir, store) = make_test_store().await;
        let (partition, address) = put_oodle(&store, 0xa1, true).await;

        let err = load_fragment(
            store,
            partition,
            address,
            ReadOptions::default().no_remote(),
            None,
        )
        .await
        .expect_err("an Oodle payload cannot be decoded by this build");

        assert!(err.is_not_supported(), "unexpected error: {err:?}");
    }

    /// A read into a slice of a payload only the local store holds reports it undecodable rather
    /// than missing: the held payload is loaded again, not taken as absent.
    #[tokio::test]
    async fn a_read_into_a_slice_reports_a_local_only_payload_not_supported() {
        let (_dir, store) = make_test_store().await;
        let (partition, address) = put_oodle(&store, 0xa5, false).await;
        let mut slice = vec![0u8; 64];

        let err = read_into(
            store,
            partition,
            address,
            None,
            &mut slice,
            ReadOptions::default(),
            None,
        )
        .await
        .expect_err("an Oodle payload cannot be decoded by this build");

        assert!(err.is_not_supported(), "unexpected error: {err:?}");
    }

    /// A resolve of a key mapped to such a payload reports it undecodable rather than missing, as a
    /// load of its address does: one held only locally, and a durable one with no remote to fetch
    /// it from.
    #[tokio::test]
    async fn a_resolve_reports_it_not_supported_without_a_remote() {
        for (seed, durable) in [(0xa6u8, false), (0xa7, true)] {
            let (dir, store) = make_test_store().await;
            let mutable = test_mutable_store(&dir, &store).await;
            let (partition, address) = put_oodle(&store, seed, durable).await;
            let key = hash::hash_slice(&[seed]);
            mutable
                .clone()
                .store(partition, key, address.hash, KeyType::Resolve)
                .await
                .expect("map the key locally");

            let Err(err) = resolve_root(
                store,
                mutable,
                partition,
                key,
                address.context,
                0,
                ReadOptions::default(),
                None,
            )
            .await
            else {
                panic!("an Oodle payload cannot be decoded by this build");
            };

            assert!(err.is_not_supported(), "durable {durable}: {err:?}");
        }
    }

    /// A read allowed to fall back to the remote but handed no session has nothing to fetch
    /// from either, and reports the same.
    #[tokio::test]
    async fn is_reported_not_supported_without_a_session() {
        let (_dir, store) = make_test_store().await;
        let (partition, address) = put_oodle(&store, 0xa2, true).await;

        let err = load_fragment(store, partition, address, ReadOptions::default(), None)
            .await
            .expect_err("an Oodle payload cannot be decoded by this build");

        assert!(err.is_not_supported(), "unexpected error: {err:?}");
    }

    /// A durably stored payload is fetched from the remote, so what the remote answers is what
    /// the read reports.
    #[tokio::test]
    async fn is_fetched_from_the_remote_when_durable() {
        let (_dir, store) = make_test_store().await;
        let (partition, address) = put_oodle(&store, 0xa3, true).await;

        let err = load_fragment(
            store,
            partition,
            address,
            ReadOptions::default(),
            Some(empty_remote()),
        )
        .await
        .expect_err("the remote holds nothing");

        assert!(
            matches!(err, StorageError::AddressNotFound(_)),
            "unexpected error: {err:?}"
        );
    }

    /// A local-only payload has no copy upstream, so it is reported as undecodable without
    /// asking the remote.
    #[tokio::test]
    async fn is_reported_not_supported_when_local_only() {
        let (_dir, store) = make_test_store().await;
        let (partition, address) = put_oodle(&store, 0xa4, false).await;

        let err = load_fragment(
            store,
            partition,
            address,
            ReadOptions::default(),
            Some(empty_remote()),
        )
        .await
        .expect_err("an Oodle payload cannot be decoded by this build");

        assert!(err.is_not_supported(), "unexpected error: {err:?}");
    }
}
