// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use lore_base::runtime::DetachingJoinSet;
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
use tokio_util::task::AbortOnDropHandle;

use crate::concurrency::FRAGMENT_MINIMUM_COST_KIB;
use crate::concurrency::fragment_budget_kib;
use crate::concurrency::fragment_limiter;
use crate::concurrency::fragment_permit_count;
use crate::error::StorageError;
use crate::fragment_flags::FragmentFlags;
use crate::hash;
use crate::immutable_store::ImmutableStore;
use crate::local::store_lock::StoreGuard;
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
    /// Write at offset to a file (unordered, concurrent positional writes), opened by
    /// [`open_file_write`] so every range of it starts unallocated, or by a resuming read
    /// ([`ReadOptions::resume_write`]) as [`crate::resume`] describes.
    /// `size` is the expected content length, used to reject out-of-range offsets. With
    /// [`ReadOptions::resume_write`], the pipeline's tasks end with it when it is dropped, and
    /// with [`ReadOptions::resume_held`], a whole leaf the file already holds at its offset,
    /// checked against the leaf's hash, is not fetched again.
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

/// What the fetch pool hands the write sink.
#[lore_macro::test_pub]
enum DataMessage {
    /// A fetched payload: target offset, bytes, and the fragment memory permit covering those
    /// bytes. The permit rides along so it is released when the write completes rather than when
    /// the fetch did.
    Payload(usize, Bytes, tokio::sync::SemaphorePermit<'static>),
    /// A hole: target offset and length, a range holding no data, which reads as zeros.
    Hole(usize, u64),
    /// A range the file already holds: target offset and length, a whole leaf a resuming read
    /// checked there against its hash ([`ReadOptions::resume_held`]).
    Held(usize, u64),
}
#[lore_macro::test_pub]
type DataSender = Sender<DataMessage>;
type DataReceiver = Receiver<DataMessage>;

/// Zeros a hole is delivered from where it is delivered as bytes, a fragment's worth at a time,
/// so no hole allocates.
static ZEROS: [u8; crate::FRAGMENT_SIZE_THRESHOLD] = [0; crate::FRAGMENT_SIZE_THRESHOLD];

/// The first piece of a hole of `len` bytes delivered as zeros, and the bytes it covers.
fn zero_piece(len: u64) -> (Bytes, u64) {
    let covered = len.min(ZEROS.len() as u64);
    (Bytes::from_static(&ZEROS[..covered as usize]), covered)
}

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
    /// loaded payload against this rather than against `clip`: a payload is stored whole, a
    /// fetched one is checked whole against the hash that names it, and a compressed one
    /// expands whole, so the whole leaf is what gets loaded whatever part of it the caller wants.
    expected_size: u64,
    /// The part of this leaf the read asked for, relative to the leaf's own start. Whole
    /// leaves carry `0..expected_size`; only the first and last leaf of a ranged read carry
    /// anything narrower. Applied after the payload is loaded, so it narrows what is
    /// delivered rather than what is read.
    clip: Range<u64>,
    context: Context,
}

impl LeafReference {
    /// Whether this leaf is a hole, which names no fragment; see [`FragmentReference::is_hole`].
    fn is_hole(&self) -> bool {
        self.hash.is_zero()
    }
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
/// fetches on a deeply nested tree, and ends a walk through a list naming itself
/// or an ancestor.
pub const MAX_FRAGMENT_TREE_DEPTH: usize = 8;

/// Walks the fragment tree depth-first with prefetch pipelining, yielding leaf
/// fragment references into the provided channel.
///
/// `range` is the content the caller asked for, counted from the start of the content and
/// already clamped to it. Offsets inside a fragment tree are absolute within the content,
/// so the range is rebased onto the root list's own first offset once here and every level
/// below compares against it directly.
///
/// Holds `claim`, the pipeline's claim on `store`, for as long as it walks: it runs as a task of
/// its own, which a dropped pipeline leaves running past the span that covered the pipeline.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[lore_macro::test_pub]
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
fn walk_fragment_tree(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    fragment: Fragment,
    source_buffer: Bytes,
    range: Range<u64>,
    leaf_tx: Sender<LeafReference>,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
    claim: Option<Arc<StoreGuard>>,
) -> impl Future<Output = Result<(), StorageError>> {
    async move {
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
            &claim,
            0,
        )
        .await
    }
}

/// The absolute content window one entry of a fragment list stands for: from its own offset
/// to where the next entry starts, and for the last entry to the end of the level.
///
/// Derived from the list alone rather than from the fragment the entry names, which is what
/// lets a ranged walk decide an entry is not wanted before paying to load it. An entry with the
/// zero hash is a hole, which names no fragment and spans its window as any entry does.
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

/// The contiguous index range of a level's entries that `window` reaches, or `None` when it
/// reaches none. Every entry is checked, wanted or not: offsets strictly increasing up to
/// `level_end`, and no empty entry.
pub fn wanted_entries(
    fragment_list: &[FragmentReference],
    level_end: u64,
    window: &Range<u64>,
) -> Result<Option<Range<usize>>, StorageError> {
    scan_entries(fragment_list, level_end, window).map(|scan| scan.wanted)
}

/// What one pass over a level's entries establishes.
pub struct LevelScan {
    /// See [`wanted_entries`].
    pub wanted: Option<Range<usize>>,
    /// Whether any entry is a hole. Only a list of leaves holds holes, so a level holding one is
    /// known to be a leaf level without loading an entry.
    pub holes: bool,
}

/// [`wanted_entries`], noting on the same pass whether the level holds a hole.
pub fn scan_entries(
    fragment_list: &[FragmentReference],
    level_end: u64,
    window: &Range<u64>,
) -> Result<LevelScan, StorageError> {
    let mut wanted: Option<Range<usize>> = None;
    let mut holes = false;
    for index in 0..fragment_list.len() {
        let entry = entry_window(fragment_list, index, level_end)?;
        holes |= fragment_list[index].is_hole();
        if clip_to_window(&entry, window).is_some() {
            wanted = Some(match wanted {
                Some(range) => range.start..index + 1,
                None => index..index + 1,
            });
        }
    }
    Ok(LevelScan { wanted, holes })
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
/// down. A level holding a hole is a leaf level, which takes no peek.
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
    claim: &Option<Arc<StoreGuard>>,
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
    let scan = scan_entries(fragment_list, level_end, window)?;
    let Some(wanted) = scan.wanted else {
        return Ok(());
    };
    if scan.holes {
        return walk_leaf_level(
            fragment_list,
            total_content_size,
            base_offset,
            window,
            context,
            leaf_tx,
        )
        .await;
    }
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
            claim,
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
///
/// A hole is sent as a reference with the zero hash, which the fetch pools deliver without a
/// fetch. It is the one entry that may span more than a fragment.
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
        if expected_content_size > crate::FRAGMENT_SIZE_THRESHOLD as u64 && !frag_ref.is_hole() {
            return Err(StorageError::internal(format!(
                "fragment list chunk size {expected_content_size} exceeds FRAGMENT_SIZE_THRESHOLD {}",
                crate::FRAGMENT_SIZE_THRESHOLD
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

/// Checks that `sub_list`, expanding to `sub_content_size` bytes, covers exactly `window`, the
/// window its `parent` entry's list gives it: the window and the sublist start at the parent's
/// offset, and the sublist ends at the window's end. A zero-hash parent, a reversed window, an
/// empty sublist and one expanding to nothing are rejected.
pub fn sublist_coverage(
    parent: &FragmentReference,
    sub_list: &[FragmentReference],
    sub_content_size: usize,
    window: &Range<u64>,
) -> Result<(), StorageError> {
    if parent.is_hole() {
        return Err(StorageError::internal(format!(
            "fragment list entry at content offset {} has a zero hash",
            window.start
        )));
    }
    if parent.offset_content != window.start || window.end < window.start {
        return Err(StorageError::internal(format!(
            "fragment list entry at content offset {} is given the window {window:?}",
            parent.offset_content
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
///
/// The launcher and every load it spawns hold `claim`: an abandoned walk detaches the loads
/// queued, which then finish past the walk.
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
    claim: &Option<Arc<StoreGuard>>,
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
    // without loading content the read did not ask for. A sublist holding a hole is a list of
    // leaves, which settles it without a load.
    let first_scan = scan_entries(first_list, first_entry.end, window)?;
    let Some(peek) = first_scan.wanted else {
        return Err(StorageError::internal(format!(
            "fragment sublist at content offset {} holds none of the range it was entered for",
            first_entry.start
        )));
    };
    let children_are_leaves = first_scan.holes || {
        let peek_address = Address {
            context,
            hash: first_list[peek.start].hash,
        };
        let (peek_frag, _peek_buf) = load_fragment(
            store.clone(),
            partition,
            peek_address,
            options,
            remote_session.clone(),
        )
        .await?;
        (peek_frag.flags & FragmentFlags::PayloadFragmented) != FragmentFlags::PayloadFragmented
    };

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
            claim,
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
        let claim = claim.clone();
        let base_index = first_index + 1;
        lore_base::lore_spawn!(async move {
            for (offset, hash) in hashes.into_iter().enumerate() {
                let index = base_index + offset;
                let subaddress = Address { context, hash };
                let store = store.clone();
                let remote_session = remote_session.clone();
                let claim = claim.clone();
                let handle: JoinHandle<Result<(Fragment, Bytes), StorageError>> =
                    lore_base::lore_spawn!(async move {
                        let _claim = claim;
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
                claim,
                depth + 1,
            ))
            .await
        };
        result = result.and(subresult);
    }

    result.and(join_launcher(prefetch_rx, launcher).await)
}

/// Unordered fetch pool for file targets of a read that does not resume.
///
/// Holds `claim` for as long as it fetches, for the reason [`walk_fragment_tree`] gives. Its
/// fetches end before it does: a pipeline that is dropped detaches it, and it drains them.
#[lore_macro::test_pub]
async fn fetch_unordered(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    mut leaf_rx: Receiver<LeafReference>,
    data_tx: DataSender,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
    _claim: Option<Arc<StoreGuard>>,
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
        if leaf.is_hole() {
            send_hole_to_sink(&data_tx, &leaf).await;
            continue;
        }
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
            let buffer = checked_leaf(&loaded_fragment, buffer, expected_size, &clip)?;
            send_to_sink(&tx, DataMessage::Payload(offset, buffer, permit)).await;
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

/// Unordered fetch pool for the file target of a read that resumes ([`ReadOptions::resume_write`]).
///
/// `resumed` is the file the sink writes where it already holds part of the content
/// ([`ReadOptions::resume_held`]). The pool takes one map of the ranges it holds data for before
/// the sink writes to it ([`DataMap`]). A leaf whose whole range is data there is handed to the
/// sink as held where the file holds it ([`place_leaf`]), and fetched otherwise. A hole, a
/// reference of the zero hash, is handed to the sink as one, unwritten: the file was emptied before
/// any write of its content, and no write fills a hole. Where a whole block of the hole is data in
/// the map, zeros or not, and the sink's punch does not free it ([`DataMap::data_in_hole`]), or
/// where the file cannot be mapped, the pool fails with [`Unresumable`] at once. The parts of the
/// hole that are data in the blocks it shares with the leaves beside it are read back by a task of
/// the pool's ([`check_hole_edges`]), under the budget a leaf's read-back takes, which fails the
/// pool the same way where they are not all zeros or cannot be read. Where the pool fails so, the
/// read writes the file again as a fresh one, whatever else its tasks failed with
/// ([`and_joined`]).
///
/// Holds `claim` for as long as it fetches, for the reason [`walk_fragment_tree`] gives, and so
/// does each of its fetches. The pipeline aborts it when it is dropped; its fetches are then
/// detached, and each finishes the request it is in the middle of, for the reason
/// [`join_launcher`] gives, still holding `claim`.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
fn fetch_resuming(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    mut leaf_rx: Receiver<LeafReference>,
    data_tx: DataSender,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
    resumed: Option<IoFile>,
    claim: Option<Arc<StoreGuard>>,
) -> impl Future<Output = Result<(), StorageError>> {
    let options = options.with_decompress();
    async move {
        let semaphore = fragment_limiter();
        let mut tasks = DetachingJoinSet::default();
        let mut result = Ok(());
        let mut map = match &resumed {
            Some(file) => match DataMap::of(file).await {
                Ok(map) => Some(map),
                Err(err) => {
                    return Err(StorageError::internal_with_context(
                        Unresumable(Some(err)),
                        "map a temporary file",
                    ));
                }
            },
            None => None,
        };

        while let Some(leaf) = next_leaf(&mut leaf_rx, &data_tx).await {
            let end = leaf.target_offset + (leaf.clip.end - leaf.clip.start);
            if leaf.is_hole() {
                if let (Some(map), Some(file)) = (map.as_mut(), resumed.as_ref()) {
                    let Some(parts) = map.data_in_hole(leaf.target_offset..end) else {
                        result = Err(unresumable(None));
                        break;
                    };
                    let read_back: u64 = parts.iter().map(|part| part.end - part.start).sum();
                    if read_back > 0 {
                        let permit = match reserve_leaf_budget(semaphore, &data_tx, read_back).await
                        {
                            Ok(Some(permit)) => permit,
                            Ok(None) => break,
                            Err(e) => {
                                result = Err(e);
                                break;
                            }
                        };
                        lore_base::lore_spawn!(
                            tasks,
                            check_hole_edges(file.clone(), parts, permit)
                        );
                    }
                }
                send_hole_to_sink(&data_tx, &leaf).await;
                continue;
            }
            let held_there = map
                .as_mut()
                .is_some_and(|map| map.is_data(leaf.target_offset..end));
            let permit = match reserve_leaf_budget(semaphore, &data_tx, leaf.expected_size).await {
                Ok(Some(permit)) => permit,
                Ok(None) => break,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            };

            lore_base::lore_spawn!(
                tasks,
                place_leaf(
                    store.clone(),
                    partition,
                    leaf,
                    resumed.as_ref().filter(|_| held_there).cloned(),
                    data_tx.clone(),
                    permit,
                    options,
                    remote_session.clone(),
                    claim.clone(),
                )
            );

            while let Some(join_result) = tasks.try_join_next() {
                result = and_joined(result, join_result);
            }
            if result.is_err() {
                break;
            }
        }

        while let Some(join_result) = tasks.join_next().await {
            result = and_joined(result, join_result);
        }

        drop(claim);
        result
    }
}

/// `result` with the outcome `joined` of one of [`fetch_resuming`]'s tasks: the first error, but an
/// [`Unresumable`] ahead of any other. A hole's check that fails means the read writes the file
/// afresh, whatever another task failed with, as it did where the check ran before the tasks
/// after it were spawned.
fn and_joined(
    result: Result<(), StorageError>,
    joined: Result<Result<(), StorageError>, JoinError>,
) -> Result<(), StorageError> {
    let joined = joined
        .map_err(|e| StorageError::internal_with_context(e, "task failure"))
        .and_then(|r| r);
    match (result, joined) {
        (Err(first), Err(later)) if !cannot_resume(&first) && cannot_resume(&later) => Err(later),
        (result, joined) => result.and(joined),
    }
}

/// The [`Unresumable`] a resuming read's fetch pool fails with, holding the error of a read that
/// failed.
fn unresumable(err: Option<std::io::Error>) -> StorageError {
    StorageError::internal_with_context(Unresumable(err), "resume a temporary file")
}

/// Where a resuming read's fetch pool finds data in a hole of the content in the file it resumes,
/// or cannot map that file or read back a part of a hole in it ([`fetch_resuming`]): what the read
/// takes to write the file again as a fresh one, rather than fail every retry. Holds the error of a
/// map or a read that failed.
#[derive(Debug)]
pub(crate) struct Unresumable(Option<std::io::Error>);

impl std::fmt::Display for Unresumable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            None => f.write_str("the temporary file holds data in a hole of the content"),
            Some(err) => write!(f, "the temporary file cannot be checked: {err}"),
        }
    }
}

impl std::error::Error for Unresumable {}

/// Whether `err` is the [`Unresumable`] a resuming read's fetch pool failed with.
pub(crate) fn cannot_resume(err: &StorageError) -> bool {
    std::iter::successors(Some(err as &(dyn std::error::Error + 'static)), |err| {
        err.source()
    })
    .any(|err| err.is::<Unresumable>())
}

/// The ranges a resumed temporary file holds data for, as its file system reports them, taken once
/// when the fetch pool starts and before the sink writes to the file: sorted, in one `Vec`, of 16
/// bytes per range of data, with a cursor the pool moves forward as it walks the leaves in content
/// order. It keeps the file's length, and the file system's block its walk took
/// ([`lore_io::DataCursor::block`]), which a leaf allocates whole wherever it writes to it. Where
/// the file system reports no holes, the whole file is one range of data. On Apple platforms it
/// keeps whether the sink's punch frees the whole blocks of every hole ([`IoFile::holds_holes`]).
struct DataMap {
    ranges: Vec<Range<u64>>,
    next: usize,
    block: u64,
    len: u64,
    #[cfg(target_vendor = "apple")]
    punches: bool,
}

impl DataMap {
    /// The map of `file`, one walk of its ranges of data with one [`lore_io::DataCursor`].
    async fn of(file: &IoFile) -> std::io::Result<Self> {
        let len = file.metadata().await?.len();
        let mut ranges: Vec<Range<u64>> = Vec::new();
        let mut cursor = lore_io::DataCursor::default();
        let mut offset = 0;
        while offset < len {
            let (found, kept) = file.next_data(offset, cursor).await;
            cursor = kept;
            let Some(data) = found? else {
                break;
            };
            let data = data.start..data.end.min(len);
            if data.is_empty() {
                break;
            }
            offset = data.end;
            match ranges.last_mut() {
                Some(last) if last.end >= data.start => last.end = data.end,
                _ => ranges.push(data),
            }
        }
        Ok(Self {
            ranges,
            next: 0,
            block: cursor.block().unwrap_or(1),
            len,
            #[cfg(target_vendor = "apple")]
            punches: file.holds_holes().await,
        })
    }

    /// Moves the cursor past the ranges ending at or before `offset`.
    fn skip_to(&mut self, offset: u64) {
        while self
            .ranges
            .get(self.next)
            .is_some_and(|data| data.end <= offset)
        {
            self.next += 1;
        }
    }

    /// Whether the whole of `range`, a leaf's at or after the last one asked about, is data.
    fn is_data(&mut self, range: Range<u64>) -> bool {
        self.skip_to(range.start);
        self.ranges
            .get(self.next)
            .is_some_and(|data| data.start <= range.start && range.end <= data.end)
    }

    /// The part of `range`, at or after the last range asked about, from the first to the last byte
    /// marked as data in it: empty where none is.
    fn data_in(&mut self, range: Range<u64>) -> Range<u64> {
        self.skip_to(range.start);
        let mut marked = self.ranges[self.next..]
            .iter()
            .take_while(|data| data.start < range.end);
        let Some(first) = marked.next() else {
            return range.end..range.end;
        };
        let last = marked.last().unwrap_or(first);
        first.start.max(range.start)..last.end.min(range.end)
    }

    /// What of the hole `range`, at or after the last range asked about, is data: `None` where any
    /// whole block of it is, and otherwise the parts of the blocks it shares with the leaves beside
    /// it that are, under one block each and empty where none is. A hole that ends the file shares
    /// its last block with no leaf, so that block counts as whole. Where the sink's punch frees the
    /// whole blocks of every hole before the rename, on Apple platforms, data in them is no data
    /// of the target, and only the parts are returned.
    fn data_in_hole(&mut self, range: Range<u64>) -> Option<[Range<u64>; 2]> {
        let whole_start = range.start.div_ceil(self.block) * self.block;
        let whole_end = if range.end == self.len {
            range.end
        } else {
            range.end / self.block * self.block
        };
        let first_end = whole_start.min(range.end);
        let first = self.data_in(range.start..first_end);
        #[cfg(target_vendor = "apple")]
        let punches = self.punches;
        #[cfg(not(target_vendor = "apple"))]
        let punches = false;
        if whole_start < whole_end && !punches {
            self.skip_to(whole_start);
            if self
                .ranges
                .get(self.next)
                .is_some_and(|data| data.start < whole_end)
            {
                return None;
            }
        }
        let last = self.data_in(whole_end.max(first_end)..range.end);
        Some([first, last])
    }
}

/// Reads back `parts`, the parts of a hole of the content in `file`, a resumed temporary file, that
/// [`DataMap::data_in_hole`] found to be data in the blocks the hole shares with the leaves beside
/// it, each in one read of exactly its size, and fails with [`Unresumable`] where they are not all
/// zeros or cannot be read. A leaf's write allocates those blocks whole, and a fresh write leaves
/// the hole's bytes in them as zeros. A whole block of the hole that is data never comes here: it
/// fails the read, zeros or not, since zero is not the same as no data, or the sink's punch frees
/// it ([`DataMap::data_in_hole`]). Nowhere else do zeros stand for a hole: a leaf of zeros is never
/// held ([`holds_leaf`]). Holds `permit`, of the fetch pool's budget, until it ends.
///
/// The hole goes to the sink before this ends. That is safe: a check that fails makes the read
/// write the file afresh, emptying it first, and no leaf's write touches the hole's bytes, so the
/// reads race no write.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn check_hole_edges(
    file: IoFile,
    parts: [Range<u64>; 2],
    permit: SemaphorePermit<'static>,
) -> impl Future<Output = Result<(), StorageError>> {
    async move {
        for part in parts.iter().filter(|part| !part.is_empty()) {
            let held = file
                .read_exact_at((part.end - part.start) as usize, part.start)
                .await
                .map_err(|err| unresumable(Some(err)))?;
            if !held
                .chunks(ZEROS.len())
                .all(|chunk| ZEROS.starts_with(chunk))
            {
                return Err(unresumable(None));
            }
        }
        drop(permit);
        Ok(())
    }
}

/// Hands one leaf to the write sink: as held where `resumed`, the file the sink writes, already
/// holds it ([`holds_leaf`]), and loaded and checked for writing otherwise. `permit` covers the
/// leaf's memory, for the read of what `resumed` holds and for the load alike, and goes with the
/// bytes to be written. Holds `claim` until it ends, as [`walk_intermediate_level`]'s loads do,
/// since it runs on past a pipeline that is dropped.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::too_many_arguments, clippy::manual_async_fn)]
fn place_leaf(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    leaf: LeafReference,
    resumed: Option<IoFile>,
    sink: DataSender,
    permit: SemaphorePermit<'static>,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
    claim: Option<Arc<StoreGuard>>,
) -> impl Future<Output = Result<(), StorageError>> {
    async move {
        let _claim = claim;
        if let Some(file) = resumed
            && holds_leaf(&file, &leaf).await
        {
            drop(permit);
            let held = DataMessage::Held(leaf.target_offset as usize, leaf.expected_size);
            send_to_sink(&sink, held).await;
            return Ok(());
        }
        let subaddress = Address {
            context: leaf.context,
            hash: leaf.hash,
        };
        let (loaded_fragment, buffer) =
            load_fragment(store, partition, subaddress, options, remote_session).await?;
        let buffer = checked_leaf(&loaded_fragment, buffer, leaf.expected_size, &leaf.clip)?;
        let write = DataMessage::Payload(leaf.target_offset as usize, buffer, permit);
        send_to_sink(&sink, write).await;
        Ok(())
    }
}

/// The part `clip` of a leaf to write, from `buffer`, the payload of `loaded`, the fragment loaded
/// for a leaf whose parent list claims `expected_size` bytes, once the load is checked.
///
/// Tier check: the parent list decided this reference was a leaf by peeking at the first child. If
/// a peer mixed an intermediate fragment list into the same level, the "buffer" here is a list of
/// `FragmentReference`s, not content bytes — writing it at the leaf's offset would silently
/// corrupt the reassembled output.
///
/// Contiguity check: the chunk's actual content size must exactly match what the parent list's
/// offset delta claims. A mismatch means the reassembly would leave a gap or overlap; reject
/// rather than silently corrupt the output.
///
/// Narrowed only after the whole leaf has been loaded and checked. `slice` is a view onto the same
/// allocation, so a clipped leaf costs no copy.
fn checked_leaf(
    loaded: &Fragment,
    buffer: Bytes,
    expected_size: u64,
    clip: &Range<u64>,
) -> Result<Bytes, StorageError> {
    if loaded.flags & FragmentFlags::PayloadFragmented != 0 {
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
    Ok(buffer.slice(clip.start as usize..clip.end as usize))
}

/// Whether `file` already holds the whole of `leaf` at its target offset: the bytes there hash to
/// the leaf's address. A leaf the read clips is never held, since the file holds only part of it:
/// bytes there that hash to the whole leaf run on into the next one, as where both are zeros. Nor
/// is a leaf whose range the file cannot be read for. Nor is a leaf of zeros: a range no read wrote
/// reads as zeros too, and written zeros are data, so the leaf is written again.
async fn holds_leaf(file: &IoFile, leaf: &LeafReference) -> bool {
    if leaf.clip != (0..leaf.expected_size) {
        return false;
    }
    file.read_exact_at(leaf.expected_size as usize, leaf.target_offset)
        .await
        .is_ok_and(|held| {
            ZEROS.get(..held.len()) != Some(held.as_ref()) && hash::hash_slice(&held) == leaf.hash
        })
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
/// pipeline. A piece of a hole holds no budget, so it comes with no permit.
#[lore_macro::test_pub]
async fn send_payload<T>(
    sender: &Sender<Result<T, StorageError>>,
    leaf: T,
    permit: Option<SemaphorePermit<'static>>,
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

/// Hands `leaf`, a hole, to the write sink: its target offset, and the length of it the read asks
/// for. A send that fails is discarded, as [`send_to_sink`] discards one: a sink that has gone has
/// an error of its own to report.
async fn send_hole_to_sink(sender: &DataSender, leaf: &LeafReference) {
    if let Ok(slot) = sender.reserve().await {
        let length = leaf.clip.end - leaf.clip.start;
        slot.send(DataMessage::Hole(leaf.target_offset as usize, length));
    }
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
    claim: Option<Arc<StoreGuard>>,
) -> impl Future<Output = Result<(), StorageError>> {
    fetch_ordered_and_stream_from(
        fragment_limiter(),
        store,
        partition,
        leaf_rx,
        sender,
        options,
        remote_session,
        claim,
    )
}

/// [`fetch_ordered_and_stream`] with the fragment budget taken from `semaphore`.
#[lore_macro::test_pub]
#[allow(clippy::too_many_arguments)]
fn fetch_ordered_and_stream_from(
    semaphore: &'static Semaphore,
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    leaf_rx: Receiver<LeafReference>,
    sender: Sender<Result<Bytes, StorageError>>,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
    claim: Option<Arc<StoreGuard>>,
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
        claim,
        content_leaf,
        zero_piece,
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
    claim: Option<Arc<StoreGuard>>,
) -> impl Future<Output = Result<(), StorageError>> {
    fetch_ordered_from(
        fragment_limiter(),
        store,
        partition,
        leaf_rx,
        sender,
        options,
        remote_session,
        claim,
        whole_leaf,
        whole_hole,
    )
}

/// A hole as a leaf with its fragment: no payload, and a fragment whose `size_payload` is zero,
/// which no stored fragment has, says it is a hole of `size_content` bytes. Delivered whole.
#[lore_macro::test_pub]
fn whole_hole(len: u64) -> ((Fragment, Bytes), u64) {
    let fragment = Fragment {
        flags: 0,
        size_payload: 0,
        size_content: len,
    };
    ((fragment, Bytes::new()), len)
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

/// What the ordered fetch pool queues in content order: a fetch on its way, or a hole of the given
/// length, which needs none.
enum Queued<T> {
    Fetch(JoinHandle<FetchResult<T>>),
    Hole(u64),
}

/// The ordered fetch pool behind [`fetch_ordered_and_stream`]. Each leaf is loaded under
/// `options` and handed to `make_leaf` with the content size its parent list states and the part
/// of it that was asked for; what that returns is what the caller receives. A hole is fetched from
/// nowhere: `make_hole` gives the first piece of a hole of the length it is passed and the bytes
/// that piece covers, and the pieces are delivered in place of a leaf until the hole is covered.
///
/// The launcher and every fetch it spawns hold `claim`, for the reason [`walk_intermediate_level`]
/// gives for its loads.
#[allow(clippy::too_many_arguments)]
async fn fetch_ordered_from<T, F, H>(
    semaphore: &'static Semaphore,
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    mut leaf_rx: Receiver<LeafReference>,
    sender: Sender<Result<T, StorageError>>,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
    claim: Option<Arc<StoreGuard>>,
    make_leaf: F,
    make_hole: H,
) -> Result<(), StorageError>
where
    T: Send + 'static,
    F: Fn(Fragment, Bytes, u64, Range<u64>) -> Result<T, StorageError> + Copy + Send + 'static,
    H: Fn(u64) -> (T, u64),
{
    // Sized so it never binds before the budget does; every payload in it holds a permit.
    let max_tasks = fragment_budget_kib() / FRAGMENT_MINIMUM_COST_KIB as usize;
    let (fetch_queue_tx, mut fetch_queue_rx) = channel::<Queued<T>>(max_tasks);

    // Launcher: read leaf refs from walker, spawn fetch tasks, push handles
    let launcher: JoinHandle<Result<(), StorageError>> = {
        let store = store.clone();
        let remote_session = remote_session.clone();
        lore_base::lore_spawn!(async move {
            while let Some(leaf) = next_leaf(&mut leaf_rx, &fetch_queue_tx).await {
                if leaf.is_hole() {
                    let length = leaf.clip.end - leaf.clip.start;
                    if fetch_queue_tx.send(Queued::Hole(length)).await.is_err() {
                        break;
                    }
                    continue;
                }
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
                let claim = claim.clone();

                let handle: JoinHandle<FetchResult<T>> = lore_base::lore_spawn!(async move {
                    let _claim = claim;
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

                if fetch_queue_tx.send(Queued::Fetch(handle)).await.is_err() {
                    break;
                }
            }
            Ok(())
        })
    };

    // Consumer: await handles in FIFO order, send to caller's channel
    let mut result = Ok(());
    loop {
        let Some(queued) = fetch_queue_rx.recv().await else {
            break;
        };
        let handle = match queued {
            Queued::Fetch(handle) => handle,
            Queued::Hole(length) => {
                if send_hole(&sender, length, &make_hole).await {
                    break;
                }
                continue;
            }
        };
        match handle
            .await
            .map_err(|e| StorageError::internal_with_context(e, "load task join"))
            .and_then(|r| r)
        {
            Ok((leaf, permit)) => {
                if send_payload(&sender, leaf, Some(permit)).await {
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

/// Hands a hole of `length` bytes to the caller as the pieces `make_hole` cuts it into, reporting
/// whether the caller has gone, as [`send_payload`] does.
async fn send_hole<T>(
    sender: &Sender<Result<T, StorageError>>,
    mut length: u64,
    make_hole: &impl Fn(u64) -> (T, u64),
) -> bool {
    while length > 0 {
        let (piece, covered) = make_hole(length);
        if send_payload(sender, piece, None).await {
            return true;
        }
        length -= covered;
    }
    false
}

/// The outcome of a pipeline stage, with a task that did not run to completion counted as a
/// failure of the stage it was running.
fn joined(stage: Result<Result<(), StorageError>, JoinError>) -> Result<(), StorageError> {
    stage
        .map_err(|e| StorageError::internal_with_context(e, "task failure"))
        .and_then(|r| r)
}

/// Drains the fetch pool's [`DataMessage`]s, writing each payload at its offset and counting each
/// hole, and each range the file already holds, as written.
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
/// a zero-filled hole, indistinguishable from content. Every leaf of the whole file,
/// written, held or a hole, passes through here, which makes this the one place that can see
/// the total. The walker's tiling checks mean it should never fire, which is the point of
/// having it.
///
/// A hole is counted and not written: the file comes from [`open_file_write`], or is a resumed
/// file emptied before any write of its content, so the range is unallocated and reads as zeros.
/// A held range is counted and not written either: the file holds the leaf there already. Every
/// payload is written, one of zeros included: zeros are data. On Apple platforms, every hole is
/// deallocated once every payload is written ([`IoFile::punch_holes`]), as APFS allocates the small
/// holes beside data it writes out, and a resumed file may hold such zeros, or other data, in the
/// whole blocks of a hole; a range of data is never deallocated. The ranges of the holes are kept
/// for that, 16 bytes each, until the get ends.
#[lore_macro::test_pub]
async fn write_to_file(
    file: IoFile,
    size: usize,
    mut data_rx: DataReceiver,
) -> Result<(), StorageError> {
    let mut tasks: JoinSet<Result<(), StorageError>> = JoinSet::new();
    let mut result = Ok(());
    let mut written = 0usize;
    #[cfg(target_vendor = "apple")]
    let mut holes: Vec<std::ops::Range<u64>> = Vec::new();

    while let Some(message) = data_rx.recv().await {
        let (offset, length) = match &message {
            DataMessage::Payload(offset, payload, _) => (*offset, payload.len()),
            DataMessage::Hole(offset, length) | DataMessage::Held(offset, length) => {
                (*offset, usize::try_from(*length).unwrap_or(usize::MAX))
            }
        };
        let Some(end) = offset.checked_add(length) else {
            result = Err(StorageError::internal(
                "file write offset + data length overflows usize",
            ));
            break;
        };
        if end > size {
            result = Err(StorageError::internal(format!(
                "file write out of bounds: offset {offset} + {length} > {size}"
            )));
            break;
        }

        written += length;
        #[cfg(target_vendor = "apple")]
        if let DataMessage::Hole(..) = message {
            holes.push(offset as u64..end as u64);
        }

        let DataMessage::Payload(offset, payload, permit) = message else {
            continue;
        };
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
    #[cfg(target_vendor = "apple")]
    if result.is_ok() && !holes.is_empty() {
        result = file
            .punch_holes(holes)
            .await
            .map_err(|e| StorageError::internal_with_context(e, "punch the holes of the file"));
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
///
/// `claim` is the caller's claim on `store`, from
/// [`claim_for_task`](crate::immutable_store::claim_for_task), which every task the pipeline
/// spawns that touches `store` holds until it ends.
///
/// Dropped, the pipeline detaches its tasks, which run on to their end: the walk and the fetch
/// pool drain the loads they started, and the write sink writes what they deliver, as into a
/// temporary file its caller has unlinked. Into the file of a read that resumes
/// ([`ReadOptions::resume_write`]), whose temporary file is kept, the fetch pool and the write sink
/// are aborted instead, so that no task writes to the file past the read and the lock the read
/// holds on it ([`crate::resume`]) is let go once the operations in flight on it end; the fetches
/// in flight are detached and finish their requests, each under `claim`.
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
    claim: Option<Arc<StoreGuard>>,
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
        claim.clone(),
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
                claim,
            ));

            let (walk_result, fetch_result) = tokio::join!(walker, fetcher);
            joined(walk_result).and(joined(fetch_result))
        }
        DefragmentSink::File { file, size } => {
            let (fetcher, writer) = spawn_fetch_and_write(
                store.clone(),
                partition,
                leaf_rx,
                file,
                size,
                options,
                remote_session.clone(),
                claim,
            );
            let (walk_result, fetch_result, write_result) = if options.resume_write {
                tokio::join!(
                    walker,
                    AbortOnDropHandle::new(fetcher),
                    AbortOnDropHandle::new(writer)
                )
            } else {
                tokio::join!(walker, fetcher, writer)
            };
            joined(walk_result)
                .and(joined(fetch_result))
                .and(joined(write_result))
        }
    }
}

/// A spawned task of a [`defragment_pipeline`].
type PipelineTask = JoinHandle<Result<(), StorageError>>;

/// Spawns the fetch pool and the write sink of a [`defragment_pipeline`] into `file`, of `size`
/// bytes: the pool loads each leaf `leaf_rx` yields, and the sink writes it. For a read that
/// resumes ([`ReadOptions::resume_write`]) the pool is [`fetch_resuming`], which takes a leaf from
/// `file` where `file` holds it ([`ReadOptions::resume_held`]); for any other, [`fetch_unordered`].
///
/// A function of its own because it clones `file`, and a local the pipeline's future borrows is
/// held there until its scope ends, across the wait for the tasks.
#[allow(clippy::too_many_arguments)]
fn spawn_fetch_and_write(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    leaf_rx: Receiver<LeafReference>,
    file: IoFile,
    size: usize,
    options: ReadOptions,
    remote_session: Option<Arc<StorageSession>>,
    claim: Option<Arc<StoreGuard>>,
) -> (PipelineTask, PipelineTask) {
    let (data_tx, data_rx) = channel::<DataMessage>(PIPELINE_DATA_CHANNEL_SIZE);
    let fetcher = if options.resume_write {
        let resumed = options.resume_held.then(|| file.clone());
        lore_base::lore_spawn!(fetch_resuming(
            store,
            partition,
            leaf_rx,
            data_tx,
            options,
            remote_session,
            resumed,
            claim,
        ))
    } else {
        lore_base::lore_spawn!(fetch_unordered(
            store,
            partition,
            leaf_rx,
            data_tx,
            options,
            remote_session,
            claim,
        ))
    };
    let writer = lore_base::lore_spawn!(write_to_file(file, size, data_rx));
    (fetcher, writer)
}

/// [`defragment_pipeline`] delivering a whole content's leaves in content order on `sender`, each
/// with its fragment and loaded as `options` asks.
///
/// Not a [`DefragmentSink`] variant, which would grow every future holding a sink. `claim` as
/// [`defragment_pipeline`] takes it.
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
    claim: Option<Arc<StoreGuard>>,
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
        claim.clone(),
    ));
    let fetcher = lore_base::lore_spawn!(fetch_ordered_leaves(
        store,
        partition,
        leaf_rx,
        sender,
        options,
        remote_session,
        claim,
    ));

    let (walk_result, fetch_result) = tokio::join!(walker, fetcher);
    joined(walk_result).and(joined(fetch_result))
}

#[allow(clippy::too_many_arguments)]
pub async fn read_defragment(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    range: Range<usize>,
    fragment: Fragment,
    source_buffer: Bytes,
    mut target: crate::CallerBuffer,
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

    let level_end = fragment_list[0]
        .offset_content
        .checked_add(fragment.size_content)
        .ok_or_else(|| StorageError::internal("fragment list content window overflows"))?;
    let window = offset as u64..(offset + range.len()) as u64;
    let scan = scan_entries(fragment_list, level_end, &window)?;
    if scan.holes {
        refuse_entries_larger_than_a_leaf(fragment_list, level_end)?;
    }
    let wanted = scan.wanted.unwrap_or(0..0);
    let (fragment_begin, fragment_end) = (wanted.start, wanted.end);

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
        let mut subtarget = target.split_off(split_point);
        if fragment_list[fragment_index].is_hole() {
            subtarget.as_mut_slice().fill(0);
            continue;
        }
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
                scan.holes,
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
    if result.is_ok() && target_end != 0 {
        result = Err(StorageError::internal(format!(
            "Defragmenting malformed fragment list, {target_end} bytes of the range are in no entry"
        )));
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
/// content its address names; the leaf is not the size the list claimed for it; or the read
/// declined, which the assembling path answers by reaching the remote and healing. With `leaf`
/// set, an address naming a fragment list is refused, as [`refuse_list`] says.
#[allow(clippy::too_many_arguments)]
async fn place_whole_leaf(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    range: &Range<usize>,
    target: &mut crate::CallerBuffer,
    options: ReadOptions,
    depth: usize,
    remote_session: Option<Arc<StorageSession>>,
    leaf: bool,
) -> Result<bool, StorageError> {
    if range.start != 0 {
        return Ok(false);
    }

    let piece = target.len();
    match crate::read::read_content_into_buffer(
        store,
        partition,
        address,
        target,
        options,
        crate::read::RootSource::LocalOrRemote,
        depth,
        remote_session,
    )
    .await
    {
        Ok(Some((fragment, written))) => {
            refuse_list(leaf, &fragment)?;
            Ok(written == piece)
        }
        Ok(None) => Ok(false),
        Err(err) if err.is_oversized() => Ok(false),
        Err(err) => Err(err),
    }
}

/// Reads `range` of the content at `address` into `target`. With `leaf` set, as for an entry of a
/// list holding a hole, the address must name a leaf, as [`refuse_list`] says.
#[allow(clippy::too_many_arguments)]
fn read_defragment_subread(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    range: Range<usize>,
    mut target: crate::CallerBuffer,
    options: ReadOptions,
    depth: usize,
    remote_session: Option<Arc<StorageSession>>,
    leaf: bool,
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
            leaf,
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

        refuse_list(leaf, &fragment)?;
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

/// Refuses an entry of `fragment_list`, whose level ends at `level_end`, that is not a hole and
/// spans more than a fragment: a list holding a hole is a list of leaves, as every walk of one
/// checks before it reads an entry.
fn refuse_entries_larger_than_a_leaf(
    fragment_list: &[FragmentReference],
    level_end: u64,
) -> Result<(), StorageError> {
    for index in 0..fragment_list.len() {
        let entry = entry_window(fragment_list, index, level_end)?;
        let size = entry.end - entry.start;
        if size > crate::FRAGMENT_SIZE_THRESHOLD as u64 && !fragment_list[index].is_hole() {
            return Err(StorageError::internal(format!(
                "fragment list entry {index} beside a hole spans {size} bytes, more than a fragment"
            )));
        }
    }
    Ok(())
}

/// Refuses `fragment` where `leaf` is set and it is a fragment list: an entry of a list holding a
/// hole names a leaf.
fn refuse_list(leaf: bool, fragment: &Fragment) -> Result<(), StorageError> {
    if leaf
        && (fragment.flags & FragmentFlags::PayloadFragmented) == FragmentFlags::PayloadFragmented
    {
        return Err(StorageError::internal(
            "expected leaf fragment but the list holding a hole names a fragment list",
        ));
    }
    Ok(())
}

/// Opens a file for positional writing, emptied, and sizes it to the whole content up front.
///
/// The handle is shared: positional writes carry their own offset, so concurrent writers to
/// disjoint ranges need no exclusion. Clones of the returned handle share it.
///
/// Emptied first, so every range starts unallocated and reads as zeros, whatever the file held:
/// a range left unwritten, as a hole is, holds none of a temporary file an interrupted read left
/// behind, nor of the target a direct write replaces. Then sized rather than left empty, made
/// sparse first where `sparse` ([`IoFile::set_len_sparse`]), so a range no payload covers reads as
/// zeros instead of shortening the file — which is what the sink's byte-count check exists to
/// catch — and is a hole.
///
/// On Windows a file written sparse is emptied by [`IoFile::empty`], which deallocates it whole
/// before it truncates it: measured on version 3.14 of the Resilient File System, a sparse file
/// truncated before its data is written out and grown again records the ranges it held past the
/// cut as allocated, so the holes of the content written there would be allocated zeros. Every
/// other file is truncated as it is opened, which frees its storage on the other platforms.
pub async fn open_file_write(
    path: impl AsRef<Path>,
    size: usize,
    sparse: bool,
) -> Result<IoFile, std::io::Error> {
    let deallocate = sparse && cfg!(target_family = "windows");
    let file = IoDriver::global()
        .open(
            path,
            &OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(!deallocate),
        )
        .await?;
    if deallocate {
        file.empty(size as u64).await?;
    } else if sparse {
        file.set_len_sparse(size as u64).await?;
    } else {
        file.set_len(size as u64, ()).await?;
    }
    Ok(file)
}
