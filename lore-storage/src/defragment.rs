// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use bytes::BytesMut;
use lore_io::IoDriver;
use lore_io::IoFile;
use lore_io::OpenOptions;
use lore_transport::StorageSession;
use tokio::sync::Semaphore;
use tokio::sync::SemaphorePermit;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::channel;
use tokio::task::JoinError;
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
    File { file: IoFile, size: usize },
    /// Stream buffers in content order to a caller-provided channel.
    ///
    /// The item is a `Result` so a failure partway through the tree reaches the consumer as the
    /// final item rather than only the log. Without it the channel simply closes early and a
    /// truncated read is indistinguishable from a complete one.
    Stream {
        sender: Sender<Result<Bytes, StorageError>>,
    },
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
    /// Where this leaf's delivered bytes belong in the output, counted from the start of the
    /// range that was asked for. Equal to the leaf's content offset for a whole-content read,
    /// which is the only thing the file sink ever wrote before ranges existed.
    target_offset: u64,
    /// The leaf's whole content size, as its parent list claims it. The fetch checks the
    /// loaded payload against this rather than against `clip`: a payload is verified against
    /// the hash that names it, so the whole leaf is what gets loaded and checked whatever
    /// part of it the caller wants.
    expected_size: u64,
    /// The part of this leaf the read asked for, relative to the leaf's own start. Whole
    /// leaves carry `0..expected_size`; only the first and last leaf of a ranged read carry
    /// anything narrower. Applied after the payload is verified, so it narrows what is
    /// delivered rather than what is read.
    clip: Range<u64>,
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
///
/// `range` is the content the caller asked for, counted from the start of the content and
/// already clamped to it. Offsets inside a fragment tree are absolute within the content,
/// so the range is rebased onto the root list's own first offset once here and every level
/// below compares against it directly.
#[allow(clippy::too_many_arguments)]
async fn walk_fragment_tree(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    fragment: Fragment,
    source_buffer: Bytes,
    range: Range<u64>,
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

    if fragment_list.is_empty() {
        return Err(StorageError::internal(format!(
            "fragment list is empty, claiming {total_content_size} bytes of content"
        )));
    }

    let base_offset = fragment_list[0].offset_content;
    let window = base_offset
        .checked_add(range.start)
        .zip(base_offset.checked_add(range.end))
        .map(|(start, end)| start..end)
        .ok_or_else(|| StorageError::internal("content range offset overflow"))?;

    walk_fragment_level(
        store,
        partition,
        address.context,
        fragment_list,
        total_content_size,
        &window,
        &leaf_tx,
        options,
        remote_session,
        0,
    )
    .await
}

/// The absolute content window one entry of a fragment list stands for: from its own offset
/// to where the next entry starts, and for the last entry to the end of the level.
///
/// Derived from the list alone rather than from the fragment the entry names, which is what
/// lets a ranged walk decide an entry is not wanted before paying to load it.
fn entry_window(
    fragment_list: &[FragmentReference],
    index: usize,
    level_end: u64,
) -> Result<Range<u64>, StorageError> {
    let start = fragment_list[index].offset_content;
    let end = if index + 1 < fragment_list.len() {
        fragment_list[index + 1].offset_content
    } else {
        level_end
    };
    let size = end.checked_sub(start).ok_or_else(|| {
        StorageError::internal(
            "fragment list offset_content is not strictly increasing inside content window",
        )
    })?;
    if size == 0 {
        return Err(StorageError::internal("fragment list chunk has zero size"));
    }
    Ok(start..end)
}

/// The part of `entry` that `window` asks for, relative to the entry's own start, or `None`
/// when the two do not overlap.
fn clip_to_window(entry: &Range<u64>, window: &Range<u64>) -> Option<Range<u64>> {
    let start = entry.start.max(window.start);
    let end = entry.end.min(window.end);
    (start < end).then(|| (start - entry.start)..(end - entry.start))
}

/// The entries of a level whose content the read asked for, or `None` when the level holds
/// none of it.
///
/// One contiguous index range, because entries are strictly increasing and tile the level
/// while the window is itself contiguous — so the entries a window reaches cannot have a gap.
/// That is what lets the caller size its work from the ends alone.
///
/// Walks the whole list rather than stopping at the last hit: an entry's arithmetic is checked
/// whether or not its content is wanted, so reading one part of a malformed list cannot
/// succeed where reading another part fails.
fn wanted_entries(
    fragment_list: &[FragmentReference],
    level_end: u64,
    window: &Range<u64>,
) -> Result<Option<Range<usize>>, StorageError> {
    let mut wanted: Option<Range<usize>> = None;
    for index in 0..fragment_list.len() {
        let entry = entry_window(fragment_list, index, level_end)?;
        if clip_to_window(&entry, window).is_some() {
            wanted = Some(match wanted {
                Some(range) => range.start..index + 1,
                None => index..index + 1,
            });
        }
    }
    Ok(wanted)
}

/// Stops a launcher and waits for it, discarding whatever it has already loaded.
///
/// Closing the queue is what stops a launcher, and dropping the receiver is what closes it.
/// The drop must come first: a launcher parked on a full queue never reaches the push that
/// would tell it to stop. Nothing is cancelled to make that happen, and nothing may be — a
/// fetch part way through writing a request would leave the stream it is writing to in a state
/// its peer cannot make sense of. Dropping a `JoinHandle` detaches its task rather than
/// aborting it, so the loads already queued or in flight each finish the request they are in
/// the middle of and release their permit as they go. Their payloads are discarded, which is
/// the point.
async fn join_launcher<T>(
    queue_rx: Receiver<T>,
    launcher: JoinHandle<Result<(), StorageError>>,
) -> Result<(), StorageError> {
    drop(queue_rx);
    launcher
        .await
        .map_err(|e| StorageError::internal_with_context(e, "stream queue join"))
        .and_then(|r| r)
}

/// Whether the pipeline the walk feeds has gone.
///
/// Checked before each load, because the walk descends by fetching list nodes and would
/// otherwise go on fetching them for a queue nobody reads. A walk that stops for this reason
/// reports success: there is no caller left for a failure to reach, and the leaves it has not
/// sent are ones nobody asked to be sent.
fn walk_abandoned(leaf_tx: &Sender<LeafReference>) -> bool {
    leaf_tx.is_closed()
}

/// Walks one level of the tree, dispatching to the leaf or intermediate walker by peeking at
/// the first entry the read reaches.
///
/// The peek lands on the first *wanted* entry rather than on entry zero. Every entry at a
/// level is the same tier, so any of them answers the question, and choosing a wanted one
/// keeps a read of the tail of the content from loading the head of every level on the way
/// down.
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
    window: &Range<u64>,
    leaf_tx: &Sender<LeafReference>,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
    depth: usize,
) -> Result<(), StorageError> {
    if walk_abandoned(leaf_tx) {
        return Ok(());
    }

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
    let level_end = base_offset
        .checked_add(total_content_size as u64)
        .ok_or_else(|| {
            StorageError::internal("fragment list base_offset + total_content_size overflows u64")
        })?;

    // Validates the whole list on the way, so an unwanted level is still a well-formed one.
    let Some(wanted) = wanted_entries(fragment_list, level_end, window)? else {
        return Ok(());
    };
    let first_index = wanted.start;

    let first_address = Address {
        context,
        hash: fragment_list[first_index].hash,
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
            level_end,
            window,
            wanted,
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
            window,
            context,
            leaf_tx,
        )
        .await
    }
}

/// Yields the entries of a leaf-level fragment list that `window` reaches, as `LeafReference`.
///
/// Uses checked arithmetic on `offset_content` so a peer-supplied list with
/// non-increasing offsets, offsets outside the content window, or a total
/// span that overflows u64 fails with a clear error rather than producing a
/// wrapped `expected_size` that would blow up downstream permit accounting
/// or file writes.
///
/// Every entry is checked, including the ones outside the window; only the sending is
/// narrowed. The checks read a list already in hand and cost no I/O, so there is nothing to
/// save by skipping them and a malformed list would otherwise be accepted or rejected
/// depending on which part of the content a caller happened to ask for.
async fn walk_leaf_level(
    fragment_list: &[FragmentReference],
    total_content_size: usize,
    base_offset: u64,
    window: &Range<u64>,
    context: Context,
    leaf_tx: &Sender<LeafReference>,
) -> Result<(), StorageError> {
    let content_end = base_offset
        .checked_add(total_content_size as u64)
        .ok_or_else(|| {
            StorageError::internal("fragment list base_offset + total_content_size overflows u64")
        })?;

    for (i, frag_ref) in fragment_list.iter().enumerate() {
        let entry = entry_window(fragment_list, i, content_end)?;
        let expected_content_size = entry.end - entry.start;
        if expected_content_size > crate::FRAGMENT_SIZE_THRESHOLD as u64 {
            return Err(StorageError::internal(format!(
                "fragment list chunk size {expected_content_size} exceeds FRAGMENT_SIZE_THRESHOLD {}",
                crate::FRAGMENT_SIZE_THRESHOLD
            )));
        }
        if frag_ref.hash.is_zero() {
            return Err(StorageError::internal(format!(
                "fragment list entry {i} at content offset {} has a zero hash",
                frag_ref.offset_content
            )));
        }

        let Some(clip) = clip_to_window(&entry, window) else {
            continue;
        };

        let leaf = LeafReference {
            hash: frag_ref.hash,
            target_offset: entry.start + clip.start - window.start,
            expected_size: expected_content_size,
            clip,
            context,
        };
        if leaf_tx.send(leaf).await.is_err() {
            break;
        }
    }
    Ok(())
}

/// Verifies that one sublist covers exactly the window its parent's list gives it.
///
/// Sublist offsets are absolute in the whole content, so in a well-formed tree a parent
/// entry's `offset_content` equals its sublist's own first offset, and the sublist expands to
/// exactly the distance to the next sibling — or, for the last entry, to the end of the level.
/// A sublist that falls short leaves a gap: the output is sized to the whole range before the
/// walk starts, so a range no leaf ever writes reads back as zeros and the file is renamed
/// into place as complete. One that overruns is the same fault from the other side, which is
/// why this compares against the window rather than only bounding it.
///
/// Checked against the window the parent's list derives rather than against a running total of
/// what previous siblings covered. The two say the same thing for a whole-content read — a
/// sibling's window ends where the next one begins — but only the window form survives a
/// ranged read, which visits some siblings and not others.
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
    window: &Range<u64>,
) -> Result<(), StorageError> {
    if parent.hash.is_zero() {
        return Err(StorageError::internal(format!(
            "fragment list entry at content offset {} has a zero hash",
            window.start
        )));
    }
    if sub_list.is_empty() {
        return Err(StorageError::internal(format!(
            "fragment sublist at offset {} is empty",
            window.start
        )));
    }
    if sub_content_size == 0 {
        return Err(StorageError::internal(format!(
            "fragment sublist at offset {} expands to zero bytes",
            window.start
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
    if end != window.end {
        return Err(StorageError::internal(format!(
            "fragment sublist at content offset {} expands to {sub_content_size} bytes but its \
             parent's list gives it {}",
            window.start,
            window.end - window.start
        )));
    }
    Ok(())
}

/// Walks the entries of an intermediate level that `window` reaches, recursing into each.
///
/// Entries the window misses are never loaded. That is the whole of what a ranged read saves
/// on a large tree: an entry's window comes from its parent's list, so the walk can rule out a
/// subtree — and everything under it — without a single fetch. `wanted` is the index range the
/// window reaches, whose first entry the caller's tier peek has already loaded.
#[allow(clippy::too_many_arguments)]
async fn walk_intermediate_level(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
    fragment_list: &[FragmentReference],
    level_end: u64,
    window: &Range<u64>,
    wanted: Range<usize>,
    first_frag: Fragment,
    first_buf: Bytes,
    leaf_tx: &Sender<LeafReference>,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
    depth: usize,
) -> Result<(), StorageError> {
    let first_index = wanted.start;
    let first_content_size = first_frag.size_content as usize;
    let first_payload_size = first_frag.size_payload as usize;
    if first_buf.len() < first_payload_size {
        return Err(StorageError::internal("insufficient buffer"));
    }
    let first_buffer = first_buf.to_aligned::<FragmentReference>();
    let first_list = first_buffer.as_type_slice::<FragmentReference>();

    let first_entry = entry_window(fragment_list, first_index, level_end)?;
    sublist_coverage(
        &fragment_list[first_index],
        first_list,
        first_content_size,
        &first_entry,
    )?;

    // Every child at this level is the same tier, so peeking at a wanted one settles it
    // without loading content the read did not ask for.
    let Some(peek) = wanted_entries(first_list, first_entry.end, window)? else {
        return Err(StorageError::internal(format!(
            "fragment sublist at content offset {} holds none of the range it was entered for",
            first_entry.start
        )));
    };
    let peek_address = Address {
        context,
        hash: first_list[peek.start].hash,
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

    let first_base_offset = first_list[0].offset_content;
    let mut result = if children_are_leaves {
        walk_leaf_level(
            first_list,
            first_content_size,
            first_base_offset,
            window,
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
            window,
            leaf_tx,
            options,
            remote_session.clone(),
            depth + 1,
        ))
        .await
    };

    let remaining = &fragment_list[first_index + 1..wanted.end];
    if result.is_err() || remaining.is_empty() {
        return result;
    }

    // The launcher outlives this borrow of `fragment_list`, so the hashes it needs are copied
    // out. Sized exactly, which the contiguity of `wanted` is what makes possible.
    let hashes: Vec<Hash> = remaining.iter().map(|entry| entry.hash).collect();

    type PrefetchMessage = (usize, JoinHandle<Result<(Fragment, Bytes), StorageError>>);
    let (prefetch_tx, mut prefetch_rx) = channel::<PrefetchMessage>(PIPELINE_WALKER_LOOKAHEAD);

    let launcher: JoinHandle<Result<(), StorageError>> = {
        let store = store.clone();
        let remote_session = remote_session.clone();
        let base_index = first_index + 1;
        lore_base::lore_spawn!(async move {
            for (offset, hash) in hashes.into_iter().enumerate() {
                let index = base_index + offset;
                let subaddress = Address { context, hash };
                let store = store.clone();
                let remote_session = remote_session.clone();
                let handle: JoinHandle<Result<(Fragment, Bytes), StorageError>> =
                    lore_base::lore_spawn!(async move {
                        load_fragment(store, partition, subaddress, options, remote_session).await
                    });

                if prefetch_tx.send((index, handle)).await.is_err() {
                    break;
                }
            }
            Ok(())
        })
    };

    // The index travels with its handle so the sublist that arrives is checked against the
    // parent entry it actually came from, rather than against a position recounted here.
    while let Some((index, handle)) = prefetch_rx.recv().await {
        if walk_abandoned(leaf_tx) {
            break;
        }

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

        let entry = match entry_window(fragment_list, index, level_end) {
            Ok(entry) => entry,
            Err(err) => {
                result = result.and(Err(err));
                continue;
            }
        };
        if let Err(err) =
            sublist_coverage(&fragment_list[index], sub_list, sub_content_size, &entry)
        {
            result = result.and(Err(err));
            continue;
        }

        let subresult = if children_are_leaves {
            walk_leaf_level(
                sub_list,
                sub_content_size,
                sub_list[0].offset_content,
                window,
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
                window,
                leaf_tx,
                options,
                remote_session.clone(),
                depth + 1,
            ))
            .await
        };
        result = result.and(subresult);
    }

    result.and(join_launcher(prefetch_rx, launcher).await)
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

    while let Some(leaf) = next_leaf(&mut leaf_rx, &data_tx).await {
        let permit = match reserve_leaf_budget(semaphore, &data_tx, leaf.expected_size).await {
            Ok(Some(permit)) => permit,
            Ok(None) => break,
            Err(e) => {
                result = Err(e);
                break;
            }
        };

        let tx = data_tx.clone();
        let offset = leaf.target_offset as usize;
        let subaddress = Address {
            context: leaf.context,
            hash: leaf.hash,
        };
        let store = store.clone();
        let remote_session = remote_session.clone();

        let expected_size = leaf.expected_size;
        let clip = leaf.clip.clone();
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
            // Narrowed only after the whole leaf has been loaded and checked. `slice` is a
            // view onto the same allocation, so a clipped leaf costs no copy.
            let buffer = buffer.slice(clip.start as usize..clip.end as usize);
            send_to_sink(&tx, (offset, buffer, permit)).await;
            Ok(())
        });

        // Collect any completed tasks
        while let Some(join_result) = tasks.try_join_next() {
            result = result.and(
                join_result
                    .map_err(|e| StorageError::internal_with_context(e, "task failure"))
                    .and_then(|r| r),
            );
        }
        if result.is_err() {
            break;
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

/// A fetched payload and the memory permit it is accounted against.
type FetchResult = Result<(Bytes, SemaphorePermit<'static>), StorageError>;

/// The next leaf to fetch, or `None` once there are none left or nobody is left to fetch for.
///
/// `queue_tx` is the channel the pool feeds, watched alongside the walker because a pool learns
/// it has been abandoned from a send that fails, and it has nothing to send while it is waiting
/// for a leaf. Both pools spawn their fetches rather than awaiting them, so waiting for a leaf
/// is the state a pool spends its time in, and a walk blocked on a peer would otherwise hold
/// the pipeline open behind it.
async fn next_leaf<T>(
    leaf_rx: &mut Receiver<LeafReference>,
    queue_tx: &Sender<T>,
) -> Option<LeafReference> {
    tokio::select! {
        leaf = leaf_rx.recv() => leaf,
        () = queue_tx.closed() => None,
    }
}

/// Reserves budget for one leaf, or `None` if the pipeline was abandoned while waiting.
///
/// The wait for budget is a pool's other parking spot, and it can be a long one: the permits
/// are held by payloads the consumer has yet to take. Rechecking after it means an abandoned
/// pool stops before spawning a fetch nobody will read rather than after.
async fn reserve_leaf_budget<T>(
    semaphore: &'static Semaphore,
    queue_tx: &Sender<T>,
    expected_size: u64,
) -> Result<Option<SemaphorePermit<'static>>, StorageError> {
    let permit = semaphore
        .acquire_many(fragment_permit_count(expected_size as usize))
        .await
        .map_err(|e| StorageError::internal_with_context(e, "permit"))?;
    Ok((!queue_tx.is_closed()).then_some(permit))
}

/// Hands a payload to the caller, reporting whether the caller has gone.
///
/// A send that fails means the receiver is gone: a content comparison that has already found a
/// difference, a reader that stopped early. That is not a failure of this pipeline, and there
/// is nobody left to report one to, so it is reported as abandonment rather than as an error.
/// The permit is released here rather than at load, so the budget bounds the pipeline.
async fn send_payload(
    sender: &Sender<Result<Bytes, StorageError>>,
    buffer: Bytes,
    permit: SemaphorePermit<'static>,
) -> bool {
    let abandoned = sender.send(Ok(buffer)).await.is_err();
    drop(permit);
    abandoned
}

/// Hands a fetched leaf to the write sink.
///
/// The permit travels inside the message rather than being released here, so the payload stays
/// accounted for until the write task that owns it has written it.
///
/// A send that fails is discarded rather than reported. [`write_to_file`] reads to the end
/// of the channel unless it has already failed, so a sink that has gone is a sink that has an
/// error of its own to report, and that error is the one saying what went wrong. Raising a
/// second one here would mask it, since [`defragment_pipeline`] takes the first of the three it
/// combines. The pool learns the sink has gone from [`next_leaf`].
async fn send_to_sink(sender: &DataSender, message: DataMessage) {
    let _abandoned = sender.send(message).await;
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
    sender: Sender<Result<Bytes, StorageError>>,
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
    sender: Sender<Result<Bytes, StorageError>>,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<(), StorageError> {
    // See fetch_unordered: defragmentation leaves are always decompressed.
    let options = options.with_decompress();

    // Sized so it never binds before the budget does; every payload in it holds a permit.
    let max_tasks = FRAGMENT_BUDGET_KIB / FRAGMENT_MINIMUM_COST_KIB as usize;
    let (fetch_queue_tx, mut fetch_queue_rx) = channel::<JoinHandle<FetchResult>>(max_tasks);

    // Launcher: read leaf refs from walker, spawn fetch tasks, push handles
    let launcher: JoinHandle<Result<(), StorageError>> = {
        let store = store.clone();
        let remote_session = remote_session.clone();
        lore_base::lore_spawn!(async move {
            while let Some(leaf) = next_leaf(&mut leaf_rx, &fetch_queue_tx).await {
                let Some(permit) =
                    reserve_leaf_budget(semaphore, &fetch_queue_tx, leaf.expected_size).await?
                else {
                    break;
                };

                let subaddress = Address {
                    context: leaf.context,
                    hash: leaf.hash,
                };
                let store = store.clone();
                let remote_session = remote_session.clone();
                let expected_size = leaf.expected_size;
                let clip = leaf.clip.clone();

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
                    // See `fetch_unordered`: clipped after the whole leaf is checked, and a
                    // view rather than a copy.
                    Ok((buffer.slice(clip.start as usize..clip.end as usize), permit))
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
                if send_payload(&sender, buffer, permit).await {
                    break;
                }
            }
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }

    result.and(join_launcher(fetch_queue_rx, launcher).await)
}

/// The outcome of a pipeline stage, with a task that did not run to completion counted as a
/// failure of the stage it was running.
fn joined(stage: Result<Result<(), StorageError>, JoinError>) -> Result<(), StorageError> {
    stage
        .map_err(|e| StorageError::internal_with_context(e, "task failure"))
        .and_then(|r| r)
}

/// Drains `(offset, data, permit)` messages from the fetch pool and writes each
/// payload at its offset.
///
/// Positional writes carry their own offset, so concurrent writes to disjoint ranges
/// need no lock — the previous seek-plus-write sink had to serialize behind a mutex
/// because the pair is not atomic. Each write is one task awaiting a driver operation,
/// so no runtime worker blocks on the syscall and independent writes overlap. Completed
/// writes are reaped each iteration; the rest are joined after the channel closes,
/// including after an early error break.
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
async fn write_to_file(
    file: IoFile,
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
        lore_base::lore_spawn!(tasks, async move {
            let _permit = permit;
            file.write_all_at(payload, offset as u64)
                .await
                .map(|_returned| ())
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

/// Unified streaming defragmentation pipeline.
///
/// `range` is the content the caller asked for, counted from the start of the content and
/// already clamped to it — see [`crate::read::resolve_content_range`]. Leaves outside it are
/// never fetched and the subtrees holding none of it are never walked, so the work is
/// proportional to the range rather than to the content. Everything delivered is positioned
/// relative to `range.start`, which for a whole-content read leaves offsets exactly where
/// they were.
#[allow(clippy::too_many_arguments)]
pub async fn defragment_pipeline(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    fragment: Fragment,
    source_buffer: Bytes,
    range: Range<u64>,
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
        range,
        leaf_tx,
        options,
        session_walker,
    ));

    match sink {
        DefragmentSink::Stream { sender } => {
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
            joined(walk_result).and(joined(fetch_result))
        }
        DefragmentSink::File { file, size } => {
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

            let writer = lore_base::lore_spawn!(write_to_file(file, size, data_rx));

            let (walk_result, fetch_result, write_result) = tokio::join!(walker, fetcher, writer);
            joined(walk_result)
                .and(joined(fetch_result))
                .and(joined(write_result))
        }
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

/// Opens a file for positional writing and sizes it to the whole content up front.
///
/// The handle is shared: positional writes carry their own offset, so concurrent writers to
/// disjoint ranges need no exclusion. Clones of the returned handle share it.
///
/// The size is set rather than the file truncated, so a range no payload covers reads as zeros
/// instead of shortening the file — which is what the sink's byte-count check exists to catch.
pub async fn open_file_write(
    path: impl AsRef<Path>,
    size: usize,
) -> Result<IoFile, std::io::Error> {
    let file = IoDriver::global()
        .open(
            path,
            &OpenOptions::new().read(true).write(true).create(true),
        )
        .await?;
    file.set_len(size as u64).await?;
    Ok(file)
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
        use super::*;
        use crate::test_util::TempDir;

        const SIZE: usize = 100;

        /// A sized target file, opened the way the materialization path opens one.
        async fn target(dir: &TempDir, name: &str) -> IoFile {
            super::super::open_file_write(dir.path().join(name), SIZE)
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

            super::super::write_to_file(file.clone(), SIZE, rx)
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

            let err = super::super::write_to_file(file, SIZE, rx)
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

            let err = super::super::write_to_file(file, SIZE, rx)
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

            super::super::write_to_file(file, SIZE, rx)
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

            let err = super::super::write_to_file(file, SIZE, rx)
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
        use crate::StoreError;
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
            let (root_fragment, root_buffer) =
                crate::read::load_fragment(store.clone(), partition, root_address, options, None)
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
            let reserve = lore_base::lore_spawn!(async move {
                reserve_leaf_budget(budget, &queue_tx, PAYLOAD).await
            });

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
            let (root_fragment, root_buffer) =
                crate::read::load_fragment(store.clone(), partition, root_address, options, None)
                    .await
                    .expect("load root list");

            let file = super::super::open_file_write(dir.path().join("truncated.bin"), PAYLOAD)
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
            ) -> Result<crate::store_types::StoreGetData, StoreError> {
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
            ) -> Result<crate::store_types::StoreGetData, StoreError> {
                self.inner.clone().get_metadata(partition, address).await
            }

            async fn query(
                self: Arc<Self>,
                partition: Partition,
                addresses: &[Address],
                results: &mut [crate::store_types::StoreMatchResult],
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
                stats: Arc<crate::store_types::StoreObliterateStats>,
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
                sink: Option<crate::gc_event::GcEventSinkRef>,
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
                sink: Option<crate::gc_event::GcEventSinkRef>,
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
                durable: bool,
            ) -> Result<(), StoreError> {
                self.inner
                    .clone()
                    .copy(
                        source_partition,
                        source_address,
                        destination_partition,
                        destination_context,
                        durable,
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
                let (leaf, _) =
                    put_leaf(store, partition, context, vec![index as u8; payload]).await;
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
            let (root_fragment, root_buffer) =
                crate::read::load_fragment(store.clone(), partition, root_address, options, None)
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
            let (root_fragment, root_buffer) =
                crate::read::load_fragment(store.clone(), partition, root_address, options, None)
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
            crate::read::read_into_file(
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
            let err = crate::read::read_into_file(
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
            let err = crate::read::read_into_file(
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
            crate::read::read_into_file(
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
            let err = crate::read::read_into_file(
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
            let err = crate::read::read_into_file(
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
            let err = crate::read::read_into_file(
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
            let err = crate::read::read_into_file(
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
            let err = crate::read::read_into_file(
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
            let err = crate::read::read_into_file(
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
            let err = crate::read::read_into_file(
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
            let err = crate::read::read_into_file(
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
    }
}
