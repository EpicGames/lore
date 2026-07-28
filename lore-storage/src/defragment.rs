// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use bytes::BytesMut;
use lore_transport::StorageSession;
use memmap2::Mmap;
use memmap2::MmapMut;
use tokio::fs::File;
use tokio::io::AsyncSeekExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::channel;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;

use crate::concurrency::FRAGMENT_BUDGET_KIB;
use crate::concurrency::FRAGMENT_MINIMUM_COST_KIB;
use crate::concurrency::fragment_limiter;
use crate::concurrency::fragment_permit_count;
use crate::error::StorageError;
use crate::fragment_flags::FragmentFlags;
use crate::immutable_store::ImmutableStore;
use crate::options::ReadOptions;
use crate::read::load_fragment;
use crate::typed_bytes::TypedBytes;
use crate::types::Address;
use crate::types::Context;
use crate::types::Fragment;
use crate::types::FragmentReference;
use crate::types::Hash;
use crate::types::Partition;

/// Target for the streaming defragmentation pipeline.
#[derive(Clone)]
pub enum DefragmentSink {
    /// Write at offset to a memory-mapped file (unordered, concurrent writes).
    Mmap { ptr: *mut u8, len: usize },
    /// Write at offset to a file via seek+write (unordered, mutex-serialized).
    File { file: Arc<Mutex<File>> },
    /// Stream buffers in content order to a caller-provided channel.
    Stream { sender: Sender<Bytes> },
}

// SAFETY: Same invariants as the original — the mmap pointer outlives
// all tasks using this sink and writes target non-overlapping regions.
unsafe impl Send for DefragmentSink {}
unsafe impl Sync for DefragmentSink {}

/// Leaf fragment reference yielded by the tree walker to the fetch pool.
#[cfg_attr(test, derive(Debug))]
struct LeafReference {
    hash: Hash,
    offset_content: u64,
    expected_size: u64,
    context: Context,
}

/// Channel capacity for leaf references from walker to fetch pool.
const PIPELINE_LEAF_CHANNEL_SIZE: usize = 512;

/// Channel capacity for fetched data from fetch pool to write sink.
const PIPELINE_DATA_CHANNEL_SIZE: usize = 128;

/// Prefetch window for intermediate fragment loading at each tree level.
const PIPELINE_WALKER_LOOKAHEAD: usize = 8;

/// Maximum recursion depth when walking an intermediate fragment tree.
/// A legitimate tree for even petabyte-scale content only needs a handful of
/// levels (6553 refs per intermediate × 256 KiB leaves = 1.6 GiB per
/// intermediate; three levels already reach multi-petabyte). Bounding the
/// recursion prevents a hostile peer from forcing a large number of fragment
/// fetches on a deeply nested tree.
const MAX_FRAGMENT_TREE_DEPTH: usize = 8;

/// Walks the fragment tree depth-first with prefetch pipelining, yielding leaf
/// fragment references into the provided channel.
#[allow(clippy::too_many_arguments)]
async fn walk_fragment_tree(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    fragment: Fragment,
    source_buffer: Bytes,
    leaf_tx: Sender<LeafReference>,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<(), StorageError> {
    debug_assert!(
        (fragment.flags & FragmentFlags::PayloadFragmented) == FragmentFlags::PayloadFragmented
    );

    let payload_size = fragment.size_payload as usize;
    if source_buffer.len() < payload_size {
        return Err(StorageError::internal("insufficient buffer"));
    }

    let source_buffer = source_buffer.to_aligned::<FragmentReference>();
    let fragment_list = source_buffer.as_type_slice::<FragmentReference>();
    let total_content_size = fragment.size_content as usize;

    walk_fragment_level(
        store,
        partition,
        address.context,
        fragment_list,
        total_content_size,
        &leaf_tx,
        options,
        remote_session,
        0,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn walk_fragment_level(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
    fragment_list: &[FragmentReference],
    total_content_size: usize,
    leaf_tx: &Sender<LeafReference>,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
    depth: usize,
) -> Result<(), StorageError> {
    if depth > MAX_FRAGMENT_TREE_DEPTH {
        return Err(StorageError::internal(format!(
            "fragment tree recursion depth exceeded {MAX_FRAGMENT_TREE_DEPTH}"
        )));
    }

    if fragment_list.is_empty() {
        return Ok(());
    }

    let base_offset = fragment_list[0].offset_content;

    // Peek at the first entry to determine if this level is intermediate or leaf
    let first_address = Address {
        context,
        hash: fragment_list[0].hash,
    };
    let (first_frag, first_buf) = load_fragment(
        store.clone(),
        partition,
        first_address,
        options,
        remote_session.clone(),
    )
    .await?;

    if (first_frag.flags & FragmentFlags::PayloadFragmented) == FragmentFlags::PayloadFragmented {
        walk_intermediate_level(
            store,
            partition,
            context,
            fragment_list,
            first_frag,
            first_buf,
            leaf_tx,
            options,
            remote_session,
            depth,
        )
        .await
    } else {
        drop(first_buf);
        walk_leaf_level(
            fragment_list,
            total_content_size,
            base_offset,
            context,
            leaf_tx,
        )
        .await
    }
}

/// Yields all entries in a leaf-level fragment list as `LeafReference`.
///
/// Uses checked arithmetic on `offset_content` so a peer-supplied list with
/// non-increasing offsets, offsets outside the content window, or a total
/// span that overflows u64 fails with a clear error rather than producing a
/// wrapped `expected_size` that would blow up downstream permit accounting
/// or mmap writes.
async fn walk_leaf_level(
    fragment_list: &[FragmentReference],
    total_content_size: usize,
    base_offset: u64,
    context: Context,
    leaf_tx: &Sender<LeafReference>,
) -> Result<(), StorageError> {
    let content_end = base_offset
        .checked_add(total_content_size as u64)
        .ok_or_else(|| {
            StorageError::internal("fragment list base_offset + total_content_size overflows u64")
        })?;

    for (i, frag_ref) in fragment_list.iter().enumerate() {
        let next_offset = if i + 1 < fragment_list.len() {
            fragment_list[i + 1].offset_content
        } else {
            content_end
        };
        let expected_content_size = next_offset
            .checked_sub(frag_ref.offset_content)
            .ok_or_else(|| {
                StorageError::internal(
                    "fragment list offset_content is not strictly increasing inside content window",
                )
            })?;
        if expected_content_size > crate::FRAGMENT_SIZE_THRESHOLD as u64 {
            return Err(StorageError::internal(format!(
                "fragment list chunk size {expected_content_size} exceeds FRAGMENT_SIZE_THRESHOLD {}",
                crate::FRAGMENT_SIZE_THRESHOLD
            )));
        }
        if expected_content_size == 0 {
            return Err(StorageError::internal("fragment list chunk has zero size"));
        }

        let leaf = LeafReference {
            hash: frag_ref.hash,
            offset_content: frag_ref.offset_content,
            expected_size: expected_content_size,
            context,
        };
        if leaf_tx.send(leaf).await.is_err() {
            break;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn walk_intermediate_level(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
    fragment_list: &[FragmentReference],
    first_frag: Fragment,
    first_buf: Bytes,
    leaf_tx: &Sender<LeafReference>,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
    depth: usize,
) -> Result<(), StorageError> {
    // Parse the already-loaded first entry
    let first_content_size = first_frag.size_content as usize;
    let first_payload_size = first_frag.size_payload as usize;
    if first_buf.len() < first_payload_size {
        return Err(StorageError::internal("insufficient buffer"));
    }
    let first_buffer = first_buf.to_aligned::<FragmentReference>();
    let first_list = first_buffer.as_type_slice::<FragmentReference>();

    if first_list.is_empty() {
        return Ok(());
    }

    // Determine sub-level type by peeking at the first child
    let peek_address = Address {
        context,
        hash: first_list[0].hash,
    };
    let (peek_frag, peek_buf) = load_fragment(
        store.clone(),
        partition,
        peek_address,
        options,
        remote_session.clone(),
    )
    .await?;
    let children_are_leaves =
        (peek_frag.flags & FragmentFlags::PayloadFragmented) != FragmentFlags::PayloadFragmented;
    drop(peek_buf);

    // Process first entry
    let first_base_offset = first_list[0].offset_content;
    let mut result = if children_are_leaves {
        walk_leaf_level(
            first_list,
            first_content_size,
            first_base_offset,
            context,
            leaf_tx,
        )
        .await
    } else {
        Box::pin(walk_fragment_level(
            store.clone(),
            partition,
            context,
            first_list,
            first_content_size,
            leaf_tx,
            options,
            remote_session.clone(),
            depth + 1,
        ))
        .await
    };

    if fragment_list.len() <= 1 || result.is_err() {
        return result;
    }

    // Prefetch remaining intermediate entries
    let (prefetch_tx, mut prefetch_rx) =
        channel::<JoinHandle<Result<(Fragment, Bytes), StorageError>>>(PIPELINE_WALKER_LOOKAHEAD);

    let launcher: JoinHandle<Result<(), StorageError>> = {
        let store = store.clone();
        let remote_session = remote_session.clone();
        let remaining: Vec<FragmentReference> = fragment_list[1..].to_vec();
        lore_base::lore_spawn!(async move {
            for frag_ref in &remaining {
                let subaddress = Address {
                    context,
                    hash: frag_ref.hash,
                };
                let store = store.clone();
                let remote_session = remote_session.clone();
                let handle: JoinHandle<Result<(Fragment, Bytes), StorageError>> =
                    lore_base::lore_spawn!(async move {
                        load_fragment(store, partition, subaddress, options, remote_session).await
                    });

                if prefetch_tx.send(handle).await.is_err() {
                    break;
                }
            }
            Ok(())
        })
    };

    while let Some(handle) = prefetch_rx.recv().await {
        let (sub_frag, sub_buf) = match handle
            .await
            .map_err(|e| StorageError::internal_with_context(e, "load task join"))
            .and_then(|r| r)
        {
            Ok(v) => v,
            Err(e) => {
                result = result.and(Err(e));
                continue;
            }
        };
        if result.is_err() {
            continue;
        }

        let sub_payload_size = sub_frag.size_payload as usize;
        if sub_buf.len() < sub_payload_size {
            result = result.and(Err(StorageError::internal("insufficient buffer")));
            continue;
        }

        let sub_buffer = sub_buf.to_aligned::<FragmentReference>();
        let sub_list = sub_buffer.as_type_slice::<FragmentReference>();
        let sub_content_size = sub_frag.size_content as usize;

        let subresult = if children_are_leaves {
            let sub_base_offset = if sub_list.is_empty() {
                0
            } else {
                sub_list[0].offset_content
            };
            walk_leaf_level(
                sub_list,
                sub_content_size,
                sub_base_offset,
                context,
                leaf_tx,
            )
            .await
        } else {
            Box::pin(walk_fragment_level(
                store.clone(),
                partition,
                context,
                sub_list,
                sub_content_size,
                leaf_tx,
                options,
                remote_session.clone(),
                depth + 1,
            ))
            .await
        };
        result = result.and(subresult);
    }

    result.and(
        launcher
            .await
            .map_err(|e| StorageError::internal_with_context(e, "stream queue join"))
            .and_then(|r| r),
    )
}

/// Unordered fetch pool for file/mmap targets.
async fn fetch_unordered(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    mut leaf_rx: Receiver<LeafReference>,
    data_tx: Sender<(usize, Bytes)>,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<(), StorageError> {
    // Leaves must be decompressed here — their content is written at the
    // uncompressed `offset_content` position in the output buffer, and the
    // leaf contiguity check compares `buffer.len()` against that offset
    // delta. A non-decompressed leaf would produce size mismatches or
    // corrupt output. Only raw-load callers (reading a single fragment)
    // may ask for undecompressed payloads; defragmentation always needs
    // decompressed data.
    let options = options.with_decompress();
    let semaphore = fragment_limiter();
    let mut tasks = JoinSet::new();
    let mut result = Ok(());

    while let Some(leaf) = leaf_rx.recv().await {
        if result.is_err() {
            break;
        }

        let permit_count = fragment_permit_count(leaf.expected_size as usize);
        let permit = semaphore
            .acquire_many(permit_count)
            .await
            .map_err(|e| StorageError::internal_with_context(e, "permit"))?;

        let tx = data_tx.clone();
        let offset = leaf.offset_content as usize;
        let subaddress = Address {
            context: leaf.context,
            hash: leaf.hash,
        };
        let store = store.clone();
        let remote_session = remote_session.clone();

        let expected_size = leaf.expected_size;
        lore_base::lore_spawn!(tasks, async move {
            let load_result =
                load_fragment(store, partition, subaddress, options, remote_session).await;
            drop(permit);
            let (loaded_fragment, buffer) = load_result?;
            // Tier check: the parent list decided this reference was a leaf
            // by peeking at the first child. If a peer mixed an intermediate
            // fragment list into the same level, the "buffer" here is a list
            // of FragmentReferences, not content bytes — writing it at the
            // leaf's offset would silently corrupt the reassembled output.
            if loaded_fragment.flags & FragmentFlags::PayloadFragmented != 0 {
                return Err(StorageError::internal(
                    "expected leaf fragment but peer returned an intermediate fragment list",
                ));
            }
            // Contiguity check: the chunk's actual content size must exactly
            // match what the parent list's offset delta claims. A mismatch
            // means the reassembly would leave a gap or overlap; reject
            // rather than silently corrupt the output.
            if buffer.len() as u64 != expected_size {
                return Err(StorageError::internal(format!(
                    "leaf fragment content size {} does not match expected {expected_size}",
                    buffer.len()
                )));
            }
            tx.send((offset, buffer))
                .await
                .map_err(|_err| StorageError::internal("stream send failed"))
        });

        // Collect any completed tasks
        while let Some(join_result) = tasks.try_join_next() {
            result = result.and(
                join_result
                    .map_err(|e| StorageError::internal_with_context(e, "task failure"))
                    .and_then(|r| r),
            );
        }
    }

    // Drain remaining tasks
    while let Some(join_result) = tasks.join_next().await {
        result = result.and(
            join_result
                .map_err(|e| StorageError::internal_with_context(e, "task failure"))
                .and_then(|r| r),
        );
    }

    result
}

/// Ordered fetch pool for streaming targets.
async fn fetch_ordered_and_stream(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    mut leaf_rx: Receiver<LeafReference>,
    sender: Sender<Bytes>,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<(), StorageError> {
    // See fetch_unordered: defragmentation leaves are always decompressed.
    let options = options.with_decompress();
    let semaphore = fragment_limiter();

    let max_tasks = FRAGMENT_BUDGET_KIB / FRAGMENT_MINIMUM_COST_KIB as usize;
    let (fetch_queue_tx, mut fetch_queue_rx) =
        channel::<JoinHandle<Result<Bytes, StorageError>>>(max_tasks);

    // Launcher: read leaf refs from walker, spawn fetch tasks, push handles
    let launcher: JoinHandle<Result<(), StorageError>> = {
        let store = store.clone();
        let remote_session = remote_session.clone();
        lore_base::lore_spawn!(async move {
            while let Some(leaf) = leaf_rx.recv().await {
                let permit_count = fragment_permit_count(leaf.expected_size as usize);
                let permit = semaphore
                    .acquire_many(permit_count)
                    .await
                    .map_err(|e| StorageError::internal_with_context(e, "permit"))?;

                let subaddress = Address {
                    context: leaf.context,
                    hash: leaf.hash,
                };
                let store = store.clone();
                let remote_session = remote_session.clone();
                let expected_size = leaf.expected_size;

                let handle: JoinHandle<Result<Bytes, StorageError>> = lore_base::lore_spawn!(
                    async move {
                        let load_result =
                            load_fragment(store, partition, subaddress, options, remote_session)
                                .await;
                        drop(permit);
                        let (loaded_fragment, buffer) = load_result?;
                        if loaded_fragment.flags & FragmentFlags::PayloadFragmented != 0 {
                            return Err(StorageError::internal(
                                "expected leaf fragment but peer returned an intermediate fragment list",
                            ));
                        }
                        if buffer.len() as u64 != expected_size {
                            return Err(StorageError::internal(format!(
                                "leaf fragment content size {} does not match expected {expected_size}",
                                buffer.len()
                            )));
                        }
                        Ok(buffer)
                    }
                );

                if fetch_queue_tx.send(handle).await.is_err() {
                    break;
                }
            }
            Ok(())
        })
    };

    // Consumer: await handles in FIFO order, send to caller's channel
    let mut result = Ok(());
    while let Some(handle) = fetch_queue_rx.recv().await {
        match handle
            .await
            .map_err(|e| StorageError::internal_with_context(e, "load task join"))
            .and_then(|r| r)
        {
            Ok(buffer) => {
                if result.is_ok() {
                    result = sender
                        .send(buffer)
                        .await
                        .map_err(|_err| StorageError::internal("stream send failed"));
                }
            }
            Err(e) => {
                result = result.and(Err(e));
            }
        }
    }

    result.and(
        launcher
            .await
            .map_err(|e| StorageError::internal_with_context(e, "stream queue join"))
            .and_then(|r| r),
    )
}

/// Write sink for file/mmap targets.
async fn write_to_sink(
    sink: DefragmentSink,
    data_rx: Receiver<(usize, Bytes)>,
) -> Result<(), StorageError> {
    match sink {
        DefragmentSink::Mmap { ptr, len } => {
            write_to_sink_mmap(MmapPtr { ptr, len }, data_rx).await
        }
        DefragmentSink::File { file } => write_to_sink_file(file, data_rx).await,
        DefragmentSink::Stream { .. } => {
            debug_assert!(false, "write_to_sink called with Stream sink");
            Ok(())
        }
    }
}

/// Send-safe wrapper for a raw mmap pointer.
struct MmapPtr {
    ptr: *mut u8,
    len: usize,
}

unsafe impl Send for MmapPtr {}
unsafe impl Sync for MmapPtr {}

/// Send-safe wrapper for a single bounds-checked destination pointer moved
/// into a blocking copy task. The whole wrapper must be captured (not the
/// bare `*mut u8` field) for the closure to stay `Send`.
struct DstPtr(*mut u8);

unsafe impl Send for DstPtr {}

/// Copy size above which the mmap `memcpy` is offloaded to a blocking thread.
/// A file-backed mmap faults each touched page in synchronously, so a large
/// copy can stall for milliseconds; below this size the `spawn_blocking` round
/// trip costs more than copying inline.
const MMAP_COPY_OFFLOAD_THRESHOLD: usize = 4 * 1024;

/// Drains `(offset, data)` messages from the fetch pool and copies each
/// payload to the corresponding mmap region.
///
/// The copy into a file-backed mmap is effectively an `fwrite`: each touched
/// page faults in synchronously, so on a cold mapping the `memcpy` can stall
/// for milliseconds. The drain loop and bounds check stay on the async worker
/// (cheap), but copies at or above [`MMAP_COPY_OFFLOAD_THRESHOLD`] are spawned
/// onto blocking threads via a `JoinSet`, so page-fault stalls never block a
/// runtime worker and independent copies overlap. Smaller copies run inline as
/// the `spawn_blocking` round trip would cost more than the copy. Completed
/// copies are reaped each iteration; the rest are joined after the channel
/// closes. A copy-task or bounds error breaks the drain loop and still joins
/// the spawned tasks before returning.
///
/// # Invariants relied on
///
/// - **Non-overlapping writes.** Messages are assumed to target disjoint
///   byte ranges. The fragment-list walker's strict-increasing offset check
///   plus the leaf contiguity check (actual leaf `size_content` equals the
///   offset delta) guarantee this for any well-formed fragment tree. This is
///   now load-bearing for *memory safety*: offloaded copies run concurrently,
///   so two copies into overlapping regions would be a data race. A future
///   producer that forwards into this channel without that validation must
///   re-establish disjointness.
/// - **Bounded offset.** The bounds check below is the last line of defence
///   against a compromised offset; do not remove it even if upstream
///   appears to already cap offsets.
async fn write_to_sink_mmap(
    mmap: MmapPtr,
    mut data_rx: Receiver<(usize, Bytes)>,
) -> Result<(), StorageError> {
    let mut tasks: JoinSet<()> = JoinSet::new();
    let mut result = Ok(());

    while let Some((offset, payload)) = data_rx.recv().await {
        let len = payload.len();
        // Runtime bounds check: a compromised fragment list can feed an
        // out-of-range `offset` here. An OOB write would be memory-unsafe.
        let Some(end) = offset.checked_add(len) else {
            result = Err(StorageError::internal(
                "mmap write offset + data length overflows usize",
            ));
            break;
        };
        if end > mmap.len {
            result = Err(StorageError::internal(format!(
                "mmap write out of bounds: offset {offset} + {len} > {}",
                mmap.len
            )));
            break;
        }

        // SAFETY: `offset + len <= mmap.len` was just verified, so the region
        // lies within the mapping, and the non-overlapping invariant above
        // guarantees it does not alias any concurrent copy's destination.
        let dst = DstPtr(unsafe { mmap.ptr.add(offset) });

        if len < MMAP_COPY_OFFLOAD_THRESHOLD {
            unsafe {
                std::ptr::copy_nonoverlapping(payload.as_ptr(), dst.0, len);
            }
        } else {
            lore_base::lore_spawn_blocking!(tasks, move || {
                let dst = dst;
                unsafe {
                    std::ptr::copy_nonoverlapping(payload.as_ptr(), dst.0, len);
                }
            });
        }

        // Reap copies that have already finished without blocking.
        while let Some(join_result) = tasks.try_join_next() {
            result = result.and(
                join_result.map_err(|e| StorageError::internal_with_context(e, "mmap copy task")),
            );
        }
        if result.is_err() {
            break;
        }
    }

    // Join any copies still in flight (also drains them after an early break).
    while let Some(join_result) = tasks.join_next().await {
        result = result
            .and(join_result.map_err(|e| StorageError::internal_with_context(e, "mmap copy task")));
    }

    result
}

async fn write_to_sink_file(
    file: Arc<Mutex<File>>,
    mut data_rx: Receiver<(usize, Bytes)>,
) -> Result<(), StorageError> {
    let mut retry = crate::retry(10, 10_000, 10);
    while let Some((offset, payload)) = data_rx.recv().await {
        loop {
            let mut locked_file = file.lock().await;
            if let Err(err) = locked_file
                .seek(tokio::io::SeekFrom::Start(offset as u64))
                .await
            {
                if !retry.wait().await {
                    return Err(StorageError::internal_with_context(err, "write to file"));
                }
                continue;
            }
            if let Err(err) = locked_file.write_all(payload.as_ref()).await {
                if !retry.wait().await {
                    return Err(StorageError::internal_with_context(err, "write to file"));
                }
                continue;
            }
            break;
        }
    }
    Ok(())
}

/// Unified streaming defragmentation pipeline.
#[allow(clippy::too_many_arguments)]
pub async fn defragment_pipeline(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    fragment: Fragment,
    source_buffer: Bytes,
    sink: DefragmentSink,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<(), StorageError> {
    let (leaf_tx, leaf_rx) = channel::<LeafReference>(PIPELINE_LEAF_CHANNEL_SIZE);

    // Stage 1: Tree walker
    let store_walker = store.clone();
    let session_walker = remote_session.clone();
    let walker = lore_base::lore_spawn!(walk_fragment_tree(
        store_walker,
        partition,
        address,
        fragment,
        source_buffer,
        leaf_tx,
        options,
        session_walker,
    ));

    if let DefragmentSink::Stream { sender } = sink {
        // Ordered fetch -> stream directly to caller's channel
        let store_fetch = store.clone();
        let session_fetch = remote_session.clone();
        let fetcher = lore_base::lore_spawn!(fetch_ordered_and_stream(
            store_fetch,
            partition,
            leaf_rx,
            sender,
            options,
            session_fetch,
        ));

        let (walk_result, fetch_result) = tokio::join!(walker, fetcher);
        walk_result
            .map_err(|e| StorageError::internal_with_context(e, "task failure"))
            .and_then(|r| r)
            .and(
                fetch_result
                    .map_err(|e| StorageError::internal_with_context(e, "task failure"))
                    .and_then(|r| r),
            )
    } else {
        // Unordered fetch -> data channel -> write sink
        let (data_tx, data_rx) = channel::<(usize, Bytes)>(PIPELINE_DATA_CHANNEL_SIZE);

        let store_fetch = store.clone();
        let session_fetch = remote_session.clone();
        let fetcher = lore_base::lore_spawn!(fetch_unordered(
            store_fetch,
            partition,
            leaf_rx,
            data_tx,
            options,
            session_fetch,
        ));

        let writer = lore_base::lore_spawn!(write_to_sink(sink, data_rx));

        let (walk_result, fetch_result, write_result) = tokio::join!(walker, fetcher, writer);
        walk_result
            .map_err(|e| StorageError::internal_with_context(e, "task failure"))
            .and_then(|r| r)
            .and(
                fetch_result
                    .map_err(|e| StorageError::internal_with_context(e, "task failure"))
                    .and_then(|r| r),
            )
            .and(
                write_result
                    .map_err(|e| StorageError::internal_with_context(e, "task failure"))
                    .and_then(|r| r),
            )
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn read_defragment(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    range: Range<usize>,
    fragment: Fragment,
    source_buffer: Bytes,
    mut target: BytesMut,
    options: ReadOptions,
    depth: usize,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<(), StorageError> {
    debug_assert!(
        (fragment.flags & FragmentFlags::PayloadFragmented) == FragmentFlags::PayloadFragmented
    );

    if depth > 16 {
        return Err(StorageError::internal(
            "defragment recursion depth exceeded",
        ));
    }

    let payload_size = fragment.size_payload as usize;
    if source_buffer.len() < payload_size {
        return Err(StorageError::internal("insufficient buffer"));
    }

    let source_buffer = source_buffer.to_aligned::<FragmentReference>();
    let fragment_list = source_buffer.as_type_slice::<FragmentReference>();
    if fragment_list.is_empty() {
        return Err(StorageError::internal(format!(
            "Defragmenting malformed fragment list, size {} is too small",
            source_buffer.len()
        )));
    }

    // Make offset global and cap size
    let mut range = range;
    let offset = range
        .start
        .checked_add(fragment_list[0].offset_content as usize)
        .ok_or_else(|| StorageError::internal("fragment offset overflow"))?;
    if range.len() > target.len() {
        range.end = range.start + target.len();
    }

    // Find the first and last fragment that overlaps the requested range
    let mut fragment_begin = 0;
    let mut fragment_end = fragment_list.len();
    while (fragment_begin < (fragment_list.len() - 1))
        && (offset > fragment_list[fragment_begin + 1].offset_content as usize)
    {
        fragment_begin += 1;
    }
    while ((fragment_end - 1) > fragment_begin)
        && (fragment_list[fragment_end - 1].offset_content as usize > (offset + range.len()))
    {
        fragment_end -= 1;
    }

    let mut subreads = JoinSet::new();

    // Read the content for the range back to front
    let mut fragment_index = fragment_end;
    let mut target_end = range.len();
    let mut result = Ok(());
    while (target_end != 0) && (fragment_index > fragment_begin) {
        fragment_index -= 1;

        let fragment_offset = fragment_list[fragment_index].offset_content as usize;
        let end_offset = offset + target_end;
        if fragment_offset > end_offset {
            break;
        }
        let mut to_read = end_offset - fragment_offset;
        let local_offset = if to_read > target_end {
            to_read = target_end;
            offset.saturating_sub(fragment_offset)
        } else {
            0
        };
        target_end -= to_read;

        let subaddress = Address {
            context: address.context,
            hash: fragment_list[fragment_index].hash,
        };
        let split_point = target.len() - to_read;
        let subtarget = target.split_off(split_point);
        let subrange = local_offset..(local_offset + to_read);
        let store = store.clone();
        let remote_session = remote_session.clone();
        lore_base::lore_spawn!(
            subreads,
            read_defragment_subread(
                store,
                partition,
                subaddress,
                subrange,
                subtarget,
                options,
                depth + 1,
                remote_session,
            )
        );

        while let Some(subresult) = subreads.try_join_next() {
            result = result.and(
                subresult
                    .map_err(|e| StorageError::internal_with_context(e, "task failure"))
                    .and_then(|r| r),
            );
        }
        if result.is_err() {
            break;
        }
    }

    drop(source_buffer);

    while let Some(subresult) = subreads.join_next().await {
        result = result.and(
            subresult
                .map_err(|e| StorageError::internal_with_context(e, "task failure"))
                .and_then(|r| r),
        );
    }

    result
}

#[allow(clippy::too_many_arguments)]
fn read_defragment_subread(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    range: Range<usize>,
    mut target: BytesMut,
    options: ReadOptions,
    depth: usize,
    remote_session: Option<Arc<StorageSession>>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), StorageError>> + Send>> {
    Box::pin(async move {
        let (fragment, buffer) = load_fragment(
            store.clone(),
            partition,
            address,
            options,
            remote_session.clone(),
        )
        .await?;

        if (fragment.flags & FragmentFlags::PayloadFragmented) == FragmentFlags::PayloadFragmented {
            read_defragment(
                store,
                partition,
                address,
                range,
                fragment,
                buffer,
                target,
                options,
                depth,
                remote_session,
            )
            .await
        } else if buffer.len() < range.end {
            Err(StorageError::internal(format!(
                "unexpected size: buffer {} vs range end {}",
                buffer.len(),
                range.end
            )))
        } else {
            if target.len() < range.len() {
                return Err(StorageError::internal(format!(
                    "unexpected size: target {} vs range {}",
                    target.len(),
                    range.len()
                )));
            }
            target[..range.len()].copy_from_slice(&buffer.as_ref()[range]);
            Ok(())
        }
    })
}

pub async fn open_file_write(
    path: impl AsRef<Path>,
    size: usize,
) -> Result<tokio::fs::File, std::io::Error> {
    let file = tokio::fs::File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .await?;

    file.set_len(size as u64).await?;

    Ok(file)
}

pub async fn open_mmap_write(
    path: impl AsRef<Path>,
    size: usize,
) -> Result<(tokio::fs::File, MmapMut), std::io::Error> {
    let file = tokio::fs::File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .await?;

    file.set_len(size as u64).await?;

    let mmap = unsafe { MmapMut::map_mut(&file)? };

    Ok((file, mmap))
}

/// Open a file for reading and return its content as [`Bytes`] along with a flag
/// indicating whether the buffer is a live memory mapping.
///
/// Files at or below [`MMAP_READ_THRESHOLD`] are read into a heap buffer (a snapshot);
/// larger files are memory mapped (live — callers that require a stable view must
/// copy the bytes themselves). The file size is queried from the open handle to
/// avoid a separate metadata syscall on the path. All filesystem work runs on a
/// blocking thread via [`lore_spawn_blocking!`].
pub async fn open_mmap_read(
    path: impl AsRef<Path>,
) -> Result<(Bytes, bool, Option<tokio::sync::OwnedSemaphorePermit>), std::io::Error> {
    let path = path.as_ref().to_path_buf();

    // Reserve before allocating; mmapped files aren't copied, so they take none.
    let read_permit = match tokio::fs::metadata(&path).await {
        Ok(meta) if meta.len() > 0 && meta.len() <= MMAP_READ_THRESHOLD => {
            crate::concurrency::acquire_fragment_memory_permit(meta.len() as usize).await
        }
        _ => None,
    };

    let (bytes, is_mmapped) =
        lore_base::lore_spawn_blocking!(move || -> std::io::Result<(Bytes, bool)> {
            use std::io::Read;

            let mut file = std::fs::File::options()
                .create(false)
                .truncate(false)
                .read(true)
                .write(false)
                .open(&path)?;

            let size = file.metadata()?.len();

            if size <= MMAP_READ_THRESHOLD {
                let size = size as usize;
                let mut buf = BytesMut::with_capacity(size);
                // Safety: `with_capacity(size)` guarantees that the allocation holds
                // at least `size` bytes. The following `read_exact` initialises every
                // byte up to `size`, so no uninitialised memory is ever observed.
                unsafe { buf.set_len(size) };
                file.read_exact(&mut buf)?;
                Ok((buf.freeze(), false))
            } else {
                let mmap = unsafe { Mmap::map(&file)? };
                Ok((Bytes::from_owner(mmap), true))
            }
        })
        .await
        .map_err(std::io::Error::other)??;

    Ok((bytes, is_mmapped, read_permit))
}

/// Files at or below this size are read into a heap buffer rather than memory mapped.
pub const MMAP_READ_THRESHOLD: u64 = 4 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    mod walk_leaf_level {
        use super::*;

        fn refs(offsets: &[u64]) -> Vec<FragmentReference> {
            offsets
                .iter()
                .map(|&o| FragmentReference {
                    hash: Hash::default(),
                    offset_content: o,
                })
                .collect()
        }

        /// Drive `walk_leaf_level` to completion and collect emitted leaves.
        async fn run(
            fragment_list: &[FragmentReference],
            total_content_size: usize,
            base_offset: u64,
        ) -> Result<Vec<LeafReference>, StorageError> {
            let (tx, mut rx) = channel::<LeafReference>(32);
            let context = Context::default();
            let walk_result =
                walk_leaf_level(fragment_list, total_content_size, base_offset, context, &tx).await;
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
            let span = crate::FRAGMENT_SIZE_THRESHOLD + 1;
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
    }

    mod write_to_sink_mmap {
        //! Direct unit tests for the mmap write sink's runtime bounds check.
        //!
        //! In the full pipeline the leaf contiguity check in `fetch_unordered`
        //! filters out the inputs that would make this bound fire, so these
        //! tests exercise the sink in isolation — the bound is defense-in-depth
        //! against any future producer that bypasses earlier validation.
        use super::*;

        #[tokio::test]
        async fn accepts_in_bounds_write() {
            let mut buf = vec![0u8; 100];
            let ptr = buf.as_mut_ptr();
            let mmap = MmapPtr {
                ptr,
                len: buf.len(),
            };
            let (tx, rx) = channel::<(usize, Bytes)>(4);
            tx.send((10usize, Bytes::from(vec![0xAB; 20])))
                .await
                .expect("send");
            drop(tx);
            super::super::write_to_sink_mmap(mmap, rx)
                .await
                .expect("in-bounds write");
            assert_eq!(&buf[10..30], &[0xAB; 20]);
        }

        #[tokio::test]
        async fn rejects_offset_plus_length_past_end() {
            let mut buf = vec![0u8; 100];
            let ptr = buf.as_mut_ptr();
            let mmap = MmapPtr {
                ptr,
                len: buf.len(),
            };
            let (tx, rx) = channel::<(usize, Bytes)>(4);
            tx.send((95usize, Bytes::from(vec![0u8; 10])))
                .await
                .expect("send"); // 95 + 10 = 105 > 100
            drop(tx);
            let err = super::super::write_to_sink_mmap(mmap, rx)
                .await
                .expect_err("OOB should be rejected");
            assert!(
                err.to_string().contains("out of bounds"),
                "unexpected error: {err}"
            );
        }

        #[tokio::test]
        async fn rejects_offset_at_exact_end_with_nonzero_length() {
            let mut buf = vec![0u8; 100];
            let ptr = buf.as_mut_ptr();
            let mmap = MmapPtr {
                ptr,
                len: buf.len(),
            };
            let (tx, rx) = channel::<(usize, Bytes)>(4);
            tx.send((100usize, Bytes::from(vec![0u8; 1])))
                .await
                .expect("send");
            drop(tx);
            super::super::write_to_sink_mmap(mmap, rx)
                .await
                .expect_err("offset==len with data should be rejected");
        }

        #[tokio::test]
        async fn rejects_arithmetic_overflow() {
            let mut buf = vec![0u8; 100];
            let ptr = buf.as_mut_ptr();
            let mmap = MmapPtr {
                ptr,
                len: buf.len(),
            };
            let (tx, rx) = channel::<(usize, Bytes)>(4);
            tx.send((usize::MAX - 5, Bytes::from(vec![0u8; 10])))
                .await
                .expect("send");
            drop(tx);
            let err = super::super::write_to_sink_mmap(mmap, rx)
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

        use zerocopy::IntoBytes;

        use super::*;
        use crate::hash;
        use crate::local::immutable_store::ImmutableStoreSettings;
        use crate::local::immutable_store::LocalImmutableStore;
        use crate::options::ReadOptions;
        use crate::test_util::TempDir;

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

        async fn put_root_list(
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
            let root_address = put_root_list(&store, partition, context, &refs, 300).await;

            let out_path = dir.join("contiguity-fail.bin");
            let err = crate::read::read_into_file(
                store.clone(),
                partition,
                root_address,
                &out_path,
                ".tmp",
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
            let root_address = put_root_list(&store, partition, context, &refs, 250).await;

            let out_path = dir.join("well-formed.bin");
            crate::read::read_into_file(
                store.clone(),
                partition,
                root_address,
                &out_path,
                ".tmp",
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
            let root_address = put_root_list(&store, partition, context, &refs, 200).await;

            let out_path = dir.join("mixed-tier.bin");
            let err = crate::read::read_into_file(
                store.clone(),
                partition,
                root_address,
                &out_path,
                ".tmp",
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

            // Build a chain of intermediate lists wrapping the leaf.
            // Each intermediate holds two references to the same hash so
            // the walker sees a valid list structure at every level.
            //
            // walk_intermediate_level peeks two levels ahead to distinguish
            // leaf from intermediate children, so N wrapping levels reach
            // max walk_fragment_level depth N-2. We need depth > 8 to fire
            // MAX_FRAGMENT_TREE_DEPTH, so at least 11 wraps.
            let mut current_hash = leaf_addr.hash;
            for _ in 0..12 {
                let refs = [
                    FragmentReference {
                        hash: current_hash,
                        offset_content: 0,
                    },
                    FragmentReference {
                        hash: current_hash,
                        offset_content: 32,
                    },
                ];
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
            let err = crate::read::read_into_file(
                store.clone(),
                partition,
                root_address,
                &out_path,
                ".tmp",
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
    }
}
