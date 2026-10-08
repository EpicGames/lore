// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
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
struct CountingReadStore {
    inner: Arc<dyn ImmutableStore>,
    gets: Arc<std::sync::atomic::AtomicUsize>,
    gets_into: Arc<std::sync::atomic::AtomicUsize>,
}

impl CountingReadStore {
    fn wrap(inner: Arc<dyn ImmutableStore>) -> (Arc<dyn ImmutableStore>, Self) {
        let gets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let gets_into = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counts = CountingReadStore {
            inner: inner.clone(),
            gets: gets.clone(),
            gets_into: gets_into.clone(),
        };
        let store: Arc<dyn ImmutableStore> = Arc::new(CountingReadStore {
            inner,
            gets,
            gets_into,
        });
        (store, counts)
    }

    fn gets(&self) -> usize {
        self.gets.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn gets_into(&self) -> usize {
        self.gets_into.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ImmutableStore for CountingReadStore {
    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<lore_storage::store_types::StoreGetData, StoreError> {
        self.gets.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.clone().get(partition, address).await
    }

    async fn get_into(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        dst: &mut lore_storage::CallerBuffer,
    ) -> Result<(Fragment, PayloadRead), StoreError> {
        self.gets_into
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.clone().get_into(partition, address, dst).await
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
    (result, counts.gets())
}

/// A verified whole read of a compressed fragment delivers its content from one load.
#[tokio::test]
async fn a_whole_compressed_read_into_a_slice_loads_its_payload_once() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x91; 16]);
    let (address, content) =
        put_compressed(&store, partition, Context::from([0x91; 16]), 4096).await;

    let (slice, gets) = read_into_counted(
        store,
        partition,
        address,
        content.len(),
        ReadOptions::default().no_remote(),
    )
    .await;

    assert_eq!(slice.expect("read compressed content"), content);
    assert_eq!(gets, 1, "the payload was loaded more than once");
}

/// An unverified whole read of a compressed fragment delivers its content from one load.
#[tokio::test]
async fn an_unverified_whole_compressed_read_into_a_slice_loads_its_payload_once() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x92; 16]);
    let (address, content) =
        put_compressed(&store, partition, Context::from([0x92; 16]), 4096).await;

    let (slice, gets) = read_into_counted(
        store,
        partition,
        address,
        content.len(),
        ReadOptions::default().no_remote().no_verify(),
    )
    .await;

    assert_eq!(slice.expect("read compressed content"), content);
    assert_eq!(gets, 1, "the payload was loaded more than once");
}

/// A read into a slice of content spread across fragments assembles it from the verified list.
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

    let (slice, gets) =
        read_into_counted(store, partition, address, 16, ReadOptions::default()).await;

    assert!(
        matches!(slice, Err(StorageError::AddressNotFound(_))),
        "a payload no store holds was not reported missing"
    );
    assert_eq!(
        gets, 1,
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

    let mut slice = vec![0u8; content.len()];
    read_into(
        store,
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

    let (slice, gets) = read_into_counted(
        store,
        Partition::from([0x95; 16]),
        address,
        8,
        ReadOptions::default().no_remote(),
    )
    .await;

    assert_eq!(slice.expect("read the zero hash"), vec![0u8; 8]);
    assert_eq!(gets, 0, "the store was asked for the zero hash");
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

/// `no_verify` delivers the stored bytes without hashing them. A verifying read of the same
/// address rejects them, so the flag has to reach the read that fills the buffer.
#[tokio::test]
async fn an_unverified_read_skips_the_hash_check() {
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
        ReadOptions::default().no_remote().no_verify(),
    )
    .await
    .expect("an unverified read delivers the stored bytes");

    assert_eq!(written, payload.len());
    assert_eq!(buffer.as_slice(), payload.as_ref());
}

/// A compressed payload expands into the caller's buffer whether or not the content is hashed
/// afterwards.
#[tokio::test]
async fn an_unverified_compressed_read_expands_into_the_caller_buffer() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x84; 16]);
    let (address, content) =
        put_compressed(&store, partition, Context::from([0x84; 16]), 4096).await;

    let (buffer, written) = read_into_vec(
        store,
        partition,
        address,
        None,
        content.len(),
        ReadOptions::default().no_remote().no_verify(),
    )
    .await
    .expect("read compressed content without verifying it");

    assert_eq!(written, content.len());
    assert_eq!(buffer, content);
}

/// Leaves are verified as they are loaded, so a list walk delivers the same content whether or
/// not the caller asked for verification.
#[tokio::test]
async fn an_unverified_fragmented_read_assembles_into_the_caller_buffer() {
    let (_dir, store) = make_test_store().await;
    let partition = Partition::from([0x85; 16]);
    let context = Context::from([0x85; 16]);
    let (root_address, content) = put_two_leaf_list(&store, partition, context).await;

    let (buffer, written) = read_into_vec(
        store,
        partition,
        root_address,
        None,
        content.len(),
        ReadOptions::default().no_remote().no_verify(),
    )
    .await
    .expect("read fragmented content without verifying it");

    assert_eq!(written, content.len());
    assert_eq!(buffer, content);
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

/// A resolved stream with neither `decompress` nor `verify` hands a leaf over as stored: a zstd
/// payload that does not decode arrives untouched, where checking or expanding it would fail.
#[tokio::test]
async fn a_raw_resolved_stream_neither_verifies_nor_expands() {
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
        ReadOptions::default()
            .no_decompress()
            .no_verify()
            .no_remote(),
        tx,
        None,
    )
    .await
    .expect("a raw resolved stream neither verifies nor expands");

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
        ReadOptions::default().no_verify().no_remote(),
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
            ReadOptions::default().no_verify(),
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
            ReadOptions::default().no_verify(),
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

    async fn put_leaf(
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
    async fn put_list(
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
    ReadOptions::default().no_verify().no_remote()
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
        ReadOptions::default().no_verify(),
        None,
    )
    .await
    .expect("read_into should respect range");

    assert_eq!(&out[..], &payload[10..50]);
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
