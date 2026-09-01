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
use lore_storage::StoreError;
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

/// Determine which segment boundaries are *newly closed* by this transition.
/// A boundary `B` (multiple of `history_step_size`) is newly closed iff
/// `older_revision_number <= B < newer_revision_number`.
pub fn sealed_boundaries(
    older_revision_number: u64,
    newer_revision_number: u64,
    history_step_size: u64,
) -> Option<(u64, u64)> {
    debug_assert!(older_revision_number <= newer_revision_number);

    let lowest_b = older_revision_number.div_ceil(history_step_size) * history_step_size;

    let highest_b = if newer_revision_number > 0 {
        ((newer_revision_number - 1) / history_step_size) * history_step_size
    } else {
        return None;
    };

    if lowest_b == 0 || lowest_b > highest_b {
        return None;
    }

    Some((lowest_b, highest_b))
}

/// A branch push with long feature branches could increase the linear revision history
/// number beyond several boundaries. Each boundary should be sealed and point to the
/// last valid revision less than that boundary
pub async fn seal_boundary_revision_number(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    history_step_size: u64,
    boundary_revision_number: u64,
    older_state: &Arc<State>,
    newer_state: &Arc<State>,
) -> Result<(), StoreError> {
    let revision_to_point_to = if newer_state.revision_number() <= boundary_revision_number {
        newer_state.revision()
    } else {
        debug_assert!(older_state.revision_number() <= boundary_revision_number);
        older_state.revision()
    };

    let (key, key_type) = branch::revision_step_key(
        repository::SALT_LORE,
        repository.id,
        branch,
        boundary_revision_number,
        history_step_size,
    );
    let write_token = get_write_token();
    repository
        .write_mutable_store(&write_token)
        .store(repository.id, key, revision_to_point_to, key_type)
        .await
}

/// Store the history-step skip pointer (if a boundary was crossed) and any
/// revision-list cache entries for segments newly closed by this push.
///
/// A segment `B` (= `N * history_step_size`) is *closed* by this push iff
/// `parent_revision_number <= B < revision_number`. A single push can close
/// multiple segments (e.g. a merge that jumps past several boundaries). For
/// each closed segment we walk `parent_self` from `state` and persist the
/// items whose number falls in `(B - step, B]`.
///
/// Errors are ignored — this is purely an acceleration construct and will be
/// recreated on the next lookup if any step fails.
pub async fn store_history_step(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
    older_state: Arc<State>,
    newer_state: Arc<State>,
) {
    let Some((lowest_b, highest_b)) = sealed_boundaries(
        older_state.revision_number(),
        newer_state.revision_number(),
        history_step_size,
    ) else {
        return;
    };

    if acceleration.step_keys {
        for boundary in (lowest_b..=highest_b).step_by(history_step_size as usize) {
            let _ = seal_boundary_revision_number(
                repository.clone(),
                branch,
                history_step_size,
                boundary,
                &older_state,
                &newer_state,
            )
            .await;
        }
    }

    if !acceleration.list_cache {
        return;
    }

    // Walk parent chain from the new revision until we cross below the lowest
    // closed segment, capturing items for each closed boundary.
    let stop_below = lowest_b.saturating_sub(history_step_size);
    let span_segments = (highest_b.saturating_sub(lowest_b) / history_step_size) + 1;
    let max_items = (span_segments as usize)
        .saturating_mul(history_step_size as usize)
        // Allow a small overshoot so partial segments above the closed range
        // (the still-open one containing N) and the one terminator item can
        // still be walked.
        .saturating_add(history_step_size as usize)
        .saturating_add(1);

    let walk =
        walk_segment_revisions(&repository, newer_state.revision(), stop_below, max_items).await;

    if !walk.reached_terminator {
        // Walk was bounded by max_items; the last segment may be partial.
        // Skip cache writes — next reader will rebuild them via backfill.
        return;
    }

    let segments = partition_into_segments(&walk.items, history_step_size);
    for (segment_b, list) in segments {
        if segment_b >= lowest_b && segment_b <= highest_b {
            store_cached_list(&repository, branch, segment_b, history_step_size, &list).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Hash;
    use lore_revision::branch::CachedRevisionItem;
    use lore_revision::branch::CachedRevisionListHeader;
    use lore_revision::lore::RepositoryId;
    use lore_revision::state::StateData;
    use rand::random;

    use super::*;
    use crate::grpc::server::RevisionListAcceleration;
    use crate::store::test_store_create;

    const STEP_ONE_HUNDRED: u64 = 100;

    fn item(number: u64) -> CachedRevisionItem {
        CachedRevisionItem {
            number,
            signature: Hash::default(),
            metadata: Hash::default(),
            state: StateData::default(),
        }
    }

    fn test_repository(
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
    ) -> Arc<RepositoryContext> {
        Arc::new(RepositoryContext::new_server_context(
            immutable_store,
            mutable_store,
            random::<RepositoryId>(),
        ))
    }

    /// Serialize a linear `parent_self` chain numbered `1..=count`.
    /// Signatures are oldest-first, so index `n - 1` holds revision `n`.
    async fn serialize_linear_chain(repository: &Arc<RepositoryContext>, count: u64) -> Vec<Hash> {
        let write_token = get_write_token();
        let mut parent = Hash::default();
        let mut signatures = Vec::with_capacity(count as usize);
        for number in 1..=count {
            let state = Arc::new(State::new());
            state.set_parent_self(parent);
            state.set_revision_number(number);
            parent = state
                .serialize(repository.clone(), &write_token)
                .await
                .expect("serialize state");
            signatures.push(parent);
        }
        signatures
    }

    /// Serialize a revision whose number jumps past its parent's, as a merge
    /// against a higher-numbered branch produces.
    async fn serialize_jump_revision(
        repository: &Arc<RepositoryContext>,
        parent: Hash,
        revision_number: u64,
    ) -> Arc<State> {
        let write_token = get_write_token();
        let state = Arc::new(State::new());
        state.set_parent_self(parent);
        state.set_revision_number(revision_number);
        state
            .serialize(repository.clone(), &write_token)
            .await
            .expect("serialize jump state");
        state
    }

    async fn load_state(repository: &Arc<RepositoryContext>, revision: Hash) -> Arc<State> {
        State::deserialize(repository.clone(), revision)
            .await
            .expect("deserialize state")
    }

    /// Read the revision sealed at `boundary`, or `None` when unsealed.
    async fn load_step_key(
        repository: &Arc<RepositoryContext>,
        branch: BranchId,
        boundary: u64,
    ) -> Option<Hash> {
        let (key, key_type) = branch::revision_step_key(
            repository::SALT_LORE,
            repository.id,
            branch,
            boundary,
            STEP_ONE_HUNDRED,
        );
        repository
            .clone()
            .read_mutable_store()
            .load(repository.id, key, key_type)
            .await
            .ok()
            .filter(|revision| !revision.is_zero())
    }

    mod partition_into_segments {
        use super::*;

        #[test]
        fn empty_input_returns_empty() {
            assert!(partition_into_segments(&[], 100).is_empty());
        }

        #[test]
        fn single_segment() {
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
        fn splits_at_segment_boundary() {
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
        fn single_item_at_segment_top() {
            // Revision 100 sits at the top of segment 100, not segment 200.
            let segments = partition_into_segments(&[item(100)], 100);
            assert_eq!(segments.len(), 1);
            assert_eq!(segments[0].0, 100);
        }

        #[test]
        fn handles_run_of_segments() {
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
    }

    mod sealed_boundaries {
        use super::*;

        /// The boundaries a caller actually seals: multiples of the step
        /// size across the returned inclusive range.
        fn boundaries(older: u64, newer: u64, step_size: u64) -> Vec<u64> {
            match super::super::sealed_boundaries(older, newer, step_size) {
                Some((lowest_b, highest_b)) => {
                    (lowest_b..=highest_b).step_by(step_size as usize).collect()
                }
                None => Vec::new(),
            }
        }

        #[test]
        fn no_boundary_between_consecutive_revisions_inside_a_segment() {
            assert_eq!(sealed_boundaries(101, 102, STEP_ONE_HUNDRED), None);
        }

        #[test]
        fn landing_exactly_on_a_boundary_does_not_seal_it() {
            // The segment holding the branch head is always the open one, so
            // pushing revision 100 leaves boundary 100 unsealed until the
            // next push moves past it.
            assert_eq!(sealed_boundaries(99, 100, STEP_ONE_HUNDRED), None);
        }

        #[test]
        fn boundary_is_sealed_once_the_head_moves_past_it() {
            assert_eq!(
                sealed_boundaries(100, 101, STEP_ONE_HUNDRED),
                Some((100, 100))
            );
        }

        #[test]
        fn jump_over_a_boundary_seals_the_boundary_it_crossed() {
            // The boundary sealed is the one between the two revisions, not
            // the boundary of the segment the new revision lands in.
            assert_eq!(boundaries(99, 105, STEP_ONE_HUNDRED), vec![100]);
        }

        #[test]
        fn jump_seals_every_boundary_it_crossed() {
            assert_eq!(boundaries(150, 400, STEP_ONE_HUNDRED), vec![200, 300]);
        }

        #[test]
        fn large_jump_seals_one_boundary_per_step_not_per_revision() {
            // A jump closes one boundary per step, not one per revision
            // number it spans.
            let sealed = boundaries(150, 100_000, STEP_ONE_HUNDRED);
            assert_eq!(sealed.len(), 998);
            assert_eq!(sealed.first(), Some(&200));
            assert_eq!(sealed.last(), Some(&99_900));
        }

        #[test]
        fn first_revision_seals_nothing() {
            assert_eq!(sealed_boundaries(0, 1, STEP_ONE_HUNDRED), None);
        }

        #[test]
        fn unchanged_revision_number_seals_nothing() {
            assert_eq!(sealed_boundaries(200, 200, STEP_ONE_HUNDRED), None);
        }

        #[test]
        fn zero_target_seals_nothing() {
            assert_eq!(sealed_boundaries(0, 0, STEP_ONE_HUNDRED), None);
        }

        #[test]
        fn honours_a_non_default_step_size() {
            assert_eq!(boundaries(150, 400, 50), vec![150, 200, 250, 300, 350]);
            assert_eq!(boundaries(1, 4, 1), vec![1, 2, 3]);
        }
    }

    mod seal_boundary_revision_number {
        use super::*;

        #[tokio::test]
        async fn seals_boundary_with_the_parent_revision() {
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("create stores");

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(immutable_store, mutable_store);
                let branch = BranchId::from(uuid::Uuid::now_v7());

                let chain = serialize_linear_chain(&repository, 150).await;
                let older = load_state(&repository, chain[149]).await;
                let newer = serialize_jump_revision(&repository, chain[149], 400).await;

                seal_boundary_revision_number(
                    repository.clone(),
                    branch,
                    STEP_ONE_HUNDRED,
                    200,
                    &older,
                    &newer,
                )
                .await
                .expect("seal boundary");

                // Boundary 200 holds the highest revision numbered <= 200,
                // which across the jump is the parent at 150.
                assert_eq!(
                    load_step_key(&repository, branch, 200).await,
                    Some(chain[149])
                );
                // Exactly one boundary is sealed per call: neighbours, and in
                // particular the ceil-space bucket of the new revision number,
                // must be left untouched.
                assert_eq!(load_step_key(&repository, branch, 100).await, None);
                assert_eq!(load_step_key(&repository, branch, 300).await, None);
                assert_eq!(load_step_key(&repository, branch, 400).await, None);
            }))
            .await;
        }

        #[tokio::test]
        async fn seals_boundary_at_or_above_the_new_revision_with_that_revision() {
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("create stores");

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(immutable_store, mutable_store);
                let branch = BranchId::from(uuid::Uuid::now_v7());

                let chain = serialize_linear_chain(&repository, 150).await;
                let older = load_state(&repository, chain[149]).await;
                let newer = serialize_jump_revision(&repository, chain[149], 400).await;

                seal_boundary_revision_number(
                    repository.clone(),
                    branch,
                    STEP_ONE_HUNDRED,
                    400,
                    &older,
                    &newer,
                )
                .await
                .expect("seal boundary");

                assert_eq!(
                    load_step_key(&repository, branch, 400).await,
                    Some(newer.revision())
                );
                assert_eq!(load_step_key(&repository, branch, 200).await, None);
                assert_eq!(load_step_key(&repository, branch, 300).await, None);
                assert_eq!(load_step_key(&repository, branch, 500).await, None);
            }))
            .await;
        }
    }

    mod store_history_step {
        use super::*;

        #[tokio::test]
        async fn seals_every_boundary_crossed_by_a_jump() {
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("create stores");

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(immutable_store, mutable_store);
                let branch = BranchId::from(uuid::Uuid::now_v7());

                let chain = serialize_linear_chain(&repository, 150).await;
                let older = load_state(&repository, chain[149]).await;
                let newer = serialize_jump_revision(&repository, chain[149], 400).await;

                store_history_step(
                    repository.clone(),
                    branch,
                    STEP_ONE_HUNDRED,
                    RevisionListAcceleration {
                        step_keys: true,
                        list_cache: false,
                    },
                    older,
                    newer,
                )
                .await;

                // 150 -> 400 closes boundaries 200 and 300, both answered by
                // the highest revision numbered <= them: the parent at 150.
                assert_eq!(
                    load_step_key(&repository, branch, 200).await,
                    Some(chain[149])
                );
                assert_eq!(
                    load_step_key(&repository, branch, 300).await,
                    Some(chain[149])
                );

                // 100 was already closed before this push, and 400 holds the
                // new head so its segment is still open. Nothing outside the
                // crossed range may be written.
                assert_eq!(load_step_key(&repository, branch, 100).await, None);
                assert_eq!(load_step_key(&repository, branch, 400).await, None);
                assert_eq!(load_step_key(&repository, branch, 500).await, None);
            }))
            .await;
        }

        #[tokio::test]
        async fn seals_nothing_when_no_boundary_is_crossed() {
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("create stores");

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(immutable_store, mutable_store);
                let branch = BranchId::from(uuid::Uuid::now_v7());

                let chain = serialize_linear_chain(&repository, 105).await;
                let older = load_state(&repository, chain[103]).await;
                let newer = load_state(&repository, chain[104]).await;

                store_history_step(
                    repository.clone(),
                    branch,
                    STEP_ONE_HUNDRED,
                    RevisionListAcceleration::default(),
                    older,
                    newer,
                )
                .await;

                assert_eq!(load_step_key(&repository, branch, 100).await, None);
                assert_eq!(load_step_key(&repository, branch, 200).await, None);
            }))
            .await;
        }

        #[tokio::test]
        async fn caches_lists_only_for_segments_the_jump_closed() {
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("create stores");

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(immutable_store, mutable_store);
                let branch = BranchId::from(uuid::Uuid::now_v7());

                let chain = serialize_linear_chain(&repository, 150).await;
                let older = load_state(&repository, chain[149]).await;
                let newer = serialize_jump_revision(&repository, chain[149], 400).await;

                store_history_step(
                    repository.clone(),
                    branch,
                    STEP_ONE_HUNDRED,
                    RevisionListAcceleration::default(),
                    older,
                    newer,
                )
                .await;

                // Segment 200 spans (100, 200] and really holds 150..=101.
                let cached = load_cached_list(&repository, branch, 150, STEP_ONE_HUNDRED)
                    .await
                    .expect("segment 200 cached");
                let numbers: Vec<u64> = cached.items().iter().map(|item| item.number).collect();
                assert_eq!(numbers.len(), 50);
                assert_eq!(numbers.first(), Some(&150));
                assert_eq!(numbers.last(), Some(&101));

                // Segment 300 is closed but genuinely empty — the jump skipped
                // every number in (200, 300] — and empty segments aren't written.
                assert!(
                    load_cached_list(&repository, branch, 250, STEP_ONE_HUNDRED)
                        .await
                        .is_none()
                );

                // The open segment holding the new head is never cached.
                assert!(
                    load_cached_list(&repository, branch, 400, STEP_ONE_HUNDRED)
                        .await
                        .is_none()
                );
            }))
            .await;
        }

        #[tokio::test]
        async fn writes_nothing_when_acceleration_is_disabled() {
            let (immutable_store, mutable_store, execution) =
                test_store_create().await.expect("create stores");

            Box::pin(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(immutable_store, mutable_store);
                let branch = BranchId::from(uuid::Uuid::now_v7());

                let chain = serialize_linear_chain(&repository, 150).await;
                let older = load_state(&repository, chain[149]).await;
                let newer = serialize_jump_revision(&repository, chain[149], 400).await;

                store_history_step(
                    repository.clone(),
                    branch,
                    STEP_ONE_HUNDRED,
                    RevisionListAcceleration {
                        step_keys: false,
                        list_cache: false,
                    },
                    older,
                    newer,
                )
                .await;

                assert_eq!(load_step_key(&repository, branch, 200).await, None);
                assert_eq!(load_step_key(&repository, branch, 300).await, None);
                assert!(
                    load_cached_list(&repository, branch, 150, STEP_ONE_HUNDRED)
                        .await
                        .is_none()
                );
            }))
            .await;
        }
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
