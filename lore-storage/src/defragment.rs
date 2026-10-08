// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
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
#[lore_macro::test_pub]
type DataMessage = (usize, Bytes, tokio::sync::SemaphorePermit<'static>);
#[lore_macro::test_pub]
type DataSender = Sender<DataMessage>;
type DataReceiver = Receiver<DataMessage>;

/// Leaf fragment reference yielded by the tree walker to the fetch pool.
#[lore_macro::test_pub]
#[cfg_attr(feature = "test-util", derive(Debug))]
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
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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

        let Ok(slot) = leaf_tx.reserve().await else {
            break;
        };
        slot.send(LeafReference {
            hash: frag_ref.hash,
            target_offset: entry.start + clip.start - window.start,
            expected_size: expected_content_size,
            clip,
            context,
        });
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
#[lore_macro::test_pub]
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

/// A fetched leaf and the memory permit it is accounted against.
type FetchResult<T> = Result<(T, SemaphorePermit<'static>), StorageError>;

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
#[lore_macro::test_pub]
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
/// A slot that cannot be reserved means the receiver is gone: a content comparison that has
/// already found a difference, a reader that stopped early. That is not a failure of this
/// pipeline, and there is nobody left to report one to, so it is reported as abandonment rather
/// than as an error. The permit is released here rather than at load, so the budget bounds the
/// pipeline.
#[lore_macro::test_pub]
async fn send_payload<T>(
    sender: &Sender<Result<T, StorageError>>,
    leaf: T,
    permit: SemaphorePermit<'static>,
) -> bool {
    let abandoned = sender
        .reserve()
        .await
        .map(|slot| slot.send(Ok(leaf)))
        .is_err();
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
///
/// Returns [`fetch_ordered_and_stream_from`]'s future itself: a future of its own would hold the
/// arguments again beside it.
#[lore_macro::test_pub]
fn fetch_ordered_and_stream(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    leaf_rx: Receiver<LeafReference>,
    sender: Sender<Result<Bytes, StorageError>>,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
) -> impl Future<Output = Result<(), StorageError>> {
    fetch_ordered_and_stream_from(
        fragment_limiter(),
        store,
        partition,
        leaf_rx,
        sender,
        options,
        remote_session,
    )
}

/// [`fetch_ordered_and_stream`] with the fragment budget taken from `semaphore`.
#[lore_macro::test_pub]
fn fetch_ordered_and_stream_from(
    semaphore: &'static Semaphore,
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    leaf_rx: Receiver<LeafReference>,
    sender: Sender<Result<Bytes, StorageError>>,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
) -> impl Future<Output = Result<(), StorageError>> {
    // See fetch_unordered: defragmentation leaves are always decompressed.
    fetch_ordered_from(
        semaphore,
        store,
        partition,
        leaf_rx,
        sender,
        options.with_decompress(),
        remote_session,
        content_leaf,
    )
}

/// [`fetch_ordered_and_stream`] delivering each leaf whole with its fragment, loaded as `options`
/// asks.
fn fetch_ordered_leaves(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    leaf_rx: Receiver<LeafReference>,
    sender: Sender<Result<(Fragment, Bytes), StorageError>>,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
) -> impl Future<Output = Result<(), StorageError>> {
    fetch_ordered_from(
        fragment_limiter(),
        store,
        partition,
        leaf_rx,
        sender,
        options,
        remote_session,
        whole_leaf,
    )
}

/// A leaf loaded expanded, as the part of its content that was asked for.
fn content_leaf(
    _fragment: Fragment,
    buffer: Bytes,
    expected_size: u64,
    clip: Range<u64>,
) -> Result<Bytes, StorageError> {
    if buffer.len() as u64 != expected_size {
        return Err(StorageError::internal(format!(
            "leaf fragment content size {} does not match expected {expected_size}",
            buffer.len()
        )));
    }
    // See `fetch_unordered`: clipped after the whole leaf is checked, and a view rather than a
    // copy.
    Ok(buffer.slice(clip.start as usize..clip.end as usize))
}

/// A leaf as loaded, with its fragment: held to the content size its parent list states, and its
/// payload to the fragment. It is delivered whole, since a payload left compressed cannot be cut.
#[lore_macro::test_pub]
fn whole_leaf(
    fragment: Fragment,
    payload: Bytes,
    expected_size: u64,
    clip: Range<u64>,
) -> Result<(Fragment, Bytes), StorageError> {
    if fragment.size_content != expected_size {
        return Err(StorageError::internal(format!(
            "leaf fragment content size {} does not match expected {expected_size}",
            fragment.size_content
        )));
    }
    if clip != (0..expected_size) {
        return Err(StorageError::internal(
            "a leaf with its fragment is delivered whole, not in part",
        ));
    }
    if payload.len() != fragment.size_payload as usize
        || ((fragment.flags & FragmentFlags::PayloadCompressed) == 0
            && fragment.size_payload as u64 != fragment.size_content)
    {
        return Err(StorageError::internal(format!(
            "leaf payload of {} bytes does not match its fragment",
            payload.len()
        )));
    }
    Ok((fragment, payload))
}

/// The ordered fetch pool behind [`fetch_ordered_and_stream`]. Each leaf is loaded under
/// `options` and handed to `make_leaf` with the content size its parent list states and the part
/// of it that was asked for; what that returns is what the caller receives.
#[allow(clippy::too_many_arguments)]
async fn fetch_ordered_from<T, F>(
    semaphore: &'static Semaphore,
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    mut leaf_rx: Receiver<LeafReference>,
    sender: Sender<Result<T, StorageError>>,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
    make_leaf: F,
) -> Result<(), StorageError>
where
    T: Send + 'static,
    F: Fn(Fragment, Bytes, u64, Range<u64>) -> Result<T, StorageError> + Copy + Send + 'static,
{
    // Sized so it never binds before the budget does; every payload in it holds a permit.
    let max_tasks = FRAGMENT_BUDGET_KIB / FRAGMENT_MINIMUM_COST_KIB as usize;
    let (fetch_queue_tx, mut fetch_queue_rx) = channel::<JoinHandle<FetchResult<T>>>(max_tasks);

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

                let handle: JoinHandle<FetchResult<T>> = lore_base::lore_spawn!(async move {
                    let (loaded_fragment, buffer) =
                        load_fragment(store, partition, subaddress, options, remote_session)
                            .await?;
                    if loaded_fragment.flags & FragmentFlags::PayloadFragmented != 0 {
                        return Err(StorageError::internal(
                            "expected leaf fragment but peer returned an intermediate fragment list",
                        ));
                    }
                    Ok((
                        make_leaf(loaded_fragment, buffer, expected_size, clip)?,
                        permit,
                    ))
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
            Ok((leaf, permit)) => {
                if send_payload(&sender, leaf, permit).await {
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
#[lore_macro::test_pub]
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

/// [`defragment_pipeline`] delivering a whole content's leaves in content order on `sender`, each
/// with its fragment and loaded as `options` asks.
///
/// Not a [`DefragmentSink`] variant, which would grow every future holding a sink.
#[allow(clippy::too_many_arguments)]
pub async fn defragment_pipeline_leaves(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    fragment: Fragment,
    source_buffer: Bytes,
    sender: Sender<Result<(Fragment, Bytes), StorageError>>,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<(), StorageError> {
    let (leaf_tx, leaf_rx) = channel::<LeafReference>(PIPELINE_LEAF_CHANNEL_SIZE);

    let store_walker = store.clone();
    let session_walker = remote_session.clone();
    let walker = lore_base::lore_spawn!(walk_fragment_tree(
        store_walker,
        partition,
        address,
        fragment,
        source_buffer,
        0..fragment.size_content,
        leaf_tx,
        options,
        session_walker,
    ));
    let fetcher = lore_base::lore_spawn!(fetch_ordered_leaves(
        store,
        partition,
        leaf_rx,
        sender,
        options,
        remote_session,
    ));

    let (walk_result, fetch_result) = tokio::join!(walker, fetcher);
    joined(walk_result).and(joined(fetch_result))
}

/// A destination a defragmenting read divides among the leaves it walks.
///
/// Each leaf writes one disjoint piece, so a target only has to hand those pieces out and let each
/// be written on its own. [`BytesMut`] holds the pieces of a buffer the read allocates;
/// [`CallerBuffer`](crate::CallerBuffer) holds the pieces of one the caller already owns, which the
/// leaves then write without the content being assembled anywhere else first.
pub trait DefragmentTarget: Send + 'static {
    /// The bytes this piece covers.
    fn len(&self) -> usize;

    /// Whether this piece covers no bytes.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Split at `at`, keeping `..at` and returning `at..`.
    fn split_off(&mut self, at: usize) -> Self;

    /// The piece as a slice, for a leaf that holds its bytes and only has to place them.
    fn as_mut_slice(&mut self) -> &mut [u8];

    /// This piece as memory a read can land in, for a leaf that can be read into place rather than
    /// loaded and copied.
    ///
    /// `None` for a target the read has no way to write into directly, which is answered by loading
    /// the leaf and copying it in. The handle names the same memory as the piece it was taken from,
    /// so the piece must not be written through while it is alive.
    fn as_caller_buffer(&mut self) -> Option<crate::CallerBuffer>;
}

impl DefragmentTarget for BytesMut {
    fn len(&self) -> usize {
        BytesMut::len(self)
    }

    fn split_off(&mut self, at: usize) -> Self {
        BytesMut::split_off(self, at)
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        self
    }

    fn as_caller_buffer(&mut self) -> Option<crate::CallerBuffer> {
        None
    }
}

impl DefragmentTarget for crate::CallerBuffer {
    fn len(&self) -> usize {
        crate::CallerBuffer::len(self)
    }

    fn split_off(&mut self, at: usize) -> Self {
        crate::CallerBuffer::split_off(self, at)
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        crate::CallerBuffer::as_mut_slice(self)
    }

    fn as_caller_buffer(&mut self) -> Option<crate::CallerBuffer> {
        let piece = crate::CallerBuffer::as_mut_slice(self);
        let (ptr, len) = (piece.as_mut_ptr(), piece.len());
        // SAFETY: the handle names exactly this piece and nothing wider, so it inherits the contract
        // `CallerBuffer::new` was first constructed under. Its user drops it before this piece is
        // written through again.
        Some(unsafe { crate::CallerBuffer::new(ptr, len) })
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn read_defragment<Target: DefragmentTarget>(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    range: Range<usize>,
    fragment: Fragment,
    source_buffer: Bytes,
    mut target: Target,
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

/// Read the leaf at `address` into `target`'s own memory, so nothing is allocated for its payload
/// and nothing is copied out of it.
///
/// `false` leaves the leaf to be loaded and cut instead: the range clips it, so it is not the whole
/// content its address names; the leaf is not the size the list claimed for it; `target` is memory
/// the read cannot write into directly; or the read declined, which the assembling path answers by
/// reaching the remote and healing.
#[allow(clippy::too_many_arguments)]
async fn place_whole_leaf<Target: DefragmentTarget>(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    range: &Range<usize>,
    target: &mut Target,
    options: ReadOptions,
    depth: usize,
    remote_session: Option<Arc<StorageSession>>,
) -> Result<bool, StorageError> {
    if range.start != 0 {
        return Ok(false);
    }
    let Some(mut dst) = target.as_caller_buffer() else {
        return Ok(false);
    };

    let piece = dst.len();
    match crate::read::read_content_into_buffer(
        store,
        partition,
        address,
        &mut dst,
        options,
        crate::read::RootSource::LocalOrRemote,
        depth,
        remote_session,
    )
    .await
    {
        Ok(Some((_, written))) => Ok(written == piece),
        Ok(None) => Ok(false),
        Err(err) if err.is_oversized() => Ok(false),
        Err(err) => Err(err),
    }
}

#[allow(clippy::too_many_arguments)]
fn read_defragment_subread<Target: DefragmentTarget>(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    range: Range<usize>,
    mut target: Target,
    options: ReadOptions,
    depth: usize,
    remote_session: Option<Arc<StorageSession>>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), StorageError>> + Send>> {
    Box::pin(async move {
        if place_whole_leaf(
            store.clone(),
            partition,
            address,
            &range,
            &mut target,
            options,
            depth,
            remote_session.clone(),
        )
        .await?
        {
            return Ok(());
        }

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
        } else {
            let available = target.len();
            let Some(leaf) = buffer.get(range.clone()) else {
                return Err(StorageError::internal(format!(
                    "unexpected size: buffer {} vs range {range:?}",
                    buffer.len()
                )));
            };
            let Some(place) = target.as_mut_slice().get_mut(..leaf.len()) else {
                return Err(StorageError::internal(format!(
                    "unexpected size: target {available} vs range {}",
                    leaf.len()
                )));
            };
            place.copy_from_slice(leaf);
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
