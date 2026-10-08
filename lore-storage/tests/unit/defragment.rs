// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ops::Range;

use bytes::Bytes;
use lore_io::IoFile;
use lore_storage::Address;
use lore_storage::Context;
use lore_storage::Fragment;
use lore_storage::FragmentReference;
use lore_storage::Hash;
use lore_storage::Partition;
use lore_storage::concurrency::FRAGMENT_MINIMUM_COST_KIB;
use lore_storage::concurrency::fragment_limiter;
use lore_storage::concurrency::fragment_permit_count;
use lore_storage::defragment::*;
use lore_storage::error::StorageError;
use lore_storage::fragment_flags::FragmentFlags;
use lore_storage::immutable_store::CopyBehavior;
use lore_storage::immutable_store::ImmutableStore;
use tokio::sync::Semaphore;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::channel;

mod walk_leaf_level {
    use super::*;

    /// Hashes are distinct and non-zero: a zero hash is not a legal list entry, so a
    /// list built from `Hash::default()` would be rejected before the offset arithmetic
    /// these tests are about.
    fn refs(offsets: &[u64]) -> Vec<FragmentReference> {
        offsets
            .iter()
            .map(|&o| FragmentReference {
                hash: lore_storage::hash::hash_slice(&o.to_le_bytes()),
                offset_content: o,
            })
            .collect()
    }

    /// Drive `walk_leaf_level` over the whole level and collect emitted leaves.
    async fn run(
        fragment_list: &[FragmentReference],
        total_content_size: usize,
        base_offset: u64,
    ) -> Result<Vec<LeafReference>, StorageError> {
        // Saturating: the overflow cases below hand this a base that cannot legally be
        // added to, and it is the walker's job to say so rather than the harness's.
        run_windowed(
            fragment_list,
            total_content_size,
            base_offset,
            base_offset..base_offset.saturating_add(total_content_size as u64),
        )
        .await
    }

    /// Drive `walk_leaf_level` over `window` and collect emitted leaves.
    async fn run_windowed(
        fragment_list: &[FragmentReference],
        total_content_size: usize,
        base_offset: u64,
        window: Range<u64>,
    ) -> Result<Vec<LeafReference>, StorageError> {
        let (tx, mut rx) = channel::<LeafReference>(32);
        let context = Context::default();
        let walk_result = walk_leaf_level(
            fragment_list,
            total_content_size,
            base_offset,
            &window,
            context,
            &tx,
        )
        .await;
        drop(tx);
        let mut leaves = Vec::new();
        while let Some(leaf) = rx.recv().await {
            leaves.push(leaf);
        }
        walk_result.map(|()| leaves)
    }

    #[tokio::test]
    async fn accepts_well_formed_list() {
        // Base 0, content 2000, refs at 0 / 500 / 1500.
        // Chunks: 500, 1000, 500 (final = 2000 - 1500).
        let list = refs(&[0, 500, 1500]);
        let leaves = run(&list, 2000, 0).await.expect("well-formed");
        assert_eq!(leaves.len(), 3);
        assert_eq!(leaves[0].expected_size, 500);
        assert_eq!(leaves[1].expected_size, 1000);
        assert_eq!(leaves[2].expected_size, 500);
    }

    #[tokio::test]
    async fn accepts_interior_list_with_nonzero_base_offset() {
        // Child list for a sublist that lives between absolute offsets
        // 10_000 and 12_000. Refs are in the absolute coordinate system.
        let list = refs(&[10_000, 10_500, 11_000]);
        let leaves = run(&list, 2000, 10_000).await.expect("interior ok");
        assert_eq!(leaves.len(), 3);
        assert_eq!(leaves[0].expected_size, 500);
        assert_eq!(leaves[1].expected_size, 500);
        assert_eq!(leaves[2].expected_size, 1000); // 10_000 + 2000 - 11_000
    }

    #[tokio::test]
    async fn rejects_non_increasing_offsets() {
        // Second offset equal to first — checked_sub gives zero after the
        // strict-increasing invariant would normally have rejected it;
        // here the zero-size branch catches it instead. Either way:
        // rejected.
        let list = refs(&[100, 100, 500]);
        run(&list, 1000, 0).await.expect_err("non-increasing");
    }

    #[tokio::test]
    async fn rejects_decreasing_offsets() {
        let list = refs(&[500, 100]);
        run(&list, 1000, 0).await.expect_err("decreasing");
    }

    #[tokio::test]
    async fn rejects_base_plus_content_overflow() {
        // base_offset near u64::MAX + a non-trivial content size wraps.
        let list = refs(&[u64::MAX - 10]);
        run(&list, 100, u64::MAX - 10)
            .await
            .expect_err("overflow on base+content");
    }

    #[tokio::test]
    async fn rejects_last_offset_at_or_past_content_end() {
        // base=0, content=1000, ref at 1000 → final chunk would be 0 bytes.
        let list = refs(&[0, 1000]);
        run(&list, 1000, 0).await.expect_err("last at end");
    }

    #[tokio::test]
    async fn rejects_chunk_exceeding_threshold() {
        // Two refs spanning 1 MiB of content inside a 2 MiB window — the
        // first chunk is 1 MiB, exceeding FRAGMENT_SIZE_THRESHOLD (256 KiB).
        // A hostile peer's intermediate list that somehow looks like a leaf
        // list with oversized chunks is rejected here.
        let span = lore_storage::FRAGMENT_SIZE_THRESHOLD + 1;
        let list = refs(&[0, span as u64]);
        run(&list, span * 2, 0).await.expect_err("oversized chunk");
    }

    #[tokio::test]
    async fn accepts_single_ref_list() {
        // Single leaf with the whole content window. Not produced by the
        // engine (lists have ≥ 2 refs by construction), but walk_leaf_level
        // itself doesn't enforce that — the ≥ 2 check lives in
        // validate_fragment_list on the Put side.
        let list = refs(&[0]);
        let leaves = run(&list, 500, 0).await.expect("single ref ok");
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].expected_size, 500);
    }

    /// A window inside one leaf yields that leaf alone, clipped to the window and
    /// positioned at the start of the output. The leaf's own size is unchanged: the
    /// payload is verified whole, and only what is delivered is narrowed.
    #[tokio::test]
    async fn a_window_inside_one_leaf_yields_only_that_leaf() {
        let list = refs(&[0, 500, 1500]);
        let leaves = run_windowed(&list, 2000, 0, 600..700)
            .await
            .expect("windowed");
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].expected_size, 1000);
        assert_eq!(leaves[0].clip, 100..200);
        assert_eq!(leaves[0].target_offset, 0);
    }

    /// A window spanning three leaves clips only the ends. Targets tile the output from
    /// zero, which is what makes the file sink's coverage check add up for a range.
    #[tokio::test]
    async fn a_window_spanning_leaves_clips_only_the_ends() {
        let list = refs(&[0, 500, 1500]);
        let leaves = run_windowed(&list, 2000, 0, 400..1600)
            .await
            .expect("windowed");
        assert_eq!(leaves.len(), 3);
        assert_eq!(
            (leaves[0].clip.clone(), leaves[0].target_offset),
            (400..500, 0)
        );
        assert_eq!(
            (leaves[1].clip.clone(), leaves[1].target_offset),
            (0..1000, 100)
        );
        assert_eq!(
            (leaves[2].clip.clone(), leaves[2].target_offset),
            (0..100, 1100)
        );

        let delivered: u64 = leaves
            .iter()
            .map(|leaf| leaf.clip.end - leaf.clip.start)
            .sum();
        assert_eq!(delivered, 1200, "the clips must cover the window exactly");
    }

    /// A window touching no entry yields nothing while still checking the list: a
    /// malformed list is malformed whichever part of the content a caller asks for.
    #[tokio::test]
    async fn a_window_past_the_level_yields_nothing() {
        let list = refs(&[0, 500, 1500]);
        let leaves = run_windowed(&list, 2000, 0, 2000..2500)
            .await
            .expect("past the end is empty, not an error");
        assert!(leaves.is_empty());

        let bad = refs(&[0, 1000, 500]);
        run_windowed(&bad, 2000, 0, 2000..2500)
            .await
            .expect_err("a list outside the window is still validated");
    }

    /// Interior levels carry absolute offsets, so a window has to be compared in the
    /// same coordinates rather than rebased per level.
    #[tokio::test]
    async fn a_window_on_an_interior_level_uses_absolute_offsets() {
        let list = refs(&[10_000, 10_500, 11_000]);
        let leaves = run_windowed(&list, 2000, 10_000, 10_400..10_600)
            .await
            .expect("interior windowed");
        assert_eq!(leaves.len(), 2);
        assert_eq!(
            (leaves[0].clip.clone(), leaves[0].target_offset),
            (400..500, 0)
        );
        assert_eq!(
            (leaves[1].clip.clone(), leaves[1].target_offset),
            (0..100, 100)
        );
    }
}

mod write_to_file {
    //! Direct unit tests for the file write sink's runtime bounds check.
    //!
    //! In the full pipeline the leaf contiguity check in `fetch_unordered`
    //! filters out the inputs that would make this bound fire, so these
    //! tests exercise the sink in isolation — the bound is defense-in-depth
    //! against any future producer that bypasses earlier validation. Unlike
    //! the memory-mapped sink this replaced, an unchecked offset here is not
    //! unsound, but it would still write far past the intended end of file.
    use lore_base::test_util::TempDir;

    use super::*;

    const SIZE: usize = 100;

    /// A sized target file, opened the way the materialization path opens one.
    async fn target(dir: &TempDir, name: &str) -> IoFile {
        lore_storage::defragment::open_file_write(dir.path().join(name), SIZE)
            .await
            .expect("create target file")
    }

    /// Send one message carrying a permit, as the fetch pool does.
    async fn send_one(tx: &DataSender, offset: usize, payload: Bytes) {
        let permit = fragment_limiter()
            .acquire_many(fragment_permit_count(payload.len()))
            .await
            .expect("permit");
        tx.send((offset, payload, permit)).await.expect("send");
    }

    #[tokio::test]
    async fn accepts_in_bounds_write() {
        let dir = TempDir::new("lore-storage-sink-test-");
        let file = target(&dir, "in-bounds").await;
        let (tx, rx) = channel::<DataMessage>(4);
        send_one(&tx, 0, Bytes::from(vec![0xCD; 10])).await;
        send_one(&tx, 10, Bytes::from(vec![0xAB; 20])).await;
        send_one(&tx, 30, Bytes::from(vec![0xEF; SIZE - 30])).await;
        drop(tx);

        lore_storage::defragment::write_to_file(file.clone(), SIZE, rx)
            .await
            .expect("in-bounds write");

        let contents = file.read_exact_at(SIZE, 0).await.expect("read back");
        assert_eq!(&contents[10..30], &[0xAB; 20]);
    }

    /// Payloads that stay in bounds but do not add up to the file: the target is
    /// `set_len` up front, so the uncovered range is zeros in a file that would
    /// otherwise be renamed into place as complete.
    #[tokio::test]
    async fn rejects_payloads_that_do_not_cover_the_file() {
        let dir = TempDir::new("lore-storage-sink-test-");
        let file = target(&dir, "hole").await;
        let (tx, rx) = channel::<DataMessage>(4);
        send_one(&tx, 0, Bytes::from(vec![0xAB; 20])).await;
        send_one(&tx, 40, Bytes::from(vec![0xAB; SIZE - 40])).await;
        drop(tx);

        let err = lore_storage::defragment::write_to_file(file, SIZE, rx)
            .await
            .expect_err("a hole should be rejected");
        assert!(
            err.to_string().contains("covers 80 of 100 bytes"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn rejects_offset_plus_length_past_end() {
        let dir = TempDir::new("lore-storage-sink-test-");
        let file = target(&dir, "past-end").await;
        let (tx, rx) = channel::<DataMessage>(4);
        send_one(&tx, 95, Bytes::from(vec![0u8; 10])).await; // 95 + 10 > 100
        drop(tx);

        let err = lore_storage::defragment::write_to_file(file, SIZE, rx)
            .await
            .expect_err("OOB should be rejected");
        assert!(
            err.to_string().contains("out of bounds"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn rejects_offset_at_exact_end_with_nonzero_length() {
        let dir = TempDir::new("lore-storage-sink-test-");
        let file = target(&dir, "exact-end").await;
        let (tx, rx) = channel::<DataMessage>(4);
        send_one(&tx, SIZE, Bytes::from(vec![0u8; 1])).await;
        drop(tx);

        lore_storage::defragment::write_to_file(file, SIZE, rx)
            .await
            .expect_err("offset==size with data should be rejected");
    }

    #[tokio::test]
    async fn rejects_arithmetic_overflow() {
        let dir = TempDir::new("lore-storage-sink-test-");
        let file = target(&dir, "overflow").await;
        let (tx, rx) = channel::<DataMessage>(4);
        send_one(&tx, usize::MAX - 5, Bytes::from(vec![0u8; 10])).await;
        drop(tx);

        let err = lore_storage::defragment::write_to_file(file, SIZE, rx)
            .await
            .expect_err("offset + len overflow rejected");
        assert!(
            err.to_string().contains("overflow"),
            "unexpected error: {err}"
        );
    }
}

mod defragment_integration {
    //! End-to-end integration tests that wire a `LocalImmutableStore` with
    //! crafted fragment data and drive the read/defragment pipeline,
    //! covering checks that are only reachable through the full pipeline.
    use std::path::PathBuf;
    use std::sync::Arc;

    use lore_base::test_util::TempDir;
    use lore_storage::StoreError;
    use lore_storage::hash;
    use lore_storage::local::immutable_store::ImmutableStoreSettings;
    use lore_storage::local::immutable_store::LocalImmutableStore;
    use lore_storage::options::ReadOptions;
    use zerocopy::IntoBytes;

    use super::*;

    async fn make_store() -> (TempDir, Arc<dyn ImmutableStore>) {
        let dir = TempDir::new("lore-storage-defrag-test-");
        let store = LocalImmutableStore::new(
            Some(PathBuf::from(dir.as_ref())),
            ImmutableStoreSettings::default(),
        )
        .await
        .expect("create test store");
        (dir, store)
    }

    async fn put_leaf(
        store: &Arc<dyn ImmutableStore>,
        partition: Partition,
        context: Context,
        payload: Vec<u8>,
    ) -> (Address, Fragment) {
        let h = hash::hash_slice(&payload);
        let address = Address { hash: h, context };
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
        (address, fragment)
    }

    /// Build a fragment list placing each address at the given content offset.
    fn refs_at(entries: &[(Address, u64)]) -> Vec<FragmentReference> {
        entries
            .iter()
            .map(|&(address, offset_content)| FragmentReference {
                hash: address.hash,
                offset_content,
            })
            .collect()
    }

    async fn put_list(
        store: &Arc<dyn ImmutableStore>,
        partition: Partition,
        context: Context,
        refs: &[FragmentReference],
        size_content: u64,
    ) -> Address {
        let refs_payload = Bytes::copy_from_slice(refs.as_bytes());
        let root_hash = hash::hash_slice(refs_payload.as_ref());
        let root_address = Address {
            hash: root_hash,
            context,
        };
        let root_fragment = Fragment {
            flags: FragmentFlags::PayloadFragmented.bits(),
            size_payload: refs_payload.len() as u32,
            size_content,
        };
        store
            .clone()
            .put(
                partition,
                root_address,
                root_fragment,
                Some(refs_payload),
                false,
            )
            .await
            .expect("put root list");
        root_address
    }

    /// Leaf A's offset delta claims 200 bytes but its actual payload is
    /// 100. The contiguity check at the fetch pool must reject this.
    /// Exercises the streaming defragment pipeline via `read_into_file`.
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_leaf_with_content_size_below_offset_delta() {
        let (dir, store) = make_store().await;
        let partition = Partition::from([0x01; 16]);
        let context = Context::from([0x01; 16]);

        let (leaf_a_addr, _) = put_leaf(&store, partition, context, vec![0xAA; 100]).await;
        let (leaf_b_addr, _) = put_leaf(&store, partition, context, vec![0xBB; 100]).await;

        // Root list: ref A at offset 0, ref B at offset 200.
        // Implies: leaf A = 200 bytes (actual 100), leaf B = 100 bytes
        // (actual 100, correct). size_content = 300 so last chunk = 100.
        let refs = [
            FragmentReference {
                hash: leaf_a_addr.hash,
                offset_content: 0,
            },
            FragmentReference {
                hash: leaf_b_addr.hash,
                offset_content: 200,
            },
        ];
        let root_address = put_list(&store, partition, context, &refs, 300).await;

        let out_path = dir.join("contiguity-fail.bin");
        let err = lore_storage::read::read_into_file(
            store.clone(),
            partition,
            root_address,
            &out_path,
            ".tmp",
            None,
            ReadOptions::default().no_verify(),
            None,
        )
        .await
        .expect_err("should fail due to contiguity mismatch");

        assert!(
            err.to_string().contains("does not match expected"),
            "unexpected error: {err}"
        );
    }

    /// A consumer that stops reading is not a failure.
    ///
    /// `is_file_content_equal` streams a stored object and compares it with
    /// the file on disk, and stops at the first chunk that differs - which is
    /// the common case, since it only runs when the hashes already disagree.
    /// The pipeline behind it then has nowhere to send the chunks it has
    /// already fetched. That is not an error: there is none to deliver, since
    /// the consumer it would go to is the one that left, and none to log, for
    /// an operation that has done nothing wrong.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stream_whose_consumer_stops_reading_is_not_a_failure() {
        let (_dir, store) = make_store().await;
        let partition = Partition::from([0x0A; 16]);
        let context = Context::from([0x0A; 16]);

        // Enough leaves that the pipeline still has sends to make once the
        // consumer has taken its one chunk and gone.
        let mut refs = Vec::new();
        let mut offset = 0u64;
        for index in 0..16u8 {
            let (address, _) = put_leaf(&store, partition, context, vec![index; 1024]).await;
            refs.push(FragmentReference {
                hash: address.hash,
                offset_content: offset,
            });
            offset += 1024;
        }
        let root_address = put_list(&store, partition, context, &refs, offset).await;

        let options = ReadOptions::default().no_verify().with_decompress();
        let (root_fragment, root_buffer) = lore_storage::read::load_fragment(
            store.clone(),
            partition,
            root_address,
            options,
            None,
        )
        .await
        .expect("load root list");

        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let pipeline = defragment_pipeline(
            store.clone(),
            partition,
            root_address,
            root_fragment,
            root_buffer,
            0..offset,
            DefragmentSink::Stream { sender },
            options,
            None,
        );
        let consumer = async move {
            receiver
                .recv()
                .await
                .expect("first chunk")
                .expect("first chunk is not an error");
            // Dropped here, the way a comparison that has seen enough drops it.
        };

        let (result, ()) = tokio::join!(pipeline, consumer);
        result.expect("an abandoned stream is not a failure");
    }

    /// An abandoned pipeline stops asking for leaves, rather than fetching the
    /// rest of the object into a queue nobody will read.
    ///
    /// The pipeline returning is what proves it: it only returns once its
    /// launcher has, and the launcher only stops when the queue it pushes into
    /// closes. After that the leaf channel is dropped, so a further push into
    /// it must fail. The launcher is left waiting on the leaf channel rather
    /// than on the queue, which is the state it spends its time in and the one
    /// a push cannot reach it in. The timeout is there so that a pipeline which
    /// goes on waiting for leaves fails the test instead of hanging it.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_abandoned_stream_stops_asking_for_leaves() {
        const PAYLOAD: usize = 100;

        let (_dir, store) = make_store().await;
        let partition = Partition::from([0x0B; 16]);
        let context = Context::from([0x0B; 16]);

        let (leaf_tx, leaf_rx) = channel::<LeafReference>(1);
        let (data_tx, mut data_rx) = channel::<Result<Bytes, StorageError>>(1);
        let pipeline = lore_base::lore_spawn!(fetch_ordered_and_stream(
            store.clone(),
            partition,
            leaf_rx,
            data_tx,
            ReadOptions::default().no_verify(),
            None,
        ));

        // Enough queued that the pipeline has something to send after the
        // consumer has taken its one payload and gone.
        let mut first_hash = None;
        for index in 0..4usize {
            let (address, _) =
                put_leaf(&store, partition, context, vec![index as u8; PAYLOAD]).await;
            first_hash.get_or_insert(address.hash);
            leaf_tx
                .send(LeafReference {
                    hash: address.hash,
                    target_offset: (index * PAYLOAD) as u64,
                    expected_size: PAYLOAD as u64,
                    clip: 0..PAYLOAD as u64,
                    context,
                })
                .await
                .expect("queue leaf");
        }

        data_rx
            .recv()
            .await
            .expect("first payload")
            .expect("first payload is not an error");
        drop(data_rx);

        let joined = tokio::time::timeout(std::time::Duration::from_secs(30), pipeline)
            .await
            .expect("the pipeline must return once its consumer is gone")
            .expect("pipeline join");
        joined.expect("an abandoned stream is not a failure");

        assert!(
            leaf_tx
                .send(LeafReference {
                    hash: first_hash.expect("a leaf was queued"),
                    target_offset: 0,
                    expected_size: PAYLOAD as u64,
                    clip: 0..PAYLOAD as u64,
                    context,
                })
                .await
                .is_err(),
            "the pipeline must stop taking leaves once its consumer is gone"
        );
    }

    /// Waits until the pool has taken every leaf, where `capacity` is the channel's whole
    /// capacity and so holding all of it means the channel is empty.
    ///
    /// Awaited rather than sampled. The pool runs on the runtime `lore_spawn!` selects, so
    /// nothing this task does gives it time, and its one blocking step is a budget acquire
    /// against a semaphore the whole process shares. Reserving the capacity parks on the
    /// channel's own wakeup, which the pool triggers as it takes each leaf, so the wait
    /// lasts as long as the pool needs rather than as long as a fixed number of polls.
    async fn drain_leaf_channel(leaf_tx: &Sender<LeafReference>, capacity: usize) {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            leaf_tx.reserve_many(capacity),
        )
        .await
        .expect("the pool must take every queued leaf before the sink goes")
        .expect("the leaf channel must stay open while the pool runs");
    }

    /// The file pool stops asking for leaves once its sink has gone, and reports nothing.
    ///
    /// [`write_to_file`] reads to the end of the channel unless it has already failed,
    /// so a sink that lets go is a sink with an error of its own to report, and a second one
    /// raised here would mask it: [`defragment_pipeline`] combines the three results with
    /// `and`, which keeps the first.
    ///
    /// The sink is dropped only once every queued leaf has been taken and the pool has had
    /// time to park, which is what makes this a test of the wait on the leaf channel rather
    /// than of the budget recheck. With a leaf still in the channel the recheck reaches the
    /// same answer one leaf later, and a pool that had stopped watching its sink would pass.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_abandoned_write_sink_stops_the_fetch_pool() {
        const PAYLOAD: usize = 100;
        const LEAVES: usize = 4;

        let (_dir, store) = make_store().await;
        let partition = Partition::from([0x0E; 16]);
        let context = Context::from([0x0E; 16]);

        let (leaf_tx, leaf_rx) = channel::<LeafReference>(LEAVES);
        let (data_tx, mut data_rx) = channel::<DataMessage>(1);
        let pool = lore_base::lore_spawn!(fetch_unordered(
            store.clone(),
            partition,
            leaf_rx,
            data_tx,
            ReadOptions::default().no_verify(),
            None,
        ));

        // Enough queued that the pool has sends left to make once the sink has taken its
        // one payload and gone.
        let mut first_hash = None;
        for index in 0..LEAVES {
            let (address, _) =
                put_leaf(&store, partition, context, vec![index as u8; PAYLOAD]).await;
            first_hash.get_or_insert(address.hash);
            leaf_tx
                .send(LeafReference {
                    hash: address.hash,
                    target_offset: (index * PAYLOAD) as u64,
                    expected_size: PAYLOAD as u64,
                    clip: 0..PAYLOAD as u64,
                    context,
                })
                .await
                .expect("queue leaf");
        }

        drop(data_rx.recv().await.expect("first payload"));
        drain_leaf_channel(&leaf_tx, LEAVES).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(data_rx);

        tokio::time::timeout(std::time::Duration::from_secs(30), pool)
            .await
            .expect("the pool must return once its sink is gone")
            .expect("pool join")
            .expect("an abandoned write sink is not a failure of the pool feeding it");

        assert!(
            leaf_tx
                .send(LeafReference {
                    hash: first_hash.expect("a leaf was queued"),
                    target_offset: 0,
                    expected_size: PAYLOAD as u64,
                    clip: 0..PAYLOAD as u64,
                    context,
                })
                .await
                .is_err(),
            "the pool must stop taking leaves once its sink is gone"
        );
    }

    /// A reservation that completes after its queue has closed hands back nothing to spend.
    ///
    /// The whole budget is held until after the queue is gone, so the permit can only be
    /// granted once it is. That is the interleaving the recheck exists for: a pool waiting
    /// for budget cannot be told by a send that it has been abandoned, having nothing to
    /// send until it has the budget to fetch what it would send.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_reservation_completing_after_its_queue_closed_yields_nothing() {
        const PAYLOAD: u64 = 100;

        let cost = fragment_permit_count(PAYLOAD as usize);
        let budget: &'static Semaphore = Box::leak(Box::new(Semaphore::new(cost as usize)));
        let held = budget
            .acquire_many(cost)
            .await
            .expect("hold the whole budget");

        let (queue_tx, queue_rx) = channel::<LeafReference>(1);
        let reserve =
            lore_base::lore_spawn!(
                async move { reserve_leaf_budget(budget, &queue_tx, PAYLOAD).await }
            );

        drop(queue_rx);
        drop(held);

        let permit = reserve
            .await
            .expect("reservation join")
            .expect("waiting for budget is not an error");
        assert!(
            permit.is_none(),
            "a reservation for a queue that has closed must hand back no budget to spend"
        );
    }

    /// Handing a payload over holds less than a send of it: it waits for a channel slot and
    /// then sends, so no send future holds the payload across the wait.
    #[test]
    fn a_payload_handover_holds_less_than_a_send() {
        let budget: &'static Semaphore = Box::leak(Box::new(Semaphore::new(1)));
        let permit = budget.try_acquire().expect("an idle budget has a permit");
        let (sender, _receiver) = channel::<Result<Bytes, StorageError>>(1);

        let handover = send_payload(&sender, Bytes::new(), permit);
        let send = sender.send(Ok(Bytes::new()));
        assert!(
            size_of_val(&handover) < size_of_val(&send),
            "handing a payload over holds {} bytes, a send of it {}",
            size_of_val(&handover),
            size_of_val(&send)
        );
    }

    /// A write that fails reports its own error, not the fetch pool's view of it.
    ///
    /// [`defragment_pipeline`] combines the walk, fetch and write results with `and`, which
    /// keeps the first of them, so a pool treating a departed sink as its own failure stands
    /// in front of the error naming the cause. The sink is given room for one leaf so it
    /// rejects an offset past that, and there are more leaves than the data channel holds so
    /// the pool still has sends outstanding when the sink goes.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_write_reports_the_sinks_error_and_not_the_pools() {
        const PAYLOAD: usize = 64;
        let leaves = PIPELINE_DATA_CHANNEL_SIZE + 16;

        let (dir, store) = make_store().await;
        let partition = Partition::from([0x0F; 16]);
        let context = Context::from([0x0F; 16]);

        let mut refs = Vec::with_capacity(leaves);
        let mut offset = 0u64;
        for index in 0..leaves {
            let (address, _) =
                put_leaf(&store, partition, context, vec![index as u8; PAYLOAD]).await;
            refs.push(FragmentReference {
                hash: address.hash,
                offset_content: offset,
            });
            offset += PAYLOAD as u64;
        }
        let root_address = put_list(&store, partition, context, &refs, offset).await;

        let options = ReadOptions::default().no_verify().with_decompress();
        let (root_fragment, root_buffer) = lore_storage::read::load_fragment(
            store.clone(),
            partition,
            root_address,
            options,
            None,
        )
        .await
        .expect("load root list");

        let file =
            lore_storage::defragment::open_file_write(dir.path().join("truncated.bin"), PAYLOAD)
                .await
                .expect("create target file");

        let err = defragment_pipeline(
            store.clone(),
            partition,
            root_address,
            root_fragment,
            root_buffer,
            0..offset,
            DefragmentSink::File {
                file,
                size: PAYLOAD,
            },
            options,
            None,
        )
        .await
        .expect_err("a write past the end of the sink fails the read");

        assert!(
            err.to_string().contains("out of bounds"),
            "the sink's error must be the one reported, got: {err}"
        );
    }

    /// Delegating store that counts the fragment loads reaching it.
    ///
    /// The walk descends by loading list nodes, so the count is what distinguishes a walk
    /// that stopped from one that ran to the end of the tree sending leaves nobody took.
    struct CountingGetStore {
        inner: Arc<dyn ImmutableStore>,
        gets: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ImmutableStore for CountingGetStore {
        async fn get(
            self: Arc<Self>,
            partition: Partition,
            address: Address,
        ) -> Result<lore_storage::store_types::StoreGetData, StoreError> {
            self.gets.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.clone().get(partition, address).await
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

    /// A two-level tree of `SUBLISTS` sublists holding one leaf each, and its root, loaded.
    ///
    /// Wide rather than deep, because what is being counted is how far along a level the
    /// walk gets before it stops, and a level is where the prefetch window lives.
    async fn put_wide_tree(
        store: &Arc<dyn ImmutableStore>,
        partition: Partition,
        context: Context,
        sublists: usize,
        payload: usize,
    ) -> (Address, u64) {
        let mut entries = Vec::with_capacity(sublists);
        let mut offset = 0u64;
        for index in 0..sublists {
            let (leaf, _) = put_leaf(store, partition, context, vec![index as u8; payload]).await;
            let sublist = put_list(
                store,
                partition,
                context,
                &refs_at(&[(leaf, offset)]),
                payload as u64,
            )
            .await;
            entries.push((sublist, offset));
            offset += payload as u64;
        }
        let root = put_list(store, partition, context, &refs_at(&entries), offset).await;
        (root, offset)
    }

    /// A walk with nobody behind it loads nothing.
    ///
    /// The leaf channel is closed before the walk starts, which is the state it reaches the
    /// moment its pipeline is abandoned. Loading even the first sublist would mean the walk
    /// descends before it looks, and a tree deep enough would then be walked to the bottom.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_abandoned_walk_loads_no_list_nodes() {
        const SUBLISTS: usize = 8;
        const PAYLOAD: usize = 100;

        let (_dir, inner) = make_store().await;
        let partition = Partition::from([0x0C; 16]);
        let context = Context::from([0x0C; 16]);
        let (root_address, total) =
            put_wide_tree(&inner, partition, context, SUBLISTS, PAYLOAD).await;

        let gets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let store: Arc<dyn ImmutableStore> = Arc::new(CountingGetStore {
            inner,
            gets: gets.clone(),
        });

        let options = ReadOptions::default().no_verify().with_decompress();
        let (root_fragment, root_buffer) = lore_storage::read::load_fragment(
            store.clone(),
            partition,
            root_address,
            options,
            None,
        )
        .await
        .expect("load root list");
        gets.store(0, std::sync::atomic::Ordering::SeqCst);

        let (leaf_tx, leaf_rx) = channel::<LeafReference>(1);
        drop(leaf_rx);

        walk_fragment_tree(
            store,
            partition,
            root_address,
            root_fragment,
            root_buffer,
            0..total,
            leaf_tx,
            options,
            None,
        )
        .await
        .expect("an abandoned walk is not a failure");

        assert_eq!(
            gets.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "an abandoned walk must not load a single list node"
        );
    }

    /// A walk abandoned part way through a level stops descending the rest of it.
    ///
    /// The consumer takes one leaf and goes, which is where a content comparison stops. What
    /// is left is a level of sublists the walk has every offset for and no reason to load:
    /// the bound is the prefetch window, since loads already in flight when the leaf channel
    /// closes still land.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_walk_abandoned_part_way_stops_descending() {
        const SUBLISTS: usize = 64;
        const PAYLOAD: usize = 100;

        let (_dir, inner) = make_store().await;
        let partition = Partition::from([0x0D; 16]);
        let context = Context::from([0x0D; 16]);
        let (root_address, total) =
            put_wide_tree(&inner, partition, context, SUBLISTS, PAYLOAD).await;

        let gets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let store: Arc<dyn ImmutableStore> = Arc::new(CountingGetStore {
            inner,
            gets: gets.clone(),
        });

        let options = ReadOptions::default().no_verify().with_decompress();
        let (root_fragment, root_buffer) = lore_storage::read::load_fragment(
            store.clone(),
            partition,
            root_address,
            options,
            None,
        )
        .await
        .expect("load root list");
        gets.store(0, std::sync::atomic::Ordering::SeqCst);

        let (leaf_tx, mut leaf_rx) = channel::<LeafReference>(1);
        let walk = lore_base::lore_spawn!(walk_fragment_tree(
            store,
            partition,
            root_address,
            root_fragment,
            root_buffer,
            0..total,
            leaf_tx,
            options,
            None,
        ));

        leaf_rx.recv().await.expect("first leaf");
        drop(leaf_rx);

        tokio::time::timeout(std::time::Duration::from_secs(30), walk)
            .await
            .expect("the walk must return once its consumer is gone")
            .expect("walk join")
            .expect("an abandoned walk is not a failure");

        let loaded = gets.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            loaded < SUBLISTS,
            "an abandoned walk loaded {loaded} fragments, as many as walking all {SUBLISTS} \
                 sublists would take"
        );
    }

    /// Happy path control: matching leaf sizes assemble cleanly.
    #[tokio::test(flavor = "multi_thread")]
    async fn accepts_well_formed_fragment_list() {
        let (dir, store) = make_store().await;
        let partition = Partition::from([0x02; 16]);
        let context = Context::from([0x02; 16]);

        let (leaf_a_addr, _) = put_leaf(&store, partition, context, vec![0xAA; 100]).await;
        let (leaf_b_addr, _) = put_leaf(&store, partition, context, vec![0xBB; 150]).await;

        let refs = [
            FragmentReference {
                hash: leaf_a_addr.hash,
                offset_content: 0,
            },
            FragmentReference {
                hash: leaf_b_addr.hash,
                offset_content: 100,
            },
        ];
        let root_address = put_list(&store, partition, context, &refs, 250).await;

        let out_path = dir.join("well-formed.bin");
        lore_storage::read::read_into_file(
            store.clone(),
            partition,
            root_address,
            &out_path,
            ".tmp",
            None,
            ReadOptions::default().no_verify(),
            None,
        )
        .await
        .expect("well-formed read succeeds");

        let content = std::fs::read(&out_path).expect("read output file");
        assert_eq!(content.len(), 250);
        assert!(content[0..100].iter().all(|&b| b == 0xAA));
        assert!(content[100..250].iter().all(|&b| b == 0xBB));
    }

    /// Mixed-tier attack: a root list claims children are leaves (first
    /// ref points to a real leaf) but a later ref points to an
    /// intermediate fragment list. Without the `PayloadFragmented` check
    /// at the leaf fetch, the intermediate list's reference bytes would
    /// be written at the content offset, silently corrupting output.
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_intermediate_fragment_at_leaf_tier() {
        let (dir, store) = make_store().await;
        let partition = Partition::from([0x03; 16]);
        let context = Context::from([0x03; 16]);

        // Real leaf at offset 0, 100 bytes
        let (leaf_a_addr, _) = put_leaf(&store, partition, context, vec![0xAA; 100]).await;

        // Build a sub-list that also looks like a 100-byte leaf by its
        // size_content (so the contiguity check would pass), but has
        // PayloadFragmented set. The tier check must reject it.
        let (leaf_inner_addr, _) = put_leaf(&store, partition, context, vec![0xBB; 100]).await;
        let sub_refs = [
            FragmentReference {
                hash: leaf_inner_addr.hash,
                offset_content: 100,
            },
            FragmentReference {
                hash: leaf_inner_addr.hash,
                offset_content: 150,
            },
        ];
        let sub_payload = Bytes::copy_from_slice(sub_refs.as_bytes());
        let sub_hash = hash::hash_slice(sub_payload.as_ref());
        let sub_address = Address {
            hash: sub_hash,
            context,
        };
        let sub_fragment = Fragment {
            flags: FragmentFlags::PayloadFragmented.bits(),
            size_payload: sub_payload.len() as u32,
            size_content: 100, // matches the offset delta in the root list below
        };
        store
            .clone()
            .put(
                partition,
                sub_address,
                sub_fragment,
                Some(sub_payload),
                false,
            )
            .await
            .expect("put sub list");

        // Root list: ref A at offset 0 (leaf), ref SUB at offset 100
        // (intermediate). First child is a leaf so walk_fragment_level
        // treats this as a leaf level.
        let refs = [
            FragmentReference {
                hash: leaf_a_addr.hash,
                offset_content: 0,
            },
            FragmentReference {
                hash: sub_hash,
                offset_content: 100,
            },
        ];
        let root_address = put_list(&store, partition, context, &refs, 200).await;

        let out_path = dir.join("mixed-tier.bin");
        let err = lore_storage::read::read_into_file(
            store.clone(),
            partition,
            root_address,
            &out_path,
            ".tmp",
            None,
            ReadOptions::default().no_verify(),
            None,
        )
        .await
        .expect_err("should reject mixed-tier list");

        assert!(
            err.to_string().contains("intermediate fragment list"),
            "unexpected error: {err}"
        );
    }

    /// Recursion depth limit: a fragment tree deeper than
    /// `MAX_FRAGMENT_TREE_DEPTH` levels must be rejected. Build a chain
    /// of single-reference intermediate lists; each level adds one to
    /// the depth counter.
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_tree_exceeding_recursion_depth() {
        let (dir, store) = make_store().await;
        let partition = Partition::from([0x04; 16]);
        let context = Context::from([0x04; 16]);

        // Bottom leaf (depth = 0 of the actual data)
        let (leaf_addr, _) = put_leaf(&store, partition, context, vec![0xCC; 64]).await;

        // Build a chain of intermediate lists wrapping the leaf. Each intermediate holds a
        // single reference covering its parent's whole window, which is the only shape a
        // deep chain can take: sublist offsets are absolute, so one child hash cannot sit
        // at two offsets, and a list whose entries disagree with the sizes their parent
        // allots them is rejected on the way down before the depth is ever reached.
        //
        // Every wrap adds one level of walk_fragment_level recursion, and
        // MAX_FRAGMENT_TREE_DEPTH is 8, so a dozen wraps is comfortably past it.
        let mut current_hash = leaf_addr.hash;
        for _ in 0..12 {
            let refs = [FragmentReference {
                hash: current_hash,
                offset_content: 0,
            }];
            let payload = Bytes::copy_from_slice(refs.as_bytes());
            let h = hash::hash_slice(payload.as_ref());
            let addr = Address { hash: h, context };
            let frag = Fragment {
                flags: FragmentFlags::PayloadFragmented.bits(),
                size_payload: payload.len() as u32,
                size_content: 64,
            };
            store
                .clone()
                .put(partition, addr, frag, Some(payload), false)
                .await
                .expect("put intermediate");
            current_hash = h;
        }
        let root_address = Address {
            hash: current_hash,
            context,
        };

        let out_path = dir.join("deep-tree.bin");
        let err = lore_storage::read::read_into_file(
            store.clone(),
            partition,
            root_address,
            &out_path,
            ".tmp",
            None,
            ReadOptions::default().no_verify(),
            None,
        )
        .await
        .expect_err("should reject tree exceeding recursion depth");

        assert!(
            err.to_string().contains("recursion depth exceeded"),
            "unexpected error: {err}"
        );
    }

    /// Control for the tiling checks below: a two-level tree whose sublists tile their
    /// parent exactly must still read back byte for byte. Every tree the writer
    /// produces has this shape, so a check that rejected it would make existing
    /// repositories unreadable.
    #[tokio::test(flavor = "multi_thread")]
    async fn accepts_a_two_level_tree_that_tiles() {
        let (dir, store) = make_store().await;
        let partition = Partition::from([0x05; 16]);
        let context = Context::from([0x05; 16]);

        let (leaf_a, _) = put_leaf(&store, partition, context, vec![0xAA; 100]).await;
        let (leaf_b, _) = put_leaf(&store, partition, context, vec![0xBB; 150]).await;

        let sub_a = put_list(&store, partition, context, &refs_at(&[(leaf_a, 0)]), 100).await;
        let sub_b = put_list(&store, partition, context, &refs_at(&[(leaf_b, 100)]), 150).await;
        let root = put_list(
            &store,
            partition,
            context,
            &refs_at(&[(sub_a, 0), (sub_b, 100)]),
            250,
        )
        .await;

        let out_path = dir.join("two-level.bin");
        lore_storage::read::read_into_file(
            store.clone(),
            partition,
            root,
            &out_path,
            ".tmp",
            None,
            ReadOptions::default().no_verify(),
            None,
        )
        .await
        .expect("well-formed two-level read succeeds");

        let content = std::fs::read(&out_path).expect("read output file");
        assert_eq!(content.len(), 250);
        assert!(content[0..100].iter().all(|&b| b == 0xAA));
        assert!(content[100..250].iter().all(|&b| b == 0xBB));
    }

    /// Sibling sublists that skip a range: the second starts past where the first
    /// ended, so [100, 200) is claimed by nobody. Without the tiling check the read
    /// succeeds and the gap is zeros, because the target file is sized up front.
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_sibling_sublists_that_leave_a_hole() {
        let (dir, store) = make_store().await;
        let partition = Partition::from([0x06; 16]);
        let context = Context::from([0x06; 16]);

        let (leaf_a, _) = put_leaf(&store, partition, context, vec![0xAA; 100]).await;
        let (leaf_b, _) = put_leaf(&store, partition, context, vec![0xBB; 100]).await;

        let sub_a = put_list(&store, partition, context, &refs_at(&[(leaf_a, 0)]), 100).await;
        let sub_b = put_list(&store, partition, context, &refs_at(&[(leaf_b, 200)]), 100).await;
        let root = put_list(
            &store,
            partition,
            context,
            &refs_at(&[(sub_a, 0), (sub_b, 200)]),
            300,
        )
        .await;

        let out_path = dir.join("sibling-hole.bin");
        let err = lore_storage::read::read_into_file(
            store.clone(),
            partition,
            root,
            &out_path,
            ".tmp",
            None,
            ReadOptions::default().no_verify(),
            None,
        )
        .await
        .expect_err("a gap between siblings should be rejected");

        assert!(
            err.to_string()
                .contains("expands to 100 bytes but its parent's list gives it 200"),
            "unexpected error: {err}"
        );
    }

    /// Sublists that tile from the start but stop short of the parent's declared size.
    /// The hole is the tail of the file rather than a gap in the middle, and reads back
    /// the same way: zeros, no error.
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_sublists_that_stop_short_of_the_parent() {
        let (dir, store) = make_store().await;
        let partition = Partition::from([0x07; 16]);
        let context = Context::from([0x07; 16]);

        let (leaf_a, _) = put_leaf(&store, partition, context, vec![0xAA; 100]).await;
        let (leaf_b, _) = put_leaf(&store, partition, context, vec![0xBB; 100]).await;

        let sub_a = put_list(&store, partition, context, &refs_at(&[(leaf_a, 0)]), 100).await;
        let sub_b = put_list(&store, partition, context, &refs_at(&[(leaf_b, 100)]), 100).await;
        let root = put_list(
            &store,
            partition,
            context,
            &refs_at(&[(sub_a, 0), (sub_b, 100)]),
            300,
        )
        .await;

        let out_path = dir.join("short-tail.bin");
        let err = lore_storage::read::read_into_file(
            store.clone(),
            partition,
            root,
            &out_path,
            ".tmp",
            None,
            ReadOptions::default().no_verify(),
            None,
        )
        .await
        .expect_err("a short tail should be rejected");

        assert!(
            err.to_string().contains(
                "at content offset 100 expands to 100 bytes but its parent's list gives it 200"
            ),
            "unexpected error: {err}"
        );
    }

    /// An empty first sublist. Accepting it stands for the whole level, so a root
    /// claiming 200 bytes yields a wholly zero-filled file and its second sublist is
    /// never looked at.
    ///
    /// The empty list is a payload too short to hold one `FragmentReference` rather
    /// than a zero-length one, because the store rejects `size_payload == 0` at `put`.
    /// `as_type_slice` rounds down, so any payload under 40 bytes reads as no entries.
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_an_empty_sublist() {
        let (dir, store) = make_store().await;
        let partition = Partition::from([0x08; 16]);
        let context = Context::from([0x08; 16]);

        let (leaf_b, _) = put_leaf(&store, partition, context, vec![0xBB; 100]).await;

        let stub = Bytes::from_static(&[0u8; 8]);
        let sub_empty = Address {
            hash: hash::hash_slice(stub.as_ref()),
            context,
        };
        store
            .clone()
            .put(
                partition,
                sub_empty,
                Fragment {
                    flags: FragmentFlags::PayloadFragmented.bits(),
                    size_payload: stub.len() as u32,
                    size_content: 100,
                },
                Some(stub),
                false,
            )
            .await
            .expect("put empty sublist");
        let sub_b = put_list(&store, partition, context, &refs_at(&[(leaf_b, 100)]), 100).await;
        let root = put_list(
            &store,
            partition,
            context,
            &refs_at(&[(sub_empty, 0), (sub_b, 100)]),
            200,
        )
        .await;

        let out_path = dir.join("empty-sublist.bin");
        let err = lore_storage::read::read_into_file(
            store.clone(),
            partition,
            root,
            &out_path,
            ".tmp",
            None,
            ReadOptions::default().no_verify(),
            None,
        )
        .await
        .expect_err("an empty sublist should be rejected");

        assert!(
            err.to_string().contains("is empty"),
            "unexpected error: {err}"
        );
    }

    /// A sublist that expands to zero bytes. Like an empty list, it stands in for no
    /// content at all, which is the zero hash's job and never a list's.
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_a_sublist_that_expands_to_nothing() {
        let (dir, store) = make_store().await;
        let partition = Partition::from([0x0C; 16]);
        let context = Context::from([0x0C; 16]);

        let (leaf_a, _) = put_leaf(&store, partition, context, vec![0xAA; 100]).await;
        let (leaf_b, _) = put_leaf(&store, partition, context, vec![0xBB; 100]).await;

        // Distinct payloads: two lists differing only in `size_content` hash the same
        // and the store rejects the second as a collision.
        //
        // The root's own entries are strictly increasing, so the only fault in this tree is
        // the one under test. A root placing both sublists at the same offset would be
        // rejected for that instead, before either is ever loaded.
        let sub_zero = put_list(&store, partition, context, &refs_at(&[(leaf_b, 0)]), 0).await;
        let sub_a = put_list(&store, partition, context, &refs_at(&[(leaf_a, 100)]), 100).await;
        let root = put_list(
            &store,
            partition,
            context,
            &refs_at(&[(sub_zero, 0), (sub_a, 100)]),
            200,
        )
        .await;

        let out_path = dir.join("zero-expansion.bin");
        let err = lore_storage::read::read_into_file(
            store.clone(),
            partition,
            root,
            &out_path,
            ".tmp",
            None,
            ReadOptions::default().no_verify(),
            None,
        )
        .await
        .expect_err("a sublist expanding to nothing should be rejected");

        assert!(
            err.to_string().contains("expands to zero bytes"),
            "unexpected error: {err}"
        );
    }

    /// A zero hash in a list addresses zero-length content, which is never a fragment.
    /// `load_fragment` answers it with a default `Fragment`, so an unchecked entry in the
    /// first position would make a level of intermediate references read as leaves.
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_a_zero_hash_in_a_fragment_list() {
        let (dir, store) = make_store().await;
        let partition = Partition::from([0x0D; 16]);
        let context = Context::from([0x0D; 16]);

        let (leaf_a, _) = put_leaf(&store, partition, context, vec![0xAA; 100]).await;
        let zero = Address {
            hash: Hash::default(),
            context,
        };
        let root = put_list(
            &store,
            partition,
            context,
            &refs_at(&[(leaf_a, 0), (zero, 100)]),
            200,
        )
        .await;

        let out_path = dir.join("zero-hash.bin");
        let err = lore_storage::read::read_into_file(
            store.clone(),
            partition,
            root,
            &out_path,
            ".tmp",
            None,
            ReadOptions::default().no_verify(),
            None,
        )
        .await
        .expect_err("a zero hash in a list should be rejected");

        assert!(
            err.to_string()
                .contains("entry 1 at content offset 100 has a zero hash"),
            "unexpected error: {err}"
        );
    }

    /// The same rule where the zero hash stands in the intermediate position: a sibling
    /// of a real sublist. The load answers with an empty default fragment, so without
    /// the check the entry is reported as an empty sublist rather than as what it is.
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_a_zero_hash_among_intermediate_entries() {
        let (dir, store) = make_store().await;
        let partition = Partition::from([0x0F; 16]);
        let context = Context::from([0x0F; 16]);

        let (leaf_a, _) = put_leaf(&store, partition, context, vec![0xAA; 100]).await;
        let zero = Address {
            hash: Hash::default(),
            context,
        };

        let sub_a = put_list(&store, partition, context, &refs_at(&[(leaf_a, 0)]), 100).await;
        let root = put_list(
            &store,
            partition,
            context,
            &refs_at(&[(sub_a, 0), (zero, 100)]),
            200,
        )
        .await;

        let out_path = dir.join("zero-hash-intermediate.bin");
        let err = lore_storage::read::read_into_file(
            store.clone(),
            partition,
            root,
            &out_path,
            ".tmp",
            None,
            ReadOptions::default().no_verify(),
            None,
        )
        .await
        .expect_err("a zero hash among intermediate entries should be rejected");

        assert!(
            err.to_string()
                .contains("entry at content offset 100 has a zero hash"),
            "unexpected error: {err}"
        );
    }

    /// The same rule one level down, where the sublist reaches `walk_leaf_level`
    /// straight from `walk_intermediate_level` and never passes the root's check.
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_a_zero_hash_inside_a_sublist() {
        let (dir, store) = make_store().await;
        let partition = Partition::from([0x0E; 16]);
        let context = Context::from([0x0E; 16]);

        let (leaf_a, _) = put_leaf(&store, partition, context, vec![0xAA; 100]).await;
        let zero = Address {
            hash: Hash::default(),
            context,
        };

        let sub_a = put_list(&store, partition, context, &refs_at(&[(leaf_a, 0)]), 100).await;
        let sub_zero = put_list(&store, partition, context, &refs_at(&[(zero, 100)]), 100).await;
        let root = put_list(
            &store,
            partition,
            context,
            &refs_at(&[(sub_a, 0), (sub_zero, 100)]),
            200,
        )
        .await;

        let out_path = dir.join("zero-hash-sublist.bin");
        let err = lore_storage::read::read_into_file(
            store.clone(),
            partition,
            root,
            &out_path,
            ".tmp",
            None,
            ReadOptions::default().no_verify(),
            None,
        )
        .await
        .expect_err("a zero hash inside a sublist should be rejected");

        assert!(
            err.to_string().contains("has a zero hash"),
            "unexpected error: {err}"
        );
    }

    /// The other half of the same acceptance: an empty list at the root, where there is
    /// no parent entry to check it against. `walk_fragment_level` returned `Ok` and the
    /// pipeline wrote nothing at all into a file already sized to 100 bytes.
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_an_empty_root_list() {
        let (dir, store) = make_store().await;
        let partition = Partition::from([0x0B; 16]);
        let context = Context::from([0x0B; 16]);

        let stub = Bytes::from_static(&[1u8; 8]);
        let root = Address {
            hash: hash::hash_slice(stub.as_ref()),
            context,
        };
        store
            .clone()
            .put(
                partition,
                root,
                Fragment {
                    flags: FragmentFlags::PayloadFragmented.bits(),
                    size_payload: stub.len() as u32,
                    size_content: 100,
                },
                Some(stub),
                false,
            )
            .await
            .expect("put empty root list");

        let out_path = dir.join("empty-root.bin");
        let err = lore_storage::read::read_into_file(
            store.clone(),
            partition,
            root,
            &out_path,
            ".tmp",
            None,
            ReadOptions::default().no_verify(),
            None,
        )
        .await
        .expect_err("an empty root list should be rejected");

        assert!(
            err.to_string().contains("fragment list is empty"),
            "unexpected error: {err}"
        );
    }

    /// A sublist whose own first offset disagrees with where its parent places it. The
    /// leaves are then written at offsets the parent never accounted for, which both
    /// leaves a hole and overwrites a sibling's range.
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_a_sublist_that_disagrees_with_its_parent_offset() {
        let (dir, store) = make_store().await;
        let partition = Partition::from([0x09; 16]);
        let context = Context::from([0x09; 16]);

        let (leaf_a, _) = put_leaf(&store, partition, context, vec![0xAA; 100]).await;
        let (leaf_b, _) = put_leaf(&store, partition, context, vec![0xBB; 100]).await;

        let sub_a = put_list(&store, partition, context, &refs_at(&[(leaf_a, 0)]), 100).await;
        // Parent places this at 100; the sublist itself claims to start at 0.
        let sub_b = put_list(&store, partition, context, &refs_at(&[(leaf_b, 0)]), 100).await;
        let root = put_list(
            &store,
            partition,
            context,
            &refs_at(&[(sub_a, 0), (sub_b, 100)]),
            200,
        )
        .await;

        let out_path = dir.join("offset-disagreement.bin");
        let err = lore_storage::read::read_into_file(
            store.clone(),
            partition,
            root,
            &out_path,
            ".tmp",
            None,
            ReadOptions::default().no_verify(),
            None,
        )
        .await
        .expect_err("a sublist contradicting its parent should be rejected");

        assert!(
            err.to_string().contains("parent entry places it at 100"),
            "unexpected error: {err}"
        );
    }

    /// A payload must stay charged to the fragment budget until the caller has taken
    /// it. Releasing at load bounds only the fetch, leaving the payloads themselves to
    /// pile up in a queue sized for 262,144 of them.
    ///
    /// Budget for two payloads, four leaves, and a caller that stops consuming: the
    /// pipeline must run out of budget and stay out of it. With the permit released at
    /// load, all four load, all four permits come back, and the budget reads full while
    /// nothing has been delivered.
    #[tokio::test(flavor = "multi_thread")]
    async fn payloads_stay_charged_to_the_budget_until_the_caller_takes_them() {
        const LEAVES: usize = 4;
        const PAYLOAD: usize = 100;
        let charged = 2 * FRAGMENT_MINIMUM_COST_KIB as usize;

        let (_dir, store) = make_store().await;
        let partition = Partition::from([0x0A; 16]);
        let context = Context::from([0x0A; 16]);

        // Leaked so the pipeline can hold `SemaphorePermit<'static>` against a budget
        // this test owns; sampling the global one is unreliable because every other
        // test in the binary draws on it.
        let budget: &'static Semaphore = Box::leak(Box::new(Semaphore::new(charged)));

        let (leaf_tx, leaf_rx) = channel::<LeafReference>(LEAVES);
        for index in 0..LEAVES {
            let (address, _) =
                put_leaf(&store, partition, context, vec![index as u8; PAYLOAD]).await;
            leaf_tx
                .send(LeafReference {
                    hash: address.hash,
                    target_offset: (index * PAYLOAD) as u64,
                    expected_size: PAYLOAD as u64,
                    clip: 0..PAYLOAD as u64,
                    context,
                })
                .await
                .expect("queue leaf");
        }
        drop(leaf_tx);

        // One slot, so only the first payload leaves the pipeline's accounting.
        let (data_tx, mut data_rx) = channel::<Result<Bytes, StorageError>>(1);
        let pipeline = lore_base::lore_spawn!(fetch_ordered_and_stream_from(
            budget,
            store.clone(),
            partition,
            leaf_rx,
            data_tx,
            ReadOptions::default().no_verify(),
            None,
        ));

        // Wait for the pipeline to reach the budget, rather than assuming it got there.
        let mut waited = 0;
        while budget.available_permits() > 0 && waited < 100 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            waited += 1;
        }
        assert_eq!(
            budget.available_permits(),
            0,
            "pipeline never took the budget it needs for payloads it is holding"
        );

        // And stays there: the state above is momentary while permits are released at
        // load, permanent while they travel with the payload.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(
            budget.available_permits(),
            0,
            "budget came back while payloads were still undelivered"
        );

        // Draining releases them in order, and the walk completes.
        for index in 0..LEAVES {
            let payload = data_rx
                .recv()
                .await
                .expect("payload")
                .expect("payload must not be an error");
            assert_eq!(payload.len(), PAYLOAD);
            assert!(
                payload.iter().all(|&byte| byte == index as u8),
                "payloads must arrive in list order"
            );
        }
        pipeline
            .await
            .expect("pipeline join")
            .expect("pipeline result");
        assert_eq!(
            budget.available_permits(),
            charged,
            "every permit must come back once the payloads are delivered"
        );
    }

    /// Stores `part` in `mode` under the hash of its content, returning the fragment and payload
    /// stored; `NoCompression` stores it as it is.
    async fn put_stored_leaf(
        store: &Arc<dyn ImmutableStore>,
        partition: Partition,
        context: Context,
        part: &[u8],
        mode: lore_storage::CompressionMode,
    ) -> (Address, Fragment, Bytes) {
        let raw = Fragment {
            flags: 0,
            size_payload: part.len() as u32,
            size_content: part.len() as u64,
        };
        let (fragment, payload) = match mode {
            lore_storage::CompressionMode::NoCompression => (raw, Bytes::copy_from_slice(part)),
            mode => lore_storage::compress(raw, part, mode).expect("compress leaf"),
        };
        let address = Address {
            hash: hash::hash_slice(part),
            context,
        };
        store
            .clone()
            .put(partition, address, fragment, Some(payload.clone()), false)
            .await
            .expect("put leaf");
        (address, fragment, payload)
    }

    /// The root list over `entries` for `size_content` bytes, as the leaf pipeline takes it.
    fn list_over(entries: &[(Address, u64)], size_content: u64) -> (Fragment, Bytes) {
        let list = Bytes::copy_from_slice(refs_at(entries).as_bytes());
        let fragment = Fragment {
            flags: FragmentFlags::PayloadFragmented.bits(),
            size_payload: list.len() as u32,
            size_content,
        };
        (fragment, list)
    }

    /// The leaf pipeline delivers every leaf whole and in content order with its fragment: as stored
    /// without `decompress`, whatever its codec, and expanded with it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_leaf_pipeline_delivers_leaves_as_the_options_ask() {
        use lore_storage::CompressionMode;

        let (_dir, store) = make_store().await;
        let partition = Partition::from([0x0C; 16]);
        let context = Context::from([0x0C; 16]);
        let content: Vec<u8> = (0..3 * 4096u32).map(|i| (i % 7) as u8).collect();

        let mut entries = Vec::new();
        let mut leaves = Vec::new();
        for (index, mode) in [
            CompressionMode::Zstd,
            CompressionMode::Lz4,
            CompressionMode::NoCompression,
        ]
        .into_iter()
        .enumerate()
        {
            let part = &content[index * 4096..(index + 1) * 4096];
            let (address, fragment, payload) =
                put_stored_leaf(&store, partition, context, part, mode).await;
            entries.push((address, (index * 4096) as u64));
            leaves.push((fragment, payload));
        }
        assert_ne!(leaves[0].0.flags & FragmentFlags::PayloadCompressed, 0);
        assert_ne!(leaves[1].0.flags & FragmentFlags::PayloadCompressed, 0);

        for options in [
            ReadOptions::default().no_decompress().no_verify(),
            ReadOptions::default(),
        ] {
            let (fragment, list) = list_over(&entries, content.len() as u64);
            let (tx, mut rx) = channel(entries.len());
            defragment_pipeline_leaves(
                store.clone(),
                partition,
                Address::default(),
                fragment,
                list,
                tx,
                options,
                None,
            )
            .await
            .expect("leaf pipeline");

            let mut delivered = Vec::new();
            while let Ok(leaf) = rx.try_recv() {
                delivered.push(leaf.expect("leaf"));
            }
            assert_eq!(delivered.len(), 3);
            for (index, (fragment, payload)) in delivered.into_iter().enumerate() {
                if options.decompress {
                    assert_eq!(fragment.flags & FragmentFlags::PayloadCompressed, 0);
                    assert_eq!(payload, content[index * 4096..(index + 1) * 4096]);
                } else {
                    assert_eq!((fragment, payload), leaves[index], "leaf {index} as stored");
                }
            }
        }
    }

    /// A leaf the store does not hold fails the leaf pipeline rather than ending it short.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_leaf_pipeline_fails_on_a_missing_leaf() {
        let (_dir, store) = make_store().await;
        let partition = Partition::from([0x0D; 16]);
        let context = Context::from([0x0D; 16]);
        let (present, _) = put_leaf(&store, partition, context, vec![0xAA; 100]).await;
        let missing = Address {
            hash: hash::hash_slice(&[0xBB; 100]),
            context,
        };

        let (fragment, list) = list_over(&[(present, 0), (missing, 100)], 200);
        let (tx, _rx) = channel(2);
        let result = defragment_pipeline_leaves(
            store,
            partition,
            Address::default(),
            fragment,
            list,
            tx,
            ReadOptions::default().no_decompress().no_verify(),
            None,
        )
        .await;
        assert!(result.is_err());
    }
}

mod whole_leaf {
    use super::*;

    fn zstd(size_payload: u32, size_content: u64) -> Fragment {
        Fragment {
            flags: FragmentFlags::PayloadCompressedZstd.bits(),
            size_payload,
            size_content,
        }
    }

    /// A whole leaf whose fragment agrees with its parent list and its payload goes out as loaded.
    #[test]
    fn a_whole_leaf_goes_out_as_loaded() {
        let payload = Bytes::from_static(&[7u8; 10]);
        let leaf = whole_leaf(zstd(10, 64), payload.clone(), 64, 0..64).unwrap();
        assert_eq!(leaf, (zstd(10, 64), payload));
    }

    /// A leaf is refused when its fragment states another content size than its parent list, when
    /// only part of it was asked for, when its payload is not the size its fragment states, or when
    /// it is uncompressed and its fragment's sizes disagree.
    #[test]
    fn a_leaf_that_cannot_go_out_whole_and_as_described_is_refused() {
        let payload = Bytes::from_static(&[7u8; 10]);
        let uncompressed = Fragment {
            flags: 0,
            size_payload: 10,
            size_content: 64,
        };
        assert!(whole_leaf(zstd(10, 63), payload.clone(), 64, 0..64).is_err());
        assert!(whole_leaf(zstd(10, 64), payload.clone(), 64, 8..64).is_err());
        assert!(whole_leaf(zstd(10, 64), payload.clone(), 64, 0..32).is_err());
        assert!(whole_leaf(zstd(11, 64), payload.clone(), 64, 0..64).is_err());
        assert!(whole_leaf(uncompressed, payload, 64, 0..64).is_err());
    }
}
