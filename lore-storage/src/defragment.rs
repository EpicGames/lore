// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ops::Range;
#[cfg(target_family = "unix")]
use std::os::unix::fs::FileExt;
#[cfg(target_family = "windows")]
use std::os::windows::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use bytes::BytesMut;
use lore_transport::StorageSession;
use tokio::sync::Semaphore;
use tokio::sync::SemaphorePermit;
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
    /// Write at offset to a file (unordered, concurrent positional writes).
    /// `size` is the expected content length, used to reject out-of-range offsets.
    File {
        file: Arc<std::fs::File>,
        size: usize,
    },
    /// Stream buffers in content order to a caller-provided channel.
    Stream { sender: Sender<Bytes> },
}

/// A fetched payload on its way to the write sink: target offset, bytes, and the
/// fragment memory permit covering those bytes. The permit rides along so it is
/// released when the write completes rather than when the fetch did.
type DataMessage = (usize, Bytes, tokio::sync::SemaphorePermit<'static>);
type DataSender = Sender<DataMessage>;
type DataReceiver = Receiver<DataMessage>;

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

/// Walks one level of the tree, dispatching to the leaf or intermediate walker by peeking at
/// the first entry.
///
/// An empty list is invalid at every level, whatever its parent claims and including a parent
/// claiming zero: zero-length content is addressed by the zero hash, never by a fragment whose
/// list expands to nothing. Accepting one would report a level as walked when nothing had been
/// written, and since the target file is sized before the walk starts, that is a zero-filled
/// range indistinguishable from content.
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
        return Err(StorageError::internal(format!(
            "fragment list is empty, claiming {total_content_size} bytes of content"
        )));
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
            total_content_size,
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
/// or file writes.
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
        if frag_ref.hash.is_zero() {
            return Err(StorageError::internal(format!(
                "fragment list entry {i} at content offset {} has a zero hash",
                frag_ref.offset_content
            )));
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

/// Verifies that one sublist covers exactly the range its parent entry claims, returning
/// the content offset the next sibling has to start at.
///
/// Sublist offsets are absolute in the whole content, so in a well-formed tree a parent
/// entry's `offset_content` equals its sublist's own first offset, and consecutive siblings
/// tile the parent's range end to end. A gap between siblings is not a read error on its own:
/// the output file is `set_len` to its full size before the walk starts, so a range no leaf
/// ever writes reads back as zeros and the file is renamed into place as complete. An overlap
/// is the same fault from the other side, which is why this compares offsets rather than only
/// summing sizes.
///
/// A sublist that is empty or expands to zero bytes is invalid outright: zero-length content
/// is addressed by the zero hash, so no valid tree contains a list standing in for nothing.
/// The zero hash itself is just as invalid as an entry, and is checked here rather than in a
/// pass of its own — `load_fragment` resolves it to a default `Fragment` instead of an error,
/// and that carries no `PayloadFragmented` flag and zero `size_content`, so an unchecked one
/// turns a level of intermediate references into leaves.
fn sublist_coverage(
    parent: &FragmentReference,
    sub_list: &[FragmentReference],
    sub_content_size: usize,
    expected_offset: u64,
    level_end: u64,
) -> Result<u64, StorageError> {
    if parent.offset_content != expected_offset {
        return Err(StorageError::internal(format!(
            "fragment sublist starts at {} but the previous sibling ends at {expected_offset}",
            parent.offset_content
        )));
    }
    if parent.hash.is_zero() {
        return Err(StorageError::internal(format!(
            "fragment list entry at content offset {expected_offset} has a zero hash"
        )));
    }
    if sub_list.is_empty() {
        return Err(StorageError::internal(format!(
            "fragment sublist at offset {expected_offset} is empty"
        )));
    }
    if sub_content_size == 0 {
        return Err(StorageError::internal(format!(
            "fragment sublist at offset {expected_offset} expands to zero bytes"
        )));
    }
    if sub_list[0].offset_content != parent.offset_content {
        return Err(StorageError::internal(format!(
            "fragment sublist starts at {} but its parent entry places it at {}",
            sub_list[0].offset_content, parent.offset_content
        )));
    }
    let end = parent
        .offset_content
        .checked_add(sub_content_size as u64)
        .ok_or_else(|| {
            StorageError::internal("fragment sublist offset + content size overflows u64")
        })?;
    if end > level_end {
        return Err(StorageError::internal(format!(
            "fragment sublist ends at {end}, past its parent's content end {level_end}"
        )));
    }
    Ok(end)
}

/// The sublists of one level have to reach the end of their parent's content range, not
/// merely stay inside it. Stopping short leaves the tail of the file uncovered.
fn level_fully_covered(covered: u64, level_end: u64) -> Result<(), StorageError> {
    if covered != level_end {
        return Err(StorageError::internal(format!(
            "fragment sublists cover up to {covered} but their parent's content ends at {level_end}"
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn walk_intermediate_level(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
    fragment_list: &[FragmentReference],
    total_content_size: usize,
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

    let base_offset = fragment_list[0].offset_content;
    let level_end = base_offset
        .checked_add(total_content_size as u64)
        .ok_or_else(|| {
            StorageError::internal("fragment list base_offset + total_content_size overflows u64")
        })?;
    let mut covered = sublist_coverage(
        &fragment_list[0],
        first_list,
        first_content_size,
        base_offset,
        level_end,
    )?;

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
        return result.and(level_fully_covered(covered, level_end));
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

    // Advances on every iteration, bail-outs included, or a later sublist checks against
    // the wrong parent.
    let mut index = 0usize;
    while let Some(handle) = prefetch_rx.recv().await {
        index += 1;
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

        let Some(parent) = fragment_list.get(index) else {
            result = result.and(Err(StorageError::internal(
                "more prefetched sublists than fragment list entries",
            )));
            continue;
        };
        match sublist_coverage(parent, sub_list, sub_content_size, covered, level_end) {
            Ok(end) => covered = end,
            Err(err) => {
                result = result.and(Err(err));
                continue;
            }
        }

        let subresult = if children_are_leaves {
            walk_leaf_level(
                sub_list,
                sub_content_size,
                sub_list[0].offset_content,
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

    let launcher_result = launcher
        .await
        .map_err(|e| StorageError::internal_with_context(e, "stream queue join"))
        .and_then(|r| r);

    result
        .and(level_fully_covered(covered, level_end))
        .and(launcher_result)
}

/// Unordered fetch pool for file targets.
async fn fetch_unordered(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    mut leaf_rx: Receiver<LeafReference>,
    data_tx: DataSender,
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
            let (loaded_fragment, buffer) =
                load_fragment(store, partition, subaddress, options, remote_session).await?;
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
            tx.send((offset, buffer, permit))
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
///
/// Every payload carries its memory permit from the load until it is handed to the
/// caller's channel, so the fragment budget bounds what the pipeline holds even when the
/// caller consumes slowly. The fetch is one task per leaf, awaited in list order, which is
/// what makes the output a stream rather than positional writes.
async fn fetch_ordered_and_stream(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    leaf_rx: Receiver<LeafReference>,
    sender: Sender<Bytes>,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<(), StorageError> {
    fetch_ordered_and_stream_from(
        fragment_limiter(),
        store,
        partition,
        leaf_rx,
        sender,
        options,
        remote_session,
    )
    .await
}

async fn fetch_ordered_and_stream_from(
    semaphore: &'static Semaphore,
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    mut leaf_rx: Receiver<LeafReference>,
    sender: Sender<Bytes>,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<(), StorageError> {
    // See fetch_unordered: defragmentation leaves are always decompressed.
    let options = options.with_decompress();

    // Sized so it never binds before the budget does; every payload in it holds a permit.
    let max_tasks = FRAGMENT_BUDGET_KIB / FRAGMENT_MINIMUM_COST_KIB as usize;
    type FetchResult = Result<(Bytes, SemaphorePermit<'static>), StorageError>;
    let (fetch_queue_tx, mut fetch_queue_rx) = channel::<JoinHandle<FetchResult>>(max_tasks);

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

                let handle: JoinHandle<FetchResult> = lore_base::lore_spawn!(async move {
                    let (loaded_fragment, buffer) =
                        load_fragment(store, partition, subaddress, options, remote_session)
                            .await?;
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
                    Ok((buffer, permit))
                });

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
            Ok((buffer, permit)) => {
                if result.is_ok() {
                    result = sender
                        .send(buffer)
                        .await
                        .map_err(|_err| StorageError::internal("stream send failed"));
                }
                // Released here, not at load, so the budget bounds the pipeline.
                drop(permit);
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

/// Write sink for file targets.
async fn write_to_sink(sink: DefragmentSink, data_rx: DataReceiver) -> Result<(), StorageError> {
    match sink {
        DefragmentSink::File { file, size } => write_to_sink_file(file, size, data_rx).await,
        DefragmentSink::Stream { .. } => {
            debug_assert!(false, "write_to_sink called with Stream sink");
            Ok(())
        }
    }
}

/// Drains `(offset, data, permit)` messages from the fetch pool and writes each
/// payload at its offset.
///
/// Positional writes carry their own offset, so concurrent writes to disjoint ranges
/// need no lock — the previous seek-plus-write sink had to serialize behind a mutex
/// because the pair is not atomic. Each write is one blocking task, so the syscall
/// never stalls a runtime worker and independent writes overlap. Completed writes are
/// reaped each iteration; the rest are joined after the channel closes, including
/// after an early error break.
///
/// Each message carries the fragment memory permit for its payload, released only when
/// the write task ends. That keeps the payload accounted for its whole life rather than
/// just while it was being fetched.
///
/// Overlapping ranges would corrupt the output but are not a soundness problem, unlike
/// the memory-mapped sink this replaced: the fragment-list walker's strict-increasing
/// offset check and the leaf contiguity check still guarantee disjointness for any
/// well-formed fragment tree.
///
/// The bounds check against `size` is the last line of defence against a compromised
/// fragment list. It is no longer a memory-safety boundary as it was for the mapping,
/// but an unchecked offset would still punch a sparse hole far past the intended end of
/// file rather than failing; do not remove it even if upstream appears to cap offsets.
///
/// The byte count against `size` is the other half of that: the file is `set_len` to its
/// full size before the first write, so a range no payload covers is not a short file but
/// a zero-filled hole, indistinguishable from content. Every payload for the whole file
/// passes through here, which makes this the one place that can see the total. The walker's
/// tiling checks mean it should never fire, which is the point of having it.
async fn write_to_sink_file(
    file: Arc<std::fs::File>,
    size: usize,
    mut data_rx: DataReceiver,
) -> Result<(), StorageError> {
    let mut tasks: JoinSet<Result<(), StorageError>> = JoinSet::new();
    let mut result = Ok(());
    let mut written = 0usize;

    while let Some((offset, payload, permit)) = data_rx.recv().await {
        let Some(end) = offset.checked_add(payload.len()) else {
            result = Err(StorageError::internal(
                "file write offset + data length overflows usize",
            ));
            break;
        };
        if end > size {
            result = Err(StorageError::internal(format!(
                "file write out of bounds: offset {offset} + {} > {size}",
                payload.len()
            )));
            break;
        }

        written += payload.len();

        let file = file.clone();
        lore_base::lore_spawn_blocking!(tasks, move || {
            let _permit = permit;
            write_all_at(&file, payload.as_ref(), offset as u64)
                .map_err(|e| StorageError::internal_with_context(e, "write to file"))
        });

        while let Some(join_result) = tasks.try_join_next() {
            result = result.and(
                join_result
                    .map_err(|e| StorageError::internal_with_context(e, "write task"))
                    .and_then(|r| r),
            );
        }
        if result.is_err() {
            break;
        }
    }

    while let Some(join_result) = tasks.join_next().await {
        result = result.and(
            join_result
                .map_err(|e| StorageError::internal_with_context(e, "write task"))
                .and_then(|r| r),
        );
    }

    if result.is_ok() && written != size {
        result = Err(StorageError::internal(format!(
            "defragmented content covers {written} of {size} bytes"
        )));
    }

    result
}

/// Read the whole file into `buffer`, returning the byte count. Test-only readback for
/// the write sink.
#[cfg(test)]
fn read_all_at_for_test(file: &std::fs::File, buffer: &mut [u8]) -> usize {
    let mut read = 0;
    while read < buffer.len() {
        #[cfg(target_family = "unix")]
        let count = file
            .read_at(&mut buffer[read..], read as u64)
            .expect("read");
        #[cfg(target_family = "windows")]
        let count = file
            .seek_read(&mut buffer[read..], read as u64)
            .expect("read");
        if count == 0 {
            break;
        }
        read += count;
    }
    read
}

/// Write every byte at `offset`, retrying interrupted calls. On Windows `seek_write`
/// also moves the file cursor, which is harmless because no caller of this sink reads
/// the cursor; note the handle is not opened overlapped, so concurrent writes to it are
/// serialized by the kernel there even though each carries its own offset.
fn write_all_at(file: &std::fs::File, buffer: &[u8], offset: u64) -> std::io::Result<()> {
    #[cfg(target_family = "unix")]
    {
        file.write_all_at(buffer, offset)
    }
    #[cfg(target_family = "windows")]
    {
        let mut written = 0;
        while written < buffer.len() {
            match file.seek_write(&buffer[written..], offset + written as u64) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        format!(
                            "wrote {written} of {} bytes at offset {offset}",
                            buffer.len()
                        ),
                    ));
                }
                Ok(count) => written += count,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
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
        let (data_tx, data_rx) = channel::<DataMessage>(PIPELINE_DATA_CHANNEL_SIZE);

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

/// Open (creating if needed) and size a file for positional writes. The handle is
/// shared: positional writes carry their own offset, so concurrent writers to disjoint
/// ranges need no exclusion.
/// Opens a file for positional writing and sizes it to the whole content up front.
///
/// On Windows the handle is deliberately **not** overlapped. `seek_write` issues a synchronous
/// `WriteFile` with the offset in an `OVERLAPPED` and no event, which is only defined for a
/// synchronous handle; against an overlapped one it can report `ERROR_IO_PENDING` while the
/// kernel still holds the buffer. So `FILE_FLAG_OVERLAPPED` is not a way to parallelise these
/// writes, even though the handle being synchronous is what serializes them.
pub async fn open_file_write(
    path: impl AsRef<Path>,
    size: usize,
) -> Result<Arc<std::fs::File>, std::io::Error> {
    let path = path.as_ref().to_path_buf();
    lore_base::lore_spawn_blocking!(move || -> std::io::Result<Arc<std::fs::File>> {
        let file = std::fs::File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        file.set_len(size as u64)?;
        Ok(Arc::new(file))
    })
    .await
    .map_err(std::io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;

    mod walk_leaf_level {
        use super::*;

        /// Hashes are distinct and non-zero: a zero hash is not a legal list entry, so a
        /// list built from `Hash::default()` would be rejected before the offset arithmetic
        /// these tests are about.
        fn refs(offsets: &[u64]) -> Vec<FragmentReference> {
            offsets
                .iter()
                .map(|&o| FragmentReference {
                    hash: crate::hash::hash_slice(&o.to_le_bytes()),
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

    mod write_to_sink_file {
        //! Direct unit tests for the file write sink's runtime bounds check.
        //!
        //! In the full pipeline the leaf contiguity check in `fetch_unordered`
        //! filters out the inputs that would make this bound fire, so these
        //! tests exercise the sink in isolation — the bound is defense-in-depth
        //! against any future producer that bypasses earlier validation. Unlike
        //! the memory-mapped sink this replaced, an unchecked offset here is not
        //! unsound, but it would still write far past the intended end of file.
        use super::*;
        use crate::test_util::TempDir;

        const SIZE: usize = 100;

        /// A sized target file plus a channel wired to the sink.
        fn target(dir: &TempDir, name: &str) -> Arc<std::fs::File> {
            let path = dir.path().join(name);
            let file = std::fs::File::options()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)
                .expect("create target file");
            file.set_len(SIZE as u64).expect("size target file");
            Arc::new(file)
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
            let file = target(&dir, "in-bounds");
            let (tx, rx) = channel::<DataMessage>(4);
            send_one(&tx, 0, Bytes::from(vec![0xCD; 10])).await;
            send_one(&tx, 10, Bytes::from(vec![0xAB; 20])).await;
            send_one(&tx, 30, Bytes::from(vec![0xEF; SIZE - 30])).await;
            drop(tx);

            super::super::write_to_sink_file(file.clone(), SIZE, rx)
                .await
                .expect("in-bounds write");

            let mut buf = vec![0u8; SIZE];
            let read = super::super::read_all_at_for_test(&file, &mut buf);
            assert_eq!(read, SIZE);
            assert_eq!(&buf[10..30], &[0xAB; 20]);
        }

        /// Payloads that stay in bounds but do not add up to the file: the target is
        /// `set_len` up front, so the uncovered range is zeros in a file that would
        /// otherwise be renamed into place as complete.
        #[tokio::test]
        async fn rejects_payloads_that_do_not_cover_the_file() {
            let dir = TempDir::new("lore-storage-sink-test-");
            let file = target(&dir, "hole");
            let (tx, rx) = channel::<DataMessage>(4);
            send_one(&tx, 0, Bytes::from(vec![0xAB; 20])).await;
            send_one(&tx, 40, Bytes::from(vec![0xAB; SIZE - 40])).await;
            drop(tx);

            let err = super::super::write_to_sink_file(file, SIZE, rx)
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
            let file = target(&dir, "past-end");
            let (tx, rx) = channel::<DataMessage>(4);
            send_one(&tx, 95, Bytes::from(vec![0u8; 10])).await; // 95 + 10 > 100
            drop(tx);

            let err = super::super::write_to_sink_file(file, SIZE, rx)
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
            let file = target(&dir, "exact-end");
            let (tx, rx) = channel::<DataMessage>(4);
            send_one(&tx, SIZE, Bytes::from(vec![0u8; 1])).await;
            drop(tx);

            super::super::write_to_sink_file(file, SIZE, rx)
                .await
                .expect_err("offset==size with data should be rejected");
        }

        #[tokio::test]
        async fn rejects_arithmetic_overflow() {
            let dir = TempDir::new("lore-storage-sink-test-");
            let file = target(&dir, "overflow");
            let (tx, rx) = channel::<DataMessage>(4);
            send_one(&tx, usize::MAX - 5, Bytes::from(vec![0u8; 10])).await;
            drop(tx);

            let err = super::super::write_to_sink_file(file, SIZE, rx)
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
            let root_address = put_list(&store, partition, context, &refs, 250).await;

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
            let root_address = put_list(&store, partition, context, &refs, 200).await;

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
            crate::read::read_into_file(
                store.clone(),
                partition,
                root,
                &out_path,
                ".tmp",
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
            let err = crate::read::read_into_file(
                store.clone(),
                partition,
                root,
                &out_path,
                ".tmp",
                ReadOptions::default().no_verify(),
                None,
            )
            .await
            .expect_err("a gap between siblings should be rejected");

            assert!(
                err.to_string().contains("previous sibling ends at 100"),
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
            let err = crate::read::read_into_file(
                store.clone(),
                partition,
                root,
                &out_path,
                ".tmp",
                ReadOptions::default().no_verify(),
                None,
            )
            .await
            .expect_err("a short tail should be rejected");

            assert!(
                err.to_string().contains("cover up to 200"),
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
            let err = crate::read::read_into_file(
                store.clone(),
                partition,
                root,
                &out_path,
                ".tmp",
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
            let sub_zero = put_list(&store, partition, context, &refs_at(&[(leaf_b, 0)]), 0).await;
            let sub_a = put_list(&store, partition, context, &refs_at(&[(leaf_a, 0)]), 100).await;
            let root = put_list(
                &store,
                partition,
                context,
                &refs_at(&[(sub_zero, 0), (sub_a, 0)]),
                100,
            )
            .await;

            let out_path = dir.join("zero-expansion.bin");
            let err = crate::read::read_into_file(
                store.clone(),
                partition,
                root,
                &out_path,
                ".tmp",
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
            let err = crate::read::read_into_file(
                store.clone(),
                partition,
                root,
                &out_path,
                ".tmp",
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
            let err = crate::read::read_into_file(
                store.clone(),
                partition,
                root,
                &out_path,
                ".tmp",
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
            let sub_zero =
                put_list(&store, partition, context, &refs_at(&[(zero, 100)]), 100).await;
            let root = put_list(
                &store,
                partition,
                context,
                &refs_at(&[(sub_a, 0), (sub_zero, 100)]),
                200,
            )
            .await;

            let out_path = dir.join("zero-hash-sublist.bin");
            let err = crate::read::read_into_file(
                store.clone(),
                partition,
                root,
                &out_path,
                ".tmp",
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
            let err = crate::read::read_into_file(
                store.clone(),
                partition,
                root,
                &out_path,
                ".tmp",
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
            let err = crate::read::read_into_file(
                store.clone(),
                partition,
                root,
                &out_path,
                ".tmp",
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
                        offset_content: (index * PAYLOAD) as u64,
                        expected_size: PAYLOAD as u64,
                        context,
                    })
                    .await
                    .expect("queue leaf");
            }
            drop(leaf_tx);

            // One slot, so only the first payload leaves the pipeline's accounting.
            let (data_tx, mut data_rx) = channel::<Bytes>(1);
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
                let payload = data_rx.recv().await.expect("payload");
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
    }
}
