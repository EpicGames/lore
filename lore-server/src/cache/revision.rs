// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Persistent cache of revision-list segments keyed at step boundaries.
//!
//! Each cached entry at boundary `B = N * step_size` holds the parent-chain
//! revisions whose number falls in `(B - step_size, B]` — up to `step_size`
//! items, top-inclusive at `B`, bottom-exclusive at `B - step_size`. An entry
//! is only written once segment `B` is closed (branch latest has reached past
//! `B`). Empty segments are not written.
//!
//! Storage layout mirrors the link-list pattern in `lore_revision::state`:
//! items are serialized to bytes, written to the immutable store, and the
//! resulting hash is stored in the mutable store under a
//! `revision_list_step_key`.
//!
//! All operations are best-effort: any failure aborts the cache write or
//! returns `None`, so reads always have a correct fallback path.

use std::sync::Arc;

use bytes::Bytes;
use bytes::BytesMut;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::typed_bytes::TypedBytes;
use lore_revision::branch;
use lore_revision::immutable;
use lore_revision::lore::BranchId;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use tracing::debug;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

use crate::grpc::get_write_token;

/// Header size in bytes — written at offset 0 of every cached blob.
const HEADER_SIZE: usize = std::mem::size_of::<branch::CachedRevisionListHeader>();
/// Item size in bytes — packed contiguously after the header.
const ITEM_SIZE: usize = std::mem::size_of::<branch::CachedRevisionItem>();

/// Result of a parent-chain walk used to populate the list cache.
pub(crate) struct SegmentWalk {
    /// Items in walk order (highest revision number first).
    pub items: Vec<branch::CachedRevisionItem>,
    /// True if the walk reached the configured lower threshold (saw a rev
    /// with number `<= stop_below`) or hit the root sentinel. Either case
    /// confirms the lowest segment touched by the walk is fully traversed.
    pub reached_terminator: bool,
}

/// Parsed view over a cached revision-list blob. Holds the items as a
/// `Bytes` slice into the original buffer; `items()` reinterprets that
/// slice as `&[CachedRevisionItem]` without copying. The header has
/// already been validated when the value exists.
pub(crate) struct CachedRevisionList {
    items_bytes: Bytes,
}

impl CachedRevisionList {
    /// Validate `[CachedRevisionListHeader | CachedRevisionItem...]`
    /// layout and return a view over the items, or `None` on any
    /// mismatch (length not aligned, bad magic, wrong version).
    fn from_blob(blob: Bytes) -> Option<Self> {
        if blob.len() < HEADER_SIZE || !(blob.len() - HEADER_SIZE).is_multiple_of(ITEM_SIZE) {
            debug!(
                blob_len = blob.len(),
                header_size = HEADER_SIZE,
                item_size = ITEM_SIZE,
                "Discarding revision list cache entry with mismatched blob length",
            );
            return None;
        }
        let header =
            branch::CachedRevisionListHeader::read_from_bytes(&blob.as_ref()[..HEADER_SIZE])
                .ok()?;
        if header.magic != branch::CACHED_REVISION_LIST_MAGIC
            || header.version != branch::CACHED_REVISION_LIST_VERSION
        {
            debug!(
                magic = format_args!("{:#010x}", header.magic),
                version = header.version,
                expected_magic = format_args!("{:#010x}", branch::CACHED_REVISION_LIST_MAGIC),
                expected_version = branch::CACHED_REVISION_LIST_VERSION,
                "Discarding revision list cache entry with mismatched header",
            );
            return None;
        }
        let items_bytes = blob.slice(HEADER_SIZE..);
        Some(Self { items_bytes })
    }

    /// Zero-copy view of the cached items. The slice borrows from the
    /// underlying `Bytes` retained by `self`.
    pub fn items(&self) -> &[branch::CachedRevisionItem] {
        self.items_bytes
            .as_type_slice::<branch::CachedRevisionItem>()
    }
}

/// Load the cached list at the boundary containing `revision_number`.
/// Returns `None` on any error or missing/invalid data. The returned
/// list lets callers iterate items in place without copying.
pub(crate) async fn load_cached_list(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    revision_number: u64,
    step_size: u64,
) -> Option<CachedRevisionList> {
    let (key, key_type) = branch::revision_list_step_key(
        repository::SALT_LORE,
        repository.id,
        branch,
        revision_number,
        step_size,
    );

    let blob_hash = repository
        .clone()
        .read_mutable_store()
        .load(repository.id, key, key_type)
        .await
        .ok()?;

    if blob_hash.is_zero() {
        return None;
    }

    let bytes = immutable::read(
        repository.clone(),
        Address::zero_context_hash(blob_hash),
        None,
        immutable::read_options_from_repository(repository).with_cache(),
    )
    .await
    .ok()?
    .to_aligned::<branch::CachedRevisionItem>();

    CachedRevisionList::from_blob(bytes)
}

/// If segment B containing `revision_number` is closed (proven by the
/// skip pointer at B + `step_size`), walk `parent_self` from that anchor to
/// populate `List_B`. Returns the cached segment items on success.
pub(crate) async fn try_backfill_segment(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    revision_number: u64,
    step_size: u64,
) -> Option<CachedRevisionList> {
    let target_b = revision_number.div_ceil(step_size) * step_size;
    let next_b = target_b.checked_add(step_size)?;

    let (next_key, next_key_type) = branch::revision_step_key(
        repository::SALT_LORE,
        repository.id,
        branch,
        next_b,
        step_size,
    );
    let anchor = repository
        .clone()
        .read_mutable_store()
        .load(repository.id, next_key, next_key_type)
        .await
        .ok()?;
    if anchor.is_zero() {
        return None;
    }

    let stop_below = target_b.saturating_sub(step_size);
    let max_items = (step_size as usize).saturating_mul(2).saturating_add(2);
    let walk = walk_segment_revisions(repository, anchor, stop_below, max_items).await;
    if !walk.reached_terminator {
        return None;
    }

    let segments = partition_into_segments(&walk.items, step_size);
    for (segment_b, list) in &segments {
        if *segment_b == target_b {
            store_cached_list(repository, branch, *segment_b, step_size, list).await;
        }
    }

    load_cached_list(repository, branch, revision_number, step_size).await
}

/// Store the cached list at the boundary containing `revision_number`.
/// Skips empty lists (per the "don't write empty segments" invariant).
/// Errors are silently ignored — the cache is best-effort.
pub(crate) async fn store_cached_list(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    revision_number: u64,
    step_size: u64,
    items: &[branch::CachedRevisionItem],
) {
    if items.is_empty() {
        return;
    }

    let header = branch::CachedRevisionListHeader {
        magic: branch::CACHED_REVISION_LIST_MAGIC,
        version: branch::CACHED_REVISION_LIST_VERSION,
    };
    let items_bytes = items.as_bytes();
    let mut buffer = BytesMut::with_capacity(HEADER_SIZE + items_bytes.len());
    buffer.extend_from_slice(header.as_bytes());
    buffer.extend_from_slice(items_bytes);
    let buffer = buffer.freeze();

    let Ok(address) = immutable::write(
        repository.clone(),
        Context::default(),
        buffer,
        immutable::write_options_from_repository(repository.clone()),
    )
    .await
    else {
        return;
    };

    let (key, key_type) = branch::revision_list_step_key(
        repository::SALT_LORE,
        repository.id,
        branch,
        revision_number,
        step_size,
    );
    let write_token = get_write_token();
    if repository
        .clone()
        .write_mutable_store(&write_token)
        .store(repository.id, key, address.hash, key_type)
        .await
        .is_ok()
    {
        debug!(
            number = revision_number,
            count = items.len(),
            key = %key,
            "Stored revision list cache entry"
        );
    }
}

/// Walk `parent_self` from `anchor_hash`, pushing each visited revision in
/// walk order. Stops when (a) a revision with number `<= stop_below` is
/// pushed, (b) the parent chain reaches the root (zero hash), (c) the walk
/// exceeds `max_items`, or (d) a state deserialization fails. Cases (a) and
/// (b) set `reached_terminator = true`, signalling that the lowest segment
/// touched is fully traversed.
pub(crate) async fn walk_segment_revisions(
    repository: &Arc<RepositoryContext>,
    anchor_hash: Hash,
    stop_below: u64,
    max_items: usize,
) -> SegmentWalk {
    let mut items: Vec<branch::CachedRevisionItem> = Vec::new();
    let mut hash = anchor_hash;
    let mut reached_terminator = false;

    while items.len() < max_items {
        if hash.is_zero() {
            reached_terminator = true;
            break;
        }
        let Ok(state) = State::deserialize(repository.clone(), hash).await else {
            break;
        };
        let number = state.revision_number();
        items.push(branch::CachedRevisionItem {
            number,
            signature: hash,
            metadata: state.metadata_hash(),
            state: state.state_data(),
        });
        if number <= stop_below {
            reached_terminator = true;
            break;
        }
        hash = state.parent_self();
    }

    SegmentWalk {
        items,
        reached_terminator,
    }
}

/// Partition a contiguous walk of items (highest number first) into per-segment
/// lists keyed by their step-aligned upper boundary `B`. Returned in walk
/// order — highest boundary first. Includes empty boundary entries only if
/// items genuinely belong to them; this function makes no judgement about
/// whether a segment is "fully traversed" — the caller must filter using the
/// `reached_terminator` signal from `walk_segment_revisions`.
pub(crate) fn partition_into_segments(
    items: &[branch::CachedRevisionItem],
    step_size: u64,
) -> Vec<(u64, Vec<branch::CachedRevisionItem>)> {
    if items.is_empty() {
        return Vec::new();
    }
    let mut result: Vec<(u64, Vec<branch::CachedRevisionItem>)> = Vec::new();
    let mut current: Option<(u64, Vec<branch::CachedRevisionItem>)> = None;

    for item in items {
        let b = item.number.div_ceil(step_size) * step_size;
        match current.as_mut() {
            Some((existing_b, list)) if *existing_b == b => list.push(*item),
            _ => {
                if let Some(prev) = current.take() {
                    result.push(prev);
                }
                current = Some((b, vec![*item]));
            }
        }
    }
    if let Some(prev) = current {
        result.push(prev);
    }
    result
}

#[cfg(test)]
mod tests {
    use lore_base::types::Hash;
    use lore_revision::branch::CachedRevisionItem;
    use lore_revision::branch::CachedRevisionListHeader;
    use lore_revision::state::StateData;

    use super::*;

    fn item(number: u64) -> CachedRevisionItem {
        CachedRevisionItem {
            number,
            signature: Hash::default(),
            metadata: Hash::default(),
            state: StateData::default(),
        }
    }

    #[test]
    fn partition_empty_input_returns_empty() {
        assert!(partition_into_segments(&[], 100).is_empty());
    }

    #[test]
    fn partition_single_segment() {
        // Items 200..101 all live in segment 200 (div_ceil(N, 100) * 100).
        let items: Vec<_> = (101..=200).rev().map(item).collect();
        let segments = partition_into_segments(&items, 100);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].0, 200);
        assert_eq!(segments[0].1.len(), 100);
        assert_eq!(segments[0].1[0].number, 200);
        assert_eq!(segments[0].1[99].number, 101);
    }

    #[test]
    fn partition_splits_at_segment_boundary() {
        // 101 lives in segment 200, 100 lives in segment 100, 1 in segment 100.
        let items: Vec<_> = [101, 100, 1].iter().map(|&n| item(n)).collect();
        let segments = partition_into_segments(&items, 100);
        assert_eq!(segments.len(), 2);
        // Walk order: highest segment first.
        assert_eq!(segments[0].0, 200);
        assert_eq!(segments[0].1.len(), 1);
        assert_eq!(segments[0].1[0].number, 101);
        assert_eq!(segments[1].0, 100);
        assert_eq!(segments[1].1.len(), 2);
        assert_eq!(segments[1].1[0].number, 100);
        assert_eq!(segments[1].1[1].number, 1);
    }

    #[test]
    fn partition_single_item_at_segment_top() {
        // Revision 100 sits at the top of segment 100, not segment 200.
        let segments = partition_into_segments(&[item(100)], 100);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].0, 100);
    }

    #[test]
    fn partition_handles_run_of_segments() {
        // Span four segments worth of items in walk order.
        let items: Vec<_> = (1..=350).rev().map(item).collect();
        let segments = partition_into_segments(&items, 100);
        // Segments: 400 (350..301), 300 (300..201), 200 (200..101), 100 (100..1).
        assert_eq!(segments.len(), 4);
        assert_eq!(segments[0].0, 400);
        assert_eq!(segments[0].1.len(), 50);
        assert_eq!(segments[1].0, 300);
        assert_eq!(segments[1].1.len(), 100);
        assert_eq!(segments[2].0, 200);
        assert_eq!(segments[2].1.len(), 100);
        assert_eq!(segments[3].0, 100);
        assert_eq!(segments[3].1.len(), 100);
    }

    /// Sanity check: the on-disk struct sizes don't accidentally
    /// change. Any field/layout change must also bump
    /// `CACHED_REVISION_LIST_VERSION` and update these numbers.
    #[test]
    fn cached_revision_item_size_is_stable() {
        assert_eq!(std::mem::size_of::<CachedRevisionListHeader>(), 8);
        assert_eq!(std::mem::align_of::<CachedRevisionListHeader>(), 4);
        assert_eq!(std::mem::size_of::<CachedRevisionItem>(), 392);
        assert_eq!(std::mem::align_of::<CachedRevisionItem>(), 8);
    }

    /// Header offset is item-aligned (8): items at offset
    /// `HEADER_SIZE = 8` end up properly aligned for the
    /// `as_type_slice::<CachedRevisionItem>` view in `items()`.
    #[test]
    fn header_size_preserves_item_alignment() {
        assert_eq!(HEADER_SIZE % std::mem::align_of::<CachedRevisionItem>(), 0);
    }
}
