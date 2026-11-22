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
//! Cache writes are best-effort: any failure aborts the write, and the entry
//! is rebuilt on the next lookup. Cache reads distinguish a missing entry —
//! answered by the slower fallback path — from a store that is overloaded or
//! failing, which is reported so the caller does not escalate to a full
//! parent-chain walk against a store that cannot serve it.

use std::cmp::Ordering;
use std::sync::Arc;

use bytes::Bytes;
use bytes::BytesMut;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::typed_bytes::TypedBytes;
use lore_error_set::prelude::*;
use lore_revision::branch;
use lore_revision::find::FindMatchResult;
use lore_revision::find::find_revision;
use lore_revision::immutable;
use lore_revision::lore::BranchId;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision;
use lore_revision::revision::ResolveSearchLocation;
use lore_revision::state::State;
use lore_revision::state::StateError;
use lore_storage::StoreError;
use lore_telemetry::tracing::fields::BRANCH_ID;
use tracing::debug;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

use crate::grpc::get_write_token;

/// Header size in bytes — written at offset 0 of every cached blob.
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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

/// Interpret a cache read. `Ok(None)` is a miss the caller answers from the
/// slower fallback path; `is_missing` decides which failures count as one.
/// Every other failure is forwarded rather than reported as a miss.
#[track_caller]
fn interpret_cache_read<T, E: ErrorSet>(
    result: Result<T, E>,
    is_missing: impl FnOnce(&E) -> bool,
) -> Result<Option<T>, StateError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(err) if is_missing(&err) => Ok(None),
        Err(err) => Err(err).forward_any::<StateError>("reading revision acceleration data"),
    }
}

/// Acceleration data is an optimisation, so an entry that cannot be read
/// costs time rather than correctness: every failure but backpressure is
/// answered as a miss and left to the slower path. Backpressure is the one
/// failure that must not be, since the slower path is more work against the
/// store that asked for less.
fn acceleration_miss<T>(result: Result<Option<T>, StateError>) -> Result<Option<T>, StateError> {
    match result {
        Ok(value) => Ok(value),
        Err(err) if err.is_slow_down() => Err(err),
        Err(_) => Ok(None),
    }
}

/// Load the cached list at the boundary containing `revision_number`.
/// Returns `Ok(None)` when no valid entry exists. The returned list lets
/// callers iterate items in place without copying.
#[lore_macro::test_pub]
pub(crate) async fn load_cached_list(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    revision_number: u64,
    step_size: u64,
) -> Result<Option<CachedRevisionList>, StateError> {
    let (key, key_type) = branch::revision_list_step_key(
        repository::SALT_LORE,
        repository.id,
        branch,
        revision_number,
        step_size,
    );

    let blob_hash = interpret_cache_read(
        repository
            .clone()
            .read_mutable_store()
            .load(repository.id, key, key_type)
            .await,
        StoreError::is_address_not_found,
    )?;

    let Some(blob_hash) = blob_hash.filter(|hash| !hash.is_zero()) else {
        return Ok(None);
    };

    // A blob whose payload has been reclaimed is as good as absent: the
    // entry is rebuilt from the parent chain on the next backfill.
    let bytes = interpret_cache_read(
        immutable::read(
            repository.clone(),
            Address::zero_context_hash(blob_hash),
            None,
            immutable::read_options_from_repository(repository).with_cache(),
        )
        .await,
        |err| err.is_address_not_found() || err.is_payload_not_found() || err.is_not_found(),
    )?;

    Ok(bytes.and_then(|bytes| {
        CachedRevisionList::from_blob(bytes.to_aligned::<branch::CachedRevisionItem>())
    }))
}

/// If segment B containing `revision_number` is closed (proven by the
/// skip pointer at B + `step_size`), walk `parent_self` from that anchor to
/// populate `List_B`. Returns the cached segment items on success, or
/// `Ok(None)` when the segment is not provably closed or holds no revision.
/// With `seal`, the walk also seals the step boundaries it crosses under
/// `branch`, as [`seal_crossed_boundaries`] describes.
#[lore_macro::test_pub]
pub(crate) async fn try_backfill_segment(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    revision_number: u64,
    step_size: u64,
    seal: bool,
) -> Result<Option<CachedRevisionList>, StateError> {
    let target_b = revision_number.div_ceil(step_size) * step_size;
    let Some(next_b) = target_b.checked_add(step_size) else {
        return Ok(None);
    };
    let Some(anchor) = load_step_pointer(repository, branch, next_b, step_size).await? else {
        return Ok(None);
    };

    let stop_below = target_b.saturating_sub(step_size);
    let max_items = (step_size as usize).saturating_mul(2).saturating_add(2);
    let walk = walk_segment_revisions(repository, anchor, stop_below, max_items).await?;
    if seal {
        for pair in walk.items.windows(2) {
            seal_crossed_boundaries(
                repository,
                branch,
                step_size,
                (pair[1].number, pair[1].signature),
                pair[0].number,
            )
            .await;
        }
    }
    if !walk.reached_terminator {
        return Ok(None);
    }

    let segments = partition_into_segments(&walk.items, step_size);
    for (segment_b, list) in &segments {
        if *segment_b == target_b {
            store_cached_list(repository, branch, *segment_b, step_size, list).await;
        }
    }

    load_cached_list(repository, branch, revision_number, step_size).await
}

/// The skip pointer of `branch` at the step boundary containing
/// `revision_number`: the highest revision of the branch's line numbered at
/// or below the boundary. `None` where the boundary is unsealed. A failed
/// read is reported.
#[lore_macro::test_pub]
pub(crate) async fn load_step_pointer(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    revision_number: u64,
    step_size: u64,
) -> Result<Option<Hash>, StateError> {
    let (key, key_type) = branch::revision_step_key(
        repository::SALT_LORE,
        repository.id,
        branch,
        revision_number,
        step_size,
    );
    let pointer = interpret_cache_read(
        repository
            .clone()
            .read_mutable_store()
            .load(repository.id, key, key_type)
            .await,
        StoreError::is_address_not_found,
    )?;
    Ok(pointer.filter(|pointer| !pointer.is_zero()))
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

    // no filter_slow_down()? usage here: the read that prompted this write has
    // already been answered, so a throttled blob write costs the next reader a
    // slower lookup rather than failing anything.
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
    // no filter_slow_down()? usage here: same reason — a throttled key write
    // leaves the entry to be rebuilt by a later backfill.
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
/// touched is fully traversed. A store asking the caller to back off aborts the
/// walk instead of reporting a partial traversal as a complete one.
pub(crate) async fn walk_segment_revisions(
    repository: &Arc<RepositoryContext>,
    anchor_hash: Hash,
    stop_below: u64,
    max_items: usize,
) -> Result<SegmentWalk, StateError> {
    let mut items: Vec<branch::CachedRevisionItem> = Vec::new();
    let mut hash = anchor_hash;
    let mut reached_terminator = false;

    while items.len() < max_items {
        if hash.is_zero() {
            reached_terminator = true;
            break;
        }
        let state = match State::deserialize(repository.clone(), hash).await {
            Ok(state) => state,
            Err(err) if err.is_slow_down() => return Err(err),
            Err(_) => break,
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

    Ok(SegmentWalk {
        items,
        reached_terminator,
    })
}

/// Partition a contiguous walk of items (highest number first) into per-segment
/// lists keyed by their step-aligned upper boundary `B`. Returned in walk
/// order — highest boundary first. Includes empty boundary entries only if
/// items genuinely belong to them; this function makes no judgement about
/// whether a segment is "fully traversed" — the caller must filter using the
/// `reached_terminator` signal from `walk_segment_revisions`.
#[lore_macro::test_pub]
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

/// Stores `revision` as the skip pointer of `branch` at `boundary`.
async fn store_step_pointer(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    history_step_size: u64,
    boundary: u64,
    revision: Hash,
) -> Result<(), StoreError> {
    let (key, key_type) = branch::revision_step_key(
        repository::SALT_LORE,
        repository.id,
        branch,
        boundary,
        history_step_size,
    );
    let write_token = get_write_token();
    repository
        .clone()
        .write_mutable_store(&write_token)
        .store(repository.id, key, revision, key_type)
        .await
}

/// Seals the skip pointer of `branch` at each step boundary B that a pair of
/// consecutive revisions of its line crosses, `older.0 <= B < newer_number`,
/// as [`sealed_boundaries`] picks them. `older` is the number and signature
/// of the first parent of the revision numbered `newer_number`, and the
/// line's highest revision at or below each such B, so each pointer holds
/// it. Best effort: a failed write fails nothing. Answers how many pointers
/// the store took.
#[lore_macro::test_pub]
pub(crate) async fn seal_crossed_boundaries(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    history_step_size: u64,
    older: (u64, Hash),
    newer_number: u64,
) -> u64 {
    let Some((lowest_b, highest_b)) = sealed_boundaries(older.0, newer_number, history_step_size)
    else {
        return 0;
    };
    let mut sealed = 0;
    for boundary in (lowest_b..=highest_b).step_by(history_step_size as usize) {
        if store_step_pointer(repository, branch, history_step_size, boundary, older.1)
            .await
            .is_ok()
        {
            sealed += 1;
        }
    }
    debug!({BRANCH_ID} = %branch, lowest_b, highest_b, sealed, "Sealed history step keys");
    sealed
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
        seal_crossed_boundaries(
            &repository,
            branch,
            history_step_size,
            (older_state.revision_number(), older_state.revision()),
            newer_state.revision_number(),
        )
        .await;
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

    // no filter_slow_down()? usage here: the list-cache write is best-effort,
    // so a throttled walk leaves the entry to be rebuilt by a later backfill.
    let Ok(walk) =
        walk_segment_revisions(&repository, newer_state.revision(), stop_below, max_items).await
    else {
        return;
    };

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

/// Resolve `branch` revision `revision_number` to its signature, consulting
/// the step acceleration structures before walking history.
///
/// A cached segment covering the number answers it outright. Otherwise the
/// sealed boundary at or above the number anchors a bounded walk. Anything not
/// served from acceleration data falls through to [`revision::resolve`], so an
/// absent or unusable entry costs time rather than correctness. The one
/// exception is a store asking the caller to back off: the fallback is more
/// work against that store, so it is reported instead of walked.
pub async fn resolve_revision_number(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    revision_number: u64,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
) -> Result<Hash, StateError> {
    if let Some(signature) = resolve_from_acceleration(
        repository,
        branch,
        revision_number,
        history_step_size,
        acceleration,
    )
    .await?
    {
        return Ok(signature);
    }

    revision::resolve_boxed(
        repository.clone(),
        format!("{branch}@{revision_number}"),
        ResolveSearchLocation::Local,
    )
    .await
}

/// Signature for `revision_number` if the acceleration structures can supply
/// it, or `None` when the caller must walk history.
async fn resolve_from_acceleration(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    revision_number: u64,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
) -> Result<Option<Hash>, StateError> {
    if acceleration.list_cache
        && let Some(cached) = acceleration_miss(
            load_cached_list(repository, branch, revision_number, history_step_size).await,
        )?
        && let Some(item) = cached
            .items()
            .iter()
            .find(|item| item.number == revision_number)
    {
        debug!(
            number = revision_number,
            "Resolved revision number from cached segment"
        );
        return Ok(Some(item.signature));
    }

    if !acceleration.step_keys {
        return Ok(None);
    }

    resolve_via_step_key(repository, branch, revision_number, history_step_size).await
}

/// Signature for `revision_number`, reached from the sealed boundary covering
/// it. `None` when the boundary is unsealed or its anchor does not lead to the
/// number, which leaves the caller to walk history.
///
/// The anchor is the highest revision numbered at or below its boundary, so a
/// walk from it reaches every revision the boundary's segment contains. An
/// anchor that does not lead to `revision_number` is reported as `None` rather
/// than as absence: acceleration data can be stale or predate a key change, and
/// a caller that treated that as "no such revision" would report an existing
/// revision as missing.
pub(crate) async fn resolve_via_step_key(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    revision_number: u64,
    history_step_size: u64,
) -> Result<Option<Hash>, StateError> {
    let anchor = acceleration_miss(
        load_step_pointer(repository, branch, revision_number, history_step_size).await,
    )?;
    let Some(anchor) = anchor else {
        return Ok(None);
    };

    // An anchor holding the highest revision at or below its boundary is at
    // most one segment above the target, so a walk longer than that is reading
    // an anchor that does not describe the branch any more. Give up and let
    // the caller walk history rather than following it.
    let search_limit = (history_step_size as usize).saturating_add(1);
    let signature = find_revision(
        repository.clone(),
        branch,
        anchor,
        false,
        Some(search_limit),
        |state, _metadata| match state.revision_number().cmp(&revision_number) {
            Ordering::Equal => FindMatchResult::Match,
            Ordering::Less => FindMatchResult::Abort,
            Ordering::Greater => FindMatchResult::Continue,
        },
    )
    .await;
    let Some(signature) = acceleration_miss(
        signature
            .forward_any::<StateError>("resolving revision from history step key")
            .map(Some),
    )?
    else {
        return Ok(None);
    };

    debug!(
        number = revision_number,
        %anchor,
        "Resolved revision number from history step key"
    );
    Ok(Some(signature))
}
