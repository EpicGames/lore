// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_proto::lore::model::v1 as model_v1;
use lore_proto::lore::revision::v1::RevisionListRequest;
use lore_proto::lore::revision::v1::RevisionListResponse;
use lore_proto::lore::revision::v1::revision_list_request::Start;
use lore_revision::branch;
use lore_revision::lore::BranchId;
use lore_revision::metadata::Metadata;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision;
use lore_revision::revision::ResolveSearchLocation;
use lore_revision::state;
use lore_revision::util;
use lore_storage::StoreError;
use lore_telemetry::LabelArray;
use lore_telemetry::observe::Observe;
use lore_telemetry::observe::ObserveResult;
use lore_telemetry::observe::observe_result;
use lore_telemetry::tracing::fields::REPOSITORY_ID;
use lore_telemetry::tracing::fields::REVISION;
use lore_transport::grpc::REVISION_LIST_STRATEGY_HEADER;
use opentelemetry::KeyValue;
use smallvec::smallvec;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tonic::metadata::MetadataValue;
use tracing::debug;
use tracing::warn;
use zerocopy::IntoBytes;

use crate::cache;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::ServerResultExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::none_or_status;
use crate::grpc::revision::v1::service::RevisionListInstruments;
use crate::grpc::warn_error_to_status;
use crate::util::setup_execution;

const MAX_REVISION_LIST_RESPONSE_ITEMS: usize = 100;
const METRICS_START_KEY: &str = "start_type";
const METRICS_LIST_STRATEGY_KEY: &str = "list_strategy";

enum RevisionListStrategy {
    Direct,
    FullIteration,
    HistoryStep,
    ListCache,
    ListCacheBackfill,
}

impl RevisionListStrategy {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::FullIteration => "full-iteration",
            Self::HistoryStep => "history-step",
            Self::ListCache => "list-cache",
            Self::ListCacheBackfill => "list-cache-backfill",
        }
    }
}

/// Outcome of `resolve_start`: either a pre-built page from the cache,
/// or a starting hash that the walker still needs to expand.
enum ResolveStart {
    /// Page pre-built from cached segment items. Carries the branch
    /// (needed for the forward-cursor lookup) and the parent of the
    /// last item (`signature_backward`), so the handler can build a
    /// response without invoking the walker.
    Items {
        items: Vec<model_v1::RevisionItem>,
        branch: BranchId,
        next_older: Option<Hash>,
        strategy: RevisionListStrategy,
    },
    /// Hash to walk `parent_self` from, plus the strategy that led here.
    Walk {
        start: Hash,
        strategy: RevisionListStrategy,
    },
}

impl ResolveStart {
    fn strategy(&self) -> &RevisionListStrategy {
        match self {
            Self::Items { strategy, .. } | Self::Walk { strategy, .. } => strategy,
        }
    }
}

/// Build a v1 `RevisionItem` page from cached segment items. Each item's
/// `state` field carries the full 320-byte serialized state so clients
/// avoid a follow-up fetch.
fn cached_to_proto(items: &[branch::CachedRevisionItem]) -> Vec<model_v1::RevisionItem> {
    items
        .iter()
        .map(|item| model_v1::RevisionItem {
            number: item.number,
            signature: Bytes::from_owner(item.signature),
            metadata: Bytes::from_owner(item.metadata),
            state: Bytes::copy_from_slice(item.state.as_bytes()),
        })
        .collect()
}

/// `signature_backward` for a cache-served page: the parent of the
/// segment's bottom item. None when that parent is the zero sentinel
/// (the bottom item is the root revision).
fn cached_next_older(items: &[branch::CachedRevisionItem]) -> Option<Hash> {
    let last = items.last()?;
    let parent = last.state.parent[0];
    (!parent.is_zero()).then_some(parent)
}

impl std::fmt::Display for RevisionListStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

fn start_to_metric_value(value: &Start) -> &'static str {
    match value {
        Start::Identifier(_) => "identifier",
        Start::Signature(_) => "signature",
    }
}

/// `lore.revision.v1.RevisionService.RevisionList` handler.
///
/// Returns a page of revisions newer-to-older starting from the
/// `start` anchor, plus optional cursors for the adjacent pages.
/// `signature_backward` is items[N-1]'s parent — absent when items[N-1]
/// is the root revision. `signature_forward` is the revision whose
/// `parent_self` is items[0]'s signature — absent only when items[0]
/// is the branch's latest revision, i.e. there is genuinely no newer
/// revision.
#[tracing::instrument(name = "RevisionList::v1::handle", skip_all)]
pub async fn handler(
    request: Request<RevisionListRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
    instruments: &RevisionListInstruments,
) -> Result<Response<RevisionListResponse>, Status> {
    let repository_id = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let req = request.into_inner();

    let Some(start) = req.start else {
        return Err(Status::invalid_argument(
            "RevisionListRequest.start must be set",
        ));
    };

    let execution = setup_execution(module_path!(), correlation_id, user_id);
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository_id,
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            let resolved = {
                let labels = smallvec![KeyValue::new(
                    METRICS_START_KEY,
                    start_to_metric_value(&start),
                )];
                resolve_start(start, &repository, history_step_size, acceleration)
                    .observe(
                        instruments.resolve_start_duration.clone(),
                        labels,
                        observe_resolve_start(),
                    )
                    .await
                    .output?
            };

            let (walked, strategy) = match resolved {
                ResolveStart::Items {
                    items,
                    branch,
                    next_older,
                    strategy,
                } => {
                    debug!(count = items.len(), %strategy, "Listing revisions from cache");
                    (
                        Walked {
                            items,
                            branch: Some(branch),
                            next_older,
                        },
                        strategy,
                    )
                }
                ResolveStart::Walk { start, strategy } => {
                    debug!({REVISION} = %start, %strategy, "Listing revisions");
                    let walked = walk_revisions(
                        start,
                        &strategy,
                        &repository,
                        history_step_size,
                        acceleration,
                        instruments,
                    )
                    .observe_result(
                        instruments.walk_duration.clone(),
                        smallvec![KeyValue::new(METRICS_LIST_STRATEGY_KEY, strategy.as_str())],
                    )
                    .await
                    .output?;
                    (walked, strategy)
                }
            };

            let signature_forward =
                forward_cursor(&repository, &walked, history_step_size, acceleration).await?;
            let signature_backward = walked.next_older;

            debug!(
                count = walked.items.len(),
                forward = ?signature_forward,
                backward = ?signature_backward,
                "RevisionList response",
            );

            let mut response = Response::new(RevisionListResponse {
                items: walked.items,
                signature_forward: signature_forward.map(Into::into),
                signature_backward: signature_backward.map(Into::into),
            });
            response.metadata_mut().insert(
                REVISION_LIST_STRATEGY_HEADER,
                MetadataValue::from_static(strategy.as_str()),
            );
            Ok(response)
        })
        .await
}

/// Resolves the request's `start` anchor. May return pre-built items
/// from the cache, or a hash for the walker to expand. Tip resolution
/// (`number == 0`) takes a direct path via `branch::load_latest` since
/// the step-key dance would always miss for the zero block.
async fn resolve_start(
    start: Start,
    repository: &Arc<RepositoryContext>,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
) -> Result<ResolveStart, Status> {
    match start {
        Start::Signature(signature) => {
            let hash = Hash::from(signature);
            if acceleration.list_cache
                && let Some(cached) =
                    try_serve_signature_from_cache(repository, hash, history_step_size).await?
            {
                return Ok(cached);
            }
            Ok(ResolveStart::Walk {
                start: hash,
                strategy: RevisionListStrategy::Direct,
            })
        }
        Start::Identifier(identifier) => {
            let branch = BranchId::from(&identifier.branch_id);
            if identifier.number == 0 {
                let hash = branch::load_latest(repository.clone(), branch)
                    .await
                    .filter_slow_down()?
                    .warn_map_err(|err| {
                        Status::not_found(format!("Branch {branch} not found: {err}"))
                    })?;
                return Ok(ResolveStart::Walk {
                    start: hash,
                    strategy: RevisionListStrategy::Direct,
                });
            }

            if acceleration.list_cache {
                if let Some(cached) = cache::revision::load_cached_list(
                    repository,
                    branch,
                    identifier.number,
                    history_step_size,
                )
                .await
                .filter_slow_down()?
                .unwrap_or_default()
                    && cached
                        .items()
                        .iter()
                        .any(|item| item.number == identifier.number)
                {
                    debug!(
                        number = identifier.number,
                        "Served revision list from cache"
                    );
                    return Ok(ResolveStart::Items {
                        items: cached_to_proto(cached.items()),
                        branch,
                        next_older: cached_next_older(cached.items()),
                        strategy: RevisionListStrategy::ListCache,
                    });
                }

                if let Some(cached) = cache::revision::try_backfill_segment(
                    repository,
                    branch,
                    identifier.number,
                    history_step_size,
                )
                .await
                .filter_slow_down()?
                .unwrap_or_default()
                    && cached
                        .items()
                        .iter()
                        .any(|item| item.number == identifier.number)
                {
                    debug!(number = identifier.number, "Backfilled revision list cache");
                    return Ok(ResolveStart::Items {
                        items: cached_to_proto(cached.items()),
                        branch,
                        next_older: cached_next_older(cached.items()),
                        strategy: RevisionListStrategy::ListCacheBackfill,
                    });
                }
            }

            let step_key_hit = if acceleration.step_keys {
                cache::revision::resolve_via_step_key(
                    repository,
                    branch,
                    identifier.number,
                    history_step_size,
                )
                .await
                .filter_slow_down()?
                .unwrap_or_default()
            } else {
                None
            };

            if let Some(hash) = step_key_hit {
                Ok(ResolveStart::Walk {
                    start: hash,
                    strategy: RevisionListStrategy::HistoryStep,
                })
            } else {
                let signature = format!("{branch}@{}", identifier.number);
                let hash = revision::resolve(
                    repository.clone(),
                    signature,
                    None,
                    ResolveSearchLocation::Local,
                )
                .await
                .filter_slow_down()?
                .map_err(|err| Status::not_found(format!("Revision not found: {err}")))?;
                Ok(ResolveStart::Walk {
                    start: hash,
                    strategy: RevisionListStrategy::FullIteration,
                })
            }
        }
    }
}

/// Try to serve a signature-anchored request from the cache: deserialize
/// the state to learn the branch (from metadata) and revision number,
/// look up the segment's cached list, and serve it if the requested
/// signature appears in the items.
async fn try_serve_signature_from_cache(
    repository: &Arc<RepositoryContext>,
    signature: Hash,
    history_step_size: u64,
) -> Result<Option<ResolveStart>, Status> {
    let state = match state::State::deserialize(repository.clone(), signature)
        .await
        .filter_slow_down()?
    {
        Ok(state) => state,
        Err(err) => {
            debug!(%signature, ?err, "Cache fast path: state deserialize failed");
            return Ok(None);
        }
    };
    let metadata = match Metadata::deserialize(repository.clone(), state.metadata_hash())
        .await
        .filter_slow_down()?
    {
        Ok(metadata) => metadata,
        Err(err) => {
            debug!(%signature, ?err, "Cache fast path: metadata deserialize failed");
            return Ok(None);
        }
    };
    let branch = match metadata.get_branch() {
        Ok(branch) => branch,
        Err(err) => {
            debug!(%signature, ?err, "Cache fast path: metadata missing branch");
            return Ok(None);
        }
    };
    let revision_number = state.revision_number();
    let (cached, strategy) = if let Some(items) =
        cache::revision::load_cached_list(repository, branch, revision_number, history_step_size)
            .await
            .filter_slow_down()?
            .unwrap_or_default()
    {
        (items, RevisionListStrategy::ListCache)
    } else {
        let Some(backfilled) = cache::revision::try_backfill_segment(
            repository,
            branch,
            revision_number,
            history_step_size,
        )
        .await
        .filter_slow_down()?
        .unwrap_or_default() else {
            return Ok(None);
        };
        (backfilled, RevisionListStrategy::ListCacheBackfill)
    };
    if !cached
        .items()
        .iter()
        .any(|item| item.signature == signature)
    {
        return Ok(None);
    }
    Ok(Some(ResolveStart::Items {
        items: cached_to_proto(cached.items()),
        branch,
        next_older: cached_next_older(cached.items()),
        strategy,
    }))
}

fn observe_resolve_start()
-> impl Fn(&Result<ResolveStart, Status>, &Duration, &mut LabelArray) + Copy {
    move |result: &Result<ResolveStart, Status>, elapsed: &Duration, labels: &mut LabelArray| {
        observe_result(result, elapsed, labels);
        if let Ok(ok) = result {
            labels.push(KeyValue::new(
                METRICS_LIST_STRATEGY_KEY,
                ok.strategy().as_str(),
            ));
        }
    }
}

struct Walked {
    items: Vec<model_v1::RevisionItem>,
    /// Branch the page belongs to. Captured from items[0]'s metadata
    /// for the forward-cursor lookup.
    branch: Option<BranchId>,
    /// Hash of the revision one older than items[N-1] — feeds straight
    /// into `signature_backward`. None when items[N-1] is the root.
    next_older: Option<Hash>,
}

async fn walk_revisions(
    start: Hash,
    strategy: &RevisionListStrategy,
    repository: &Arc<RepositoryContext>,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
    instruments: &RevisionListInstruments,
) -> Result<Walked, Status> {
    let mut items: Vec<model_v1::RevisionItem> =
        Vec::with_capacity(MAX_REVISION_LIST_RESPONSE_ITEMS);
    let mut current = start;
    let mut branch: Option<BranchId> = None;
    let mut next_older: Option<Hash> = None;
    let mut first = true;
    // Segment-aligns the walk: walk-served pages stop at the floor so they line up
    // with cache-served pages and consecutive backward-cursor calls don't overlap.
    let mut segment_floor: Option<u64> = None;
    let mut prev_step_state: Option<Arc<state::State>> = None;

    while items.len() < MAX_REVISION_LIST_RESPONSE_ITEMS && !current.is_zero() {
        let state = state::State::deserialize(repository.clone(), current)
            .await
            .filter_slow_down()?
            .map_err(|err| {
                if first {
                    if err.is_not_found() {
                        Status::not_found(format!("Revision {current} not found"))
                    } else {
                        warn!(
                            {REPOSITORY_ID} = %repository.id, revision = %current, ?err,
                            "Failed to deserialize base revision state",
                        );
                        warn_error_to_status(&err, |e| Status::internal(e.to_string()))
                    }
                } else {
                    warn!(
                        {REPOSITORY_ID} = %repository.id, revision = %current, ?err,
                        "Failed to deserialize revision state mid-walk",
                    );
                    warn_error_to_status(&err, |e| Status::internal(e.to_string()))
                }
            })?;

        if first
            && let Ok(metadata) = Metadata::deserialize(repository.clone(), state.metadata_hash())
                .await
                .filter_slow_down()?
        {
            if let Ok(b) = metadata.get_branch() {
                branch = Some(b);
            }
            if let Ok(state_timestamp) = metadata.get_timestamp() {
                let current_timestamp = util::time::timestamp();
                let age_seconds = (current_timestamp - state_timestamp) / 1000;
                instruments.relative_age_seconds.record(
                    age_seconds,
                    &[KeyValue::new(METRICS_LIST_STRATEGY_KEY, strategy.as_str())],
                );
            }
        }

        let current_number = state.revision_number();

        // Backfill missing history-step keys when full-iteration crosses a
        // step boundary. Subsequent paginated calls can then take the
        // HistoryStep fast path. Skipped when step keys are disabled. Must
        // run before the segment-floor check below: the crossing this
        // detects and the walk's exit point are the same revision, so
        // checking floor first would break out before this ever ran.
        if acceleration.step_keys
            && matches!(strategy, RevisionListStrategy::FullIteration)
            && let Some(previous_state) = &prev_step_state
            && let Some((lowest_b, highest_b)) = cache::revision::sealed_boundaries(
                state.revision_number(),
                previous_state.revision_number(),
                history_step_size,
            )
            // no filter_slow_down()? usage here: this read only enables the
            // best-effort step-key backfill below.
            && let Ok(metadata) =
                Metadata::deserialize(repository.clone(), previous_state.metadata_hash()).await
            && let Ok(branch_id) = metadata.get_branch()
        {
            for boundary in (lowest_b..=highest_b).step_by(history_step_size as usize) {
                let _ = cache::revision::seal_boundary_revision_number(
                    repository.clone(),
                    branch_id,
                    history_step_size,
                    boundary,
                    &state,
                    previous_state,
                )
                .await;
                debug!(boundary, "Backfilled history step key");
            }
        }
        if matches!(strategy, RevisionListStrategy::FullIteration) {
            prev_step_state = Some(state.clone());
        }

        if first {
            let b = current_number.div_ceil(history_step_size) * history_step_size;
            segment_floor = Some(b.saturating_sub(history_step_size).saturating_add(1));
        } else if let Some(floor) = segment_floor
            && current_number < floor
        {
            next_older = Some(current);
            break;
        }

        items.push(model_v1::RevisionItem {
            number: current_number,
            signature: current.into(),
            metadata: state.metadata_hash().into(),
            state: Bytes::copy_from_slice(state.state_data().as_bytes()),
        });

        let parent = state.parent_self();
        first = false;

        if items.len() == MAX_REVISION_LIST_RESPONSE_ITEMS {
            if !parent.is_zero() {
                next_older = Some(parent);
            }
            break;
        }

        if parent.is_zero() {
            break;
        }
        current = parent;
    }

    Ok(Walked {
        items,
        branch,
        next_older,
    })
}

/// Outcome of reading the step-boundary skip pointer at a boundary `B`.
/// `B`'s pointer holds the highest revision numbered `<= B`, so a value
/// other than `first_signature` proves that revision is strictly above
/// `first_number` (`Found`). A pointer still equal to `first_signature`
/// proves nothing above `B` exists (`Empty`) — a real, load-bearing fact
/// callers use to safely widen the search. A missing key proves nothing
/// either way (`Unknown`): the seal write is best-effort and silently
/// dropped on failure, and the key type was renamed once already,
/// orphaning older entries under the previous name. Conflating `Unknown`
/// with `Empty` would let the search skip past a boundary that is
/// genuinely non-empty but whose pointer was simply lost.
enum BoundaryProbe {
    Found(Hash),
    Empty,
    Unknown,
}

/// Reads the step-boundary skip pointer at `boundary`. See [`BoundaryProbe`].
async fn probe_step_boundary(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    boundary: u64,
    first_signature: Hash,
    history_step_size: u64,
) -> Result<BoundaryProbe, Status> {
    let (key, key_type) = branch::revision_step_key(
        repository::SALT_LORE,
        repository.id,
        branch,
        boundary,
        history_step_size,
    );
    match none_or_status(
        repository
            .read_mutable_store()
            .load(repository.id, key, key_type)
            .await,
        StoreError::is_address_not_found,
    )? {
        Some(revision) if revision != first_signature => Ok(BoundaryProbe::Found(revision)),
        Some(_) => Ok(BoundaryProbe::Empty),
        None => Ok(BoundaryProbe::Unknown),
    }
}

/// Where the descent to the forward-cursor target should start from, and
/// how far it is safe to trust a single-band bound.
enum ForwardAnchor {
    /// A sealed boundary was found; every boundary below it down to
    /// `first_number`'s own was proven empty, so its band — at most
    /// `history_step_size` revisions — is guaranteed to hold the target.
    Sealed { anchor: Hash, boundary: u64 },
    /// No boundary above `first_number` is sealed at all: the target
    /// lives in the branch's latest revision's own (always-open) band,
    /// likewise bounded to `history_step_size` revisions.
    LatestBand { anchor: Hash },
    /// A skip pointer was missing somewhere below `anchor`, so the gap
    /// between `first_number` and `anchor` cannot be trusted to be a
    /// single band. `anchor` is still guaranteed to be numbered above
    /// `first_number` (either a proven `Found` boundary, or the latest
    /// revision), but the descent must walk further and repair the
    /// missing pointer(s) it discovers instead of assuming one band.
    UnverifiedGap { anchor: Hash },
}

/// Finds where to start descending to the lowest revision numbered
/// strictly above `first_number`.
///
/// A boundary `B`'s skip pointer holds the highest revision numbered
/// `<= B`, which only increases with `B`. So "boundary `B` is `Empty`" is
/// monotonic in `B` — true up to some point, then false from there on —
/// and the lowest boundary where it turns false can be found by binary
/// search instead of a linear scan, using `Empty` results (not `Unknown`
/// ones — see [`BoundaryProbe`]) to narrow the range. The search is
/// bounded above by `latest`'s own boundary, which is never sealed (the
/// segment holding the branch's latest revision is always open), giving
/// a correct upper limit no matter how wide the gap above `first_number`
/// is. A fixed probe budget cannot do this safely: it has no way to tell
/// "nothing is sealed above this point" (target lives in latest's open
/// band) apart from "the real answer is just further away than the
/// budget allows" (which would wrongly fall back to a walk anchored on
/// latest, spanning however much ordinary history has since accumulated).
///
/// Hitting `Unknown` at any point — the boundary's true state cannot be
/// determined — aborts the binary search and reports [`ForwardAnchor::UnverifiedGap`]
/// anchored on the best boundary already proven `Found` (or `latest`, if
/// none has been found yet), rather than risking `Empty`'s optimism on
/// a boundary that might not actually be empty.
///
/// The immediately-following boundary is tried first as a fast path: for
/// ordinary, non-gapped pagination this resolves in one probe and never
/// needs to deserialize `latest`. Only a miss there pays for one `latest`
/// deserialize (to learn its boundary) before binary-searching.
async fn forward_anchor(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    first_number: u64,
    first_signature: Hash,
    latest: Hash,
    history_step_size: u64,
) -> Result<ForwardAnchor, Status> {
    let first_boundary = first_number
        .saturating_add(1)
        .div_ceil(history_step_size)
        .saturating_mul(history_step_size);

    match probe_step_boundary(
        repository,
        branch,
        first_boundary,
        first_signature,
        history_step_size,
    )
    .await?
    {
        BoundaryProbe::Found(revision) => {
            return Ok(ForwardAnchor::Sealed {
                anchor: revision,
                boundary: first_boundary,
            });
        }
        BoundaryProbe::Empty => {}
        BoundaryProbe::Unknown => return Ok(ForwardAnchor::UnverifiedGap { anchor: latest }),
    }

    let latest_state = state::State::deserialize(repository.clone(), latest)
        .await
        .filter_slow_down()?
        .map_err(|err| warn_error_to_status(&err, |e| Status::internal(e.to_string())))?;
    let latest_boundary =
        latest_state.revision_number().div_ceil(history_step_size) * history_step_size;

    if latest_boundary <= first_boundary {
        // Nothing above `first_boundary` is sealed at all — the target
        // lives in latest's own (always-open) band.
        return Ok(ForwardAnchor::LatestBand { anchor: latest });
    }

    // Invariant: `low` is a boundary proven `Empty` (established above for
    // `first_boundary`, or by the loop body below); `high` is either
    // `latest_boundary` (an unsealed sentinel) or a boundary proven
    // `Found`, whose hash is cached in `high_anchor`. Both start, and
    // every `mid` stays, a multiple of `history_step_size`, so
    // `high - low` is always a multiple of it too; the loop only runs
    // while that gap exceeds one step, so `mid` strictly separates `low`
    // and `high` every iteration.
    let mut low = first_boundary;
    let mut high = latest_boundary;
    let mut high_anchor: Option<Hash> = None;

    while high - low > history_step_size {
        let mid = low + ((high - low) / (2 * history_step_size)) * history_step_size;
        match probe_step_boundary(repository, branch, mid, first_signature, history_step_size)
            .await?
        {
            BoundaryProbe::Found(revision) => {
                high = mid;
                high_anchor = Some(revision);
            }
            BoundaryProbe::Empty => low = mid,
            BoundaryProbe::Unknown => {
                return Ok(ForwardAnchor::UnverifiedGap {
                    anchor: high_anchor.unwrap_or(latest),
                });
            }
        }
    }

    match high_anchor {
        Some(revision) if high < latest_boundary => Ok(ForwardAnchor::Sealed {
            anchor: revision,
            boundary: high,
        }),
        _ => Ok(ForwardAnchor::LatestBand { anchor: latest }),
    }
}

/// Descends from a [`ForwardAnchor::Sealed`] or [`ForwardAnchor::LatestBand`]
/// anchor to the lowest revision numbered strictly above `first_number`.
/// `anchor_boundary`, when set, is the sealed step boundary `anchor` was
/// read from; when `list_cache_enabled`, the persisted list-cache blob for
/// that band is tried first (one blob read) before falling back to a
/// walk. Either way — a sealed boundary's band, or (when `anchor_boundary`
/// is unset) the branch's latest revision's own open band — the band
/// holds at most `history_step_size` revisions, so the walk below is
/// bounded by that alone; no larger cap is needed.
async fn forward_target(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    anchor: Hash,
    anchor_boundary: Option<u64>,
    first_number: u64,
    history_step_size: u64,
    list_cache_enabled: bool,
) -> Result<Hash, Status> {
    if list_cache_enabled
        && let Some(boundary) = anchor_boundary
        && let Some(cached) =
            cache::revision::load_cached_list(repository, branch, boundary, history_step_size)
                .await
                .filter_slow_down()?
                .unwrap_or_default()
        && let Some(item) = cached
            .items()
            .iter()
            .rev()
            .find(|item| item.number > first_number)
    {
        return Ok(item.signature);
    }

    let max_items = history_step_size as usize + 1;
    let walk = cache::revision::walk_segment_revisions(repository, anchor, first_number, max_items)
        .await
        .filter_slow_down()?
        .map_err(|err| warn_error_to_status(&err, |e| Status::internal(e.to_string())))?;
    if !walk.reached_terminator {
        return Err(Status::internal(format!(
            "forward cursor descent from {anchor} exceeded {max_items} hops without \
             resolving the successor of revision {first_number}",
        )));
    }

    Ok(walk
        .items
        .into_iter()
        .rev()
        .find(|item| item.number > first_number)
        .expect("anchor's own item always has number > first_number")
        .signature)
}

/// Walks `parent_self` from `anchor` down to the lowest revision numbered
/// strictly above `first_number`, without assuming the gap fits in one
/// band. When `step_keys` acceleration is enabled, backfills any
/// step-boundary skip pointer discovered missing along the way — mirroring
/// the backfill `walk_revisions` performs for the `FullIteration` strategy
/// — so a request that pays this walk's cost once repairs the fast path
/// for later requests instead of leaving every subsequent lookup to
/// rediscover the same gap.
///
/// Deliberately uncapped by hop count: revision numbers strictly decrease
/// along `parent_self`, so this always terminates at `first_number` or the
/// root, bounded only by the branch's real history depth — the same
/// guarantee `resolve_start`'s `FullIteration` fallback already relies on
/// via `revision::resolve(..., None, ...)` when no acceleration is
/// available. An item-count cap here would fail requests anchored deep in
/// a long, legitimately un-accelerated history — worse, retrying such a
/// request would fail identically every time, since this always restarts
/// from `anchor` and backfills top-down without ever reaching the
/// boundary nearer `first_number` that a retry's first probe would check.
/// `RevisionList` is wrapped in `timeout_grpc` at the service layer, which
/// is the appropriate backstop for a pathologically deep walk, not a
/// count that silently caps correctness.
async fn descend_unverified_gap(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    anchor: Hash,
    first_number: u64,
    history_step_size: u64,
    step_keys_enabled: bool,
) -> Result<Hash, Status> {
    let mut hash = anchor;
    let mut prev_state: Option<Arc<state::State>> = None;
    // Invariant: `last_above` is the most recently visited hash whose
    // number was confirmed `> first_number` — initially `anchor` itself,
    // per the caller's guarantee that every `ForwardAnchor` variant is
    // numbered above `first_number`.
    let mut last_above = anchor;

    loop {
        let current_state = state::State::deserialize(repository.clone(), hash)
            .await
            .filter_slow_down()?
            .map_err(|err| warn_error_to_status(&err, |e| Status::internal(e.to_string())))?;
        let number = current_state.revision_number();

        if step_keys_enabled
            && let Some(previous_state) = &prev_state
            && let Some((lowest_b, highest_b)) = cache::revision::sealed_boundaries(
                number,
                previous_state.revision_number(),
                history_step_size,
            )
        {
            for boundary in (lowest_b..=highest_b).step_by(history_step_size as usize) {
                let _ = cache::revision::seal_boundary_revision_number(
                    repository.clone(),
                    branch,
                    history_step_size,
                    boundary,
                    &current_state,
                    previous_state,
                )
                .await;
                debug!(boundary, "Backfilled history step key during gap descent");
            }
        }

        if number <= first_number {
            return Ok(last_above);
        }
        last_above = hash;
        hash = current_state.parent_self();
        prev_state = Some(current_state);
    }
}

/// Looks up the revision whose `parent_self` is items[0]'s signature —
/// i.e. the cursor for the next newer page. Revision numbers increase
/// strictly along `parent_self`, so that revision is exactly the lowest
/// one numbered above items[0]; this walks the skip-pointer chain
/// upward from the current page to find it, rather than assuming
/// `items[0].number + 1` exists (a merge or fast-forward can leave
/// numbering gaps). Returns `Ok(None)` only when items[0] is genuinely
/// the branch's latest revision.
async fn forward_cursor(
    repository: &Arc<RepositoryContext>,
    walked: &Walked,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
) -> Result<Option<Hash>, Status> {
    let Some(first) = walked.items.first() else {
        return Ok(None);
    };
    let Some(branch) = walked.branch else {
        return Ok(None);
    };
    let first_number = first.number;
    let first_signature = Hash::from(first.signature.as_ref());

    let latest = branch::load_latest(repository.clone(), branch)
        .await
        .filter_slow_down()?
        .map_err(|err| {
            if err.is_branch_not_found() {
                Status::not_found(format!("Branch {branch} not found: {err}"))
            } else {
                warn_error_to_status(&err, |e| Status::internal(e.to_string()))
            }
        })?;
    if latest.is_zero() || latest == first_signature {
        return Ok(None);
    }

    // With step-key acceleration disabled, `forward_anchor` has nothing to
    // probe — every read it would perform is exactly the data this flag is
    // documented to gate (`RevisionListAcceleration::step_keys`: "read +
    // write"). `latest` is the only anchor available; descend from it
    // directly, same as `resolve_start` falling through to `FullIteration`.
    if !acceleration.step_keys {
        return descend_unverified_gap(
            repository,
            branch,
            latest,
            first_number,
            history_step_size,
            false,
        )
        .await
        .map(Some);
    }

    let anchor = forward_anchor(
        repository,
        branch,
        first_number,
        first_signature,
        latest,
        history_step_size,
    )
    .await?;

    let target = match anchor {
        ForwardAnchor::Sealed { anchor, boundary } => {
            forward_target(
                repository,
                branch,
                anchor,
                Some(boundary),
                first_number,
                history_step_size,
                acceleration.list_cache,
            )
            .await?
        }
        ForwardAnchor::LatestBand { anchor } => {
            forward_target(
                repository,
                branch,
                anchor,
                None,
                first_number,
                history_step_size,
                acceleration.list_cache,
            )
            .await?
        }
        ForwardAnchor::UnverifiedGap { anchor } => {
            descend_unverified_gap(
                repository,
                branch,
                anchor,
                first_number,
                history_step_size,
                acceleration.step_keys,
            )
            .await?
        }
    };
    Ok(Some(target))
}

#[cfg(test)]
mod test {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Hash;
    use lore_revision::branch;
    use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
    use lore_revision::lore::RepositoryId;
    use lore_revision::metadata::Metadata;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::state::State;
    use lore_storage::StoreError;
    use lore_telemetry::InstrumentProvider;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use opentelemetry::KeyValue;
    use rand::random;
    use tonic::Request;

    use super::*;
    use crate::grpc::get_write_token;
    use crate::grpc::handlers::branch_push;
    use crate::store::FailingLoadStore;
    use crate::store::test_store_create;

    struct TestInstrumentProvider {}

    impl InstrumentProvider for TestInstrumentProvider {
        fn namespace(&self) -> &'static str {
            "test"
        }
        fn labels(&self) -> &[KeyValue] {
            &[]
        }
    }

    fn make_instruments() -> RevisionListInstruments {
        let provider = TestInstrumentProvider {};
        RevisionListInstruments {
            resolve_start_duration: provider.latency_histogram_ms("test.resolve_start.duration"),
            relative_age_seconds: provider
                .length_histogram("test.relative_age_seconds", vec![1.0, 2.0, 3.0]),
            walk_duration: provider.latency_histogram_ms("test.walk.duration"),
        }
    }

    fn make_request_identifier(
        repository: RepositoryId,
        branch: BranchId,
        number: u64,
    ) -> Request<RevisionListRequest> {
        let mut request = Request::new(RevisionListRequest {
            start: Some(Start::Identifier(model_v1::RevisionIdentifier {
                branch_id: branch.into(),
                number,
            })),
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );
        request
    }

    fn make_request_signature(
        repository: RepositoryId,
        signature: Hash,
    ) -> Request<RevisionListRequest> {
        let mut request = Request::new(RevisionListRequest {
            start: Some(Start::Signature(signature.into())),
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );
        request
    }

    /// Push `count` chained revisions to a freshly-created branch.
    /// Returns `(branch_id, signatures-newest-first)`.
    async fn create_branch_with_history(
        repository: &Arc<RepositoryContext>,
        count: u64,
    ) -> (BranchId, Vec<Hash>) {
        let write_token = get_write_token();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        branch::create(
            repository.clone(),
            &write_token,
            branch_id,
            "test-branch",
            branch::default_category(),
            "creator",
            1,
            vec![],
            false,
            false,
        )
        .await
        .expect("create branch");

        let mut signatures = Vec::with_capacity(count as usize);
        let mut parent = Hash::default();
        for n in 1..=count {
            // The state's metadata blob has to carry `branch` so the
            // forward-cursor lookup can derive it from items[0].
            let mut metadata = Metadata::new();
            metadata.set_branch(branch_id).expect("set branch");
            let metadata_hash = metadata
                .serialize(repository.clone())
                .await
                .expect("serialize metadata");

            let state = Arc::new(State::new());
            state.set_parent_self(parent);
            state.set_revision_number(n);
            state.set_metadata_hash(metadata_hash);
            let serialized = state
                .serialize(repository.clone(), &write_token)
                .await
                .expect("serialize state");
            let pushed = branch_push::push(
                repository.clone(),
                branch_id,
                serialized,
                true,
                true,
                false,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
            )
            .await
            .expect("push revision")
            .revision;
            signatures.push(pushed);
            parent = pushed;
        }
        signatures.reverse();
        (branch_id, signatures)
    }

    /// Serialize a revision without pushing it, for use as the `parent_other`
    /// of a merge. Its revision number is what drags the branch's number up.
    async fn serialize_detached_revision(
        repository: &Arc<RepositoryContext>,
        branch_id: BranchId,
        revision_number: u64,
    ) -> Hash {
        let write_token = get_write_token();
        let mut metadata = Metadata::new();
        metadata.set_branch(branch_id).expect("set branch");
        let metadata_hash = metadata
            .serialize(repository.clone())
            .await
            .expect("serialize metadata");

        let state = Arc::new(State::new());
        state.set_revision_number(revision_number);
        state.set_metadata_hash(metadata_hash);
        state
            .serialize(repository.clone(), &write_token)
            .await
            .expect("serialize detached state")
    }

    /// Push one revision chained onto `parent_self`. `revision_number` is a
    /// hint only — `push` recomputes it from both parents.
    async fn push_chained_revision(
        repository: &Arc<RepositoryContext>,
        branch_id: BranchId,
        parent_self: Hash,
        parent_other: Hash,
        revision_number: u64,
    ) -> (Hash, u64) {
        let write_token = get_write_token();
        let mut metadata = Metadata::new();
        metadata.set_branch(branch_id).expect("set branch");
        let metadata_hash = metadata
            .serialize(repository.clone())
            .await
            .expect("serialize metadata");

        let state = Arc::new(State::new());
        state.set_parent_self(parent_self);
        if !parent_other.is_zero() {
            state.set_parent_other(parent_other);
        }
        state.set_revision_number(revision_number);
        state.set_metadata_hash(metadata_hash);
        let serialized = state
            .serialize(repository.clone(), &write_token)
            .await
            .expect("serialize state");

        let result = branch_push::push(
            repository.clone(),
            branch_id,
            serialized,
            true,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            crate::grpc::server::RevisionListAcceleration::default(),
        )
        .await
        .expect("push revision");
        (result.revision, result.revision_number)
    }

    /// Build a branch whose revision numbers are not contiguous: a linear run
    /// of `linear_before` revisions, then a merge whose `parent_other` is
    /// numbered `jump_other_number` (so the branch number jumps to
    /// `jump_other_number + 1`), then `linear_after` more revisions.
    /// Returns `(branch_id, revision number -> signature)`.
    async fn create_branch_with_jump_history(
        repository: &Arc<RepositoryContext>,
        linear_before: u64,
        jump_other_number: u64,
        linear_after: u64,
    ) -> (BranchId, BTreeMap<u64, Hash>) {
        let write_token = get_write_token();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        branch::create(
            repository.clone(),
            &write_token,
            branch_id,
            "test-branch",
            branch::default_category(),
            "creator",
            1,
            vec![],
            false,
            false,
        )
        .await
        .expect("create branch");

        let mut revisions = BTreeMap::new();
        let mut parent = Hash::default();
        for number in 1..=linear_before {
            let (revision, revision_number) =
                push_chained_revision(repository, branch_id, parent, Hash::default(), number).await;
            revisions.insert(revision_number, revision);
            parent = revision;
        }

        let other = serialize_detached_revision(repository, branch_id, jump_other_number).await;
        let (revision, jumped_number) =
            push_chained_revision(repository, branch_id, parent, other, 0).await;
        revisions.insert(jumped_number, revision);
        parent = revision;

        for offset in 1..=linear_after {
            let (revision, revision_number) = push_chained_revision(
                repository,
                branch_id,
                parent,
                Hash::default(),
                jumped_number + offset,
            )
            .await;
            revisions.insert(revision_number, revision);
            parent = revision;
        }

        (branch_id, revisions)
    }

    #[tokio::test]
    async fn unset_start_returns_invalid_argument() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let mut request = Request::new(RevisionListRequest { start: None });
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );
            let err = handler(
                request,
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect_err("unset start should fail");
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
        }))
        .await;
    }

    #[tokio::test]
    async fn lists_branch_history_via_tip_identifier() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (branch_id, signatures) = create_branch_with_history(&repository_context, 3).await;

            let response = handler(
                make_request_identifier(repository, branch_id, 0),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("Request failed");

            // Strategy header should reflect the direct tip path.
            assert_eq!(
                response
                    .metadata()
                    .get(REVISION_LIST_STRATEGY_HEADER)
                    .map(|v| v.to_str().unwrap()),
                Some("direct"),
            );

            let inner = response.into_inner();
            assert_eq!(inner.items.len(), 3);
            assert_eq!(Hash::from(inner.items[0].signature.as_ref()), signatures[0]);
            assert_eq!(inner.items[0].number, 3);
            assert_eq!(Hash::from(inner.items[2].signature.as_ref()), signatures[2]);
            assert_eq!(inner.items[2].number, 1);
            assert!(inner.signature_forward.is_none());
            assert!(inner.signature_backward.is_none());
        }))
        .await;
    }

    #[tokio::test]
    async fn empty_branch_returns_no_items() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let write_token = get_write_token();
            let branch_id = BranchId::from(uuid::Uuid::now_v7());
            branch::create(
                repository_context,
                &write_token,
                branch_id,
                "empty-branch",
                branch::default_category(),
                "creator",
                1,
                vec![],
                false,
                false,
            )
            .await
            .expect("create empty branch");

            let response = handler(
                make_request_identifier(repository, branch_id, 0),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("Request failed")
            .into_inner();
            // Empty branch resolves tip to zero hash; walk exits with
            // no items, no cursors.
            assert!(response.items.is_empty());
            assert!(response.signature_forward.is_none());
            assert!(response.signature_backward.is_none());
        }))
        .await;
    }

    #[tokio::test]
    async fn pages_via_signature_backward_cursor_segment_aligned() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            // 250 revisions, step=100. Segments 100 and 200 are closed
            // (their +step boundary was crossed by subsequent pushes).
            // Segment 300 is open (rev 250 sits in it).
            let (branch_id, signatures) =
                create_branch_with_history(&repository_context, 250).await;

            // Page 1: tip → rev 250, in open segment 300. Walk is
            // segment-aligned: floor = 201, items 250..201 (50), then
            // current_number=200 < floor, so next_older = rev 200.
            let first_page = handler(
                make_request_identifier(repository, branch_id, 0),
                immutable_store.clone(),
                mutable_store.clone(),
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("first page")
            .into_inner();
            assert_eq!(first_page.items.len(), 50);
            assert_eq!(first_page.items[0].number, 250);
            assert_eq!(first_page.items[49].number, 201);
            let backward = first_page
                .signature_backward
                .clone()
                .expect("backward cursor");
            assert_eq!(Hash::from(backward.as_ref()), signatures[250 - 200]);
            assert!(first_page.signature_forward.is_none());

            // Page 2: anchor = rev 200, in closed segment 200 (cached).
            // Cache serves items 200..101. Rev 201 exists (in the open
            // latest band, since only 250 revisions exist and seg 300's
            // step key isn't registered), so the forward cursor still
            // resolves to it via the latest-anchored fallback.
            let second_page = handler(
                make_request_signature(repository, Hash::from(backward.as_ref())),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("second page");
            assert_eq!(
                second_page
                    .metadata()
                    .get(REVISION_LIST_STRATEGY_HEADER)
                    .map(|v| v.to_str().unwrap()),
                Some("list-cache"),
            );
            let second_page = second_page.into_inner();
            assert_eq!(second_page.items.len(), MAX_REVISION_LIST_RESPONSE_ITEMS);
            assert_eq!(second_page.items[0].number, 200);
            assert_eq!(
                second_page.items[MAX_REVISION_LIST_RESPONSE_ITEMS - 1].number,
                101,
            );
            let forward = second_page.signature_forward.expect("forward cursor");
            assert_eq!(Hash::from(forward.as_ref()), signatures[250 - 201]);
            // Backward cursor: parent of items[N-1] = rev 101 is rev 100,
            // the segment-100 anchor for the next-older page.
            let next_backward = second_page
                .signature_backward
                .clone()
                .expect("backward cursor on second page");
            assert_eq!(Hash::from(next_backward.as_ref()), signatures[250 - 100]);
        }))
        .await;
    }

    #[tokio::test]
    async fn lists_via_by_number_identifier_uses_list_cache_strategy() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (branch_id, signatures) =
                create_branch_with_history(&repository_context, 250).await;

            // Revision 100 sits in the closed segment whose List_100
            // cache entry was populated when revision 101 was pushed.
            let response = handler(
                make_request_identifier(repository, branch_id, 100),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("Request failed");
            assert_eq!(
                response
                    .metadata()
                    .get(REVISION_LIST_STRATEGY_HEADER)
                    .map(|v| v.to_str().unwrap()),
                Some("list-cache"),
            );
            let inner = response.into_inner();
            assert_eq!(inner.items.len(), 100);
            assert_eq!(inner.items[0].number, 100);
            assert_eq!(
                Hash::from(inner.items[0].signature.as_ref()),
                signatures[250 - 100],
            );
            // Cache items carry the serialized state header.
            assert_eq!(
                inner.items[0].state.len(),
                std::mem::size_of::<lore_revision::state::StateData>(),
            );
        }))
        .await;
    }

    #[tokio::test]
    async fn lists_via_signature_anchor() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            // 250 revisions: segment 100 closes when revision 101 is
            // pushed, so List_100 is populated. The signature anchor
            // at revision 100 hits that cache.
            let (_branch, signatures) = create_branch_with_history(&repository_context, 250).await;

            let anchor = signatures[250 - 100];
            let response = handler(
                make_request_signature(repository, anchor),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("Request failed");
            assert_eq!(
                response
                    .metadata()
                    .get(REVISION_LIST_STRATEGY_HEADER)
                    .map(|v| v.to_str().unwrap()),
                Some("list-cache"),
            );
            let inner = response.into_inner();
            assert_eq!(inner.items[0].number, 100);
            // Forward cursor for target=101 walks from the step key at
            // 200 down to 101.
            let forward = inner.signature_forward.expect("forward cursor");
            assert_eq!(Hash::from(forward.as_ref()), signatures[250 - 101]);
            // Backward cursor is None: cached items cover 100..1 and
            // items[N-1] = revision 1's parent is the zero hash.
            assert!(inner.signature_backward.is_none());
            // Cache items carry the serialized state header.
            assert_eq!(
                inner.items[0].state.len(),
                std::mem::size_of::<lore_revision::state::StateData>(),
            );
        }))
        .await;
    }

    /// The forward cursor is served from the `BranchLatestPointer` step key.
    /// Failing that one lookup with backpressure must reach the client as
    /// `RESOURCE_EXHAUSTED`; reporting an absent cursor instead would send
    /// the client back to paging from the branch tip.
    #[tokio::test]
    async fn forward_cursor_slow_down_returns_resource_exhausted() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (branch_id, signatures) =
                create_branch_with_history(&repository_context, 250).await;

            // The cursor for the page anchored at revision 100 targets
            // revision 101, whose step key is the only lookup failed here.
            let (forward_key, _key_type) = branch::revision_step_key(
                repository::SALT_LORE,
                repository,
                branch_id,
                101,
                DEFAULT_HISTORY_STEP_SIZE,
            );
            let throttled = FailingLoadStore::for_key(
                mutable_store,
                forward_key,
                lore_storage::StoreError::from(lore_base::error::SlowDown),
            );

            let status = handler(
                make_request_signature(repository, signatures[250 - 100]),
                immutable_store,
                throttled,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect_err("backpressure must not be reported as an absent cursor");
            assert_eq!(status.code(), tonic::Code::ResourceExhausted);
        }))
        .await;
    }

    /// Wipe the cached `List_100` entry to simulate eviction. The next
    /// identifier-anchored request must rebuild it via the
    /// list-cache-backfill path, and a subsequent request must hit the
    /// fast path.
    #[tokio::test]
    async fn identifier_backfills_when_cache_missing_but_next_skip_exists() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (branch_id, _) = create_branch_with_history(&repository_context, 250).await;

            let (key, key_type) = branch::revision_list_step_key(
                lore_revision::repository::SALT_LORE,
                repository,
                branch_id,
                100,
                DEFAULT_HISTORY_STEP_SIZE,
            );

            // Storing zero deletes the mutable store entry.
            mutable_store
                .clone()
                .store(repository, key, Hash::default(), key_type)
                .await
                .expect("evict cache entry");
            assert!(
                mutable_store
                    .clone()
                    .load(repository, key, key_type)
                    .await
                    .is_err(),
                "cache should be evicted",
            );

            let response = handler(
                make_request_identifier(repository, branch_id, 50),
                immutable_store.clone(),
                mutable_store.clone(),
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("first call");
            assert_eq!(
                response
                    .metadata()
                    .get(REVISION_LIST_STRATEGY_HEADER)
                    .map(|v| v.to_str().unwrap()),
                Some("list-cache-backfill"),
            );

            let response = handler(
                make_request_identifier(repository, branch_id, 50),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("second call");
            assert_eq!(
                response
                    .metadata()
                    .get(REVISION_LIST_STRATEGY_HEADER)
                    .map(|v| v.to_str().unwrap()),
                Some("list-cache"),
            );
        }))
        .await;
    }

    #[tokio::test]
    async fn forward_cursor_resolves_within_open_latest_band_when_no_step_key_covers_target() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            // 50 revisions: only block 0 is populated; no step key
            // ever registered (no boundary crossing happened). Anchor
            // at revision 25 — target 26 has no step key registered,
            // but it still exists in the branch's open latest band, so
            // the forward cursor must resolve to it rather than report
            // no newer page.
            let (_branch, signatures) = create_branch_with_history(&repository_context, 50).await;

            let anchor = signatures[50 - 25];
            let response = handler(
                make_request_signature(repository, anchor),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("Request failed")
            .into_inner();
            assert_eq!(response.items[0].number, 25);
            let forward = response.signature_forward.expect("forward cursor");
            assert_eq!(Hash::from(forward.as_ref()), signatures[50 - 26]);
        }))
        .await;
    }

    #[tokio::test]
    async fn unknown_signature_returns_not_found() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let bogus = Hash::from(random::<[u8; 32]>());
            let err = handler(
                make_request_signature(repository, bogus),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect_err("unknown signature should fail");
            assert_eq!(err.code(), tonic::Code::NotFound);
        }))
        .await;
    }

    #[tokio::test]
    async fn unknown_identifier_returns_not_found() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let unknown_branch = BranchId::from(uuid::Uuid::now_v7());
            let err = handler(
                make_request_identifier(repository, unknown_branch, 0),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect_err("unknown branch should fail");
            assert_eq!(err.code(), tonic::Code::NotFound);
        }))
        .await;
    }

    /// Signature anchor pointing mid-segment must return the full
    /// cached segment (200..101), not just the items from the anchor
    /// down. The anchor is guaranteed to appear in the response per
    /// the relaxed v1 contract.
    #[tokio::test]
    async fn mid_segment_signature_returns_full_cached_segment() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (_branch, signatures) = create_branch_with_history(&repository_context, 250).await;

            // Revision 150 sits mid-way in closed segment 200.
            let anchor = signatures[250 - 150];
            let response = handler(
                make_request_signature(repository, anchor),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("Request failed");
            assert_eq!(
                response
                    .metadata()
                    .get(REVISION_LIST_STRATEGY_HEADER)
                    .map(|v| v.to_str().unwrap()),
                Some("list-cache"),
            );
            let inner = response.into_inner();
            // Full segment served: 200..101 inclusive.
            assert_eq!(inner.items.len(), MAX_REVISION_LIST_RESPONSE_ITEMS);
            assert_eq!(inner.items[0].number, 200);
            assert_eq!(
                inner.items[MAX_REVISION_LIST_RESPONSE_ITEMS - 1].number,
                101,
            );
            // The anchor lives somewhere in the page (not items[0]).
            let anchor_position = inner
                .items
                .iter()
                .position(|item| Hash::from(item.signature.as_ref()) == anchor)
                .expect("anchor must appear in cached page");
            assert_eq!(inner.items[anchor_position].number, 150);
            assert_ne!(anchor_position, 0, "anchor is mid-page, not items[0]");
        }))
        .await;
    }

    /// Evict `List_100` and request `rev_50` by signature. The handler's
    /// signature path must rebuild the segment via backfill (the +step
    /// skip pointer at seg 200 exists, so the segment is backfillable)
    /// and report the `list-cache-backfill` strategy. Subsequent calls
    /// then hit the warm cache.
    #[tokio::test]
    async fn signature_path_backfills_evicted_segment() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (branch_id, signatures) =
                create_branch_with_history(&repository_context, 250).await;

            // Evict the List_100 cache entry.
            let (key, key_type) = branch::revision_list_step_key(
                lore_revision::repository::SALT_LORE,
                repository,
                branch_id,
                100,
                DEFAULT_HISTORY_STEP_SIZE,
            );
            mutable_store
                .clone()
                .store(repository, key, Hash::default(), key_type)
                .await
                .expect("evict cache");

            let anchor = signatures[250 - 50];
            let response = handler(
                make_request_signature(repository, anchor),
                immutable_store.clone(),
                mutable_store.clone(),
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("first call");
            assert_eq!(
                response
                    .metadata()
                    .get(REVISION_LIST_STRATEGY_HEADER)
                    .map(|v| v.to_str().unwrap()),
                Some("list-cache-backfill"),
            );
            // Backfill returned the same page the cache would now hold.
            let inner = response.into_inner();
            assert_eq!(inner.items.len(), MAX_REVISION_LIST_RESPONSE_ITEMS);
            assert_eq!(inner.items[0].number, 100);
            assert_eq!(inner.items[MAX_REVISION_LIST_RESPONSE_ITEMS - 1].number, 1,);

            // Subsequent call: warm cache.
            let response = handler(
                make_request_signature(repository, anchor),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("second call");
            assert_eq!(
                response
                    .metadata()
                    .get(REVISION_LIST_STRATEGY_HEADER)
                    .map(|v| v.to_str().unwrap()),
                Some("list-cache"),
            );
        }))
        .await;
    }

    /// Signature in an open segment (no cache, no backfill possible
    /// because the +step skip pointer doesn't exist yet) must fall
    /// through to the direct walk, and the walk must be segment-aligned.
    #[tokio::test]
    async fn open_segment_signature_walk_is_segment_aligned() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            // 250 revs: segment 300 is open (rev 250 lives there but
            // nothing crosses into seg 400 to register the +step key).
            let (_branch, signatures) = create_branch_with_history(&repository_context, 250).await;

            // Anchor rev 220 — mid-open-segment-300. Floor = 201.
            let anchor = signatures[250 - 220];
            let response = handler(
                make_request_signature(repository, anchor),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("Request failed");
            assert_eq!(
                response
                    .metadata()
                    .get(REVISION_LIST_STRATEGY_HEADER)
                    .map(|v| v.to_str().unwrap()),
                Some("direct"),
            );
            let inner = response.into_inner();
            // Walk segment-aligned: items 220..201 (20 items), not the
            // full 100-item walk.
            assert_eq!(inner.items.len(), 20);
            assert_eq!(inner.items[0].number, 220);
            assert_eq!(inner.items[19].number, 201);
            // Backward = parent_self of rev_201 = rev_200.
            let backward = inner.signature_backward.expect("backward cursor");
            assert_eq!(Hash::from(backward.as_ref()), signatures[250 - 200]);
        }))
        .await;
    }

    /// Stuff the mutable store with a cache blob whose header version
    /// is wrong. The loader must discard it (debug-logged), backfill
    /// rebuilds with the current version, and the strategy is reported
    /// as `list-cache-backfill`.
    #[tokio::test]
    async fn mismatched_cache_version_is_discarded_and_rebuilt() {
        use zerocopy::IntoBytes;

        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (branch_id, _) = create_branch_with_history(&repository_context, 250).await;

            // Overwrite the List_100 entry with a blob whose header
            // carries a future/unknown version. Correct magic, wrong
            // version — exercises the version-mismatch branch of the
            // header check.
            let bogus_header = branch::CachedRevisionListHeader {
                magic: branch::CACHED_REVISION_LIST_MAGIC,
                version: branch::CACHED_REVISION_LIST_VERSION + 99,
            };
            let bogus_item = branch::CachedRevisionItem {
                number: 100,
                signature: Hash::default(),
                metadata: Hash::default(),
                state: lore_revision::state::StateData::default(),
            };
            let mut buffer = bytes::BytesMut::new();
            buffer.extend_from_slice(bogus_header.as_bytes());
            buffer.extend_from_slice([bogus_item].as_bytes());
            let address = lore_revision::immutable::write(
                repository_context.clone(),
                lore_storage::Context::default(),
                buffer.freeze(),
                lore_revision::immutable::write_options_from_repository(repository_context.clone()),
            )
            .await
            .expect("write bogus blob");

            let (key, key_type) = branch::revision_list_step_key(
                lore_revision::repository::SALT_LORE,
                repository,
                branch_id,
                100,
                DEFAULT_HISTORY_STEP_SIZE,
            );
            mutable_store
                .clone()
                .store(repository, key, address.hash, key_type)
                .await
                .expect("install bogus blob");

            // First call must reject the bogus blob and rebuild.
            let response = handler(
                make_request_identifier(repository, branch_id, 50),
                immutable_store.clone(),
                mutable_store.clone(),
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("first call");
            assert_eq!(
                response
                    .metadata()
                    .get(REVISION_LIST_STRATEGY_HEADER)
                    .map(|v| v.to_str().unwrap()),
                Some("list-cache-backfill"),
            );
            let inner = response.into_inner();
            assert_eq!(inner.items.len(), 100);
            assert_eq!(inner.items[0].number, 100);
            assert_eq!(inner.items[99].number, 1);

            // Second call: cache is now rebuilt with the current
            // format, so the fast path takes over.
            let response = handler(
                make_request_identifier(repository, branch_id, 50),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("second call");
            assert_eq!(
                response
                    .metadata()
                    .get(REVISION_LIST_STRATEGY_HEADER)
                    .map(|v| v.to_str().unwrap()),
                Some("list-cache"),
            );
        }))
        .await;
    }

    /// Every response item carries a `state` field that round-trips
    /// back to a `StateData` whose `revision_number` matches the item.
    /// Covers both the cache fast path (item 100) and the walk path
    /// (item 220 in the open segment 300).
    #[tokio::test]
    async fn item_state_round_trips_to_state_data() {
        use zerocopy::FromBytes;

        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (branch_id, signatures) =
                create_branch_with_history(&repository_context, 250).await;

            // Cache fast path: identifier rev 100.
            let response = handler(
                make_request_identifier(repository, branch_id, 100),
                immutable_store.clone(),
                mutable_store.clone(),
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("cache fast path")
            .into_inner();
            for item in &response.items {
                let state = lore_revision::state::StateData::read_from_bytes(item.state.as_ref())
                    .expect("state bytes must round-trip");
                assert_eq!(state.revision_number, item.number);
            }

            // Walk path: signature for rev 220 (open seg 300, no cache).
            let response = handler(
                make_request_signature(repository, signatures[250 - 220]),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("walk path")
            .into_inner();
            assert!(!response.items.is_empty());
            for item in &response.items {
                let state = lore_revision::state::StateData::read_from_bytes(item.state.as_ref())
                    .expect("state bytes must round-trip");
                assert_eq!(state.revision_number, item.number);
            }
        }))
        .await;
    }

    /// With `list_cache = false`, identifier lookups for revisions in
    /// closed segments must NOT serve from cache. The handler falls
    /// through to the step-key path (history-step strategy here, since
    /// `step_keys` is still on).
    #[tokio::test]
    async fn list_cache_disabled_skips_cache() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (branch_id, _) = create_branch_with_history(&repository_context, 250).await;

            let acceleration = crate::grpc::server::RevisionListAcceleration {
                step_keys: true,
                list_cache: false,
            };
            let response = handler(
                make_request_identifier(repository, branch_id, 100),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                acceleration,
                &make_instruments(),
            )
            .await
            .expect("Request failed");
            assert_eq!(
                response
                    .metadata()
                    .get(REVISION_LIST_STRATEGY_HEADER)
                    .map(|v| v.to_str().unwrap()),
                Some("history-step"),
            );
        }))
        .await;
    }

    /// With `step_keys = false` (and cache also off), identifier
    /// lookups fall through to the full-iteration walk.
    #[tokio::test]
    async fn both_disabled_falls_through_to_full_iteration() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (branch_id, _) = create_branch_with_history(&repository_context, 250).await;

            let acceleration = crate::grpc::server::RevisionListAcceleration {
                step_keys: false,
                list_cache: false,
            };
            let response = handler(
                make_request_identifier(repository, branch_id, 100),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                acceleration,
                &make_instruments(),
            )
            .await
            .expect("Request failed");
            assert_eq!(
                response
                    .metadata()
                    .get(REVISION_LIST_STRATEGY_HEADER)
                    .map(|v| v.to_str().unwrap()),
                Some("full-iteration"),
            );
        }))
        .await;
    }

    /// The full-iteration path resolves `branch@number` through
    /// `revision::resolve`, which reads the branch latest first. Throttling that
    /// read must reach the client as `RESOURCE_EXHAUSTED`: reporting it as
    /// `NOT_FOUND` claims a revision does not exist and gives the client no
    /// reason to retry.
    #[tokio::test]
    async fn throttled_branch_latest_resolves_to_resource_exhausted_not_not_found() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (branch_id, _) = create_branch_with_history(&repository_context, 250).await;

            let (latest_key, _key_type) =
                branch::mutable_key(repository::SALT_LORE, branch::LATEST, repository, branch_id);
            let throttled = FailingLoadStore::for_key(
                mutable_store,
                latest_key,
                lore_storage::StoreError::from(lore_base::error::SlowDown),
            );

            // Acceleration off, so the request takes the full-iteration path
            // through revision::resolve rather than a step key or the cache.
            let status = handler(
                make_request_identifier(repository, branch_id, 100),
                immutable_store,
                throttled,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration {
                    step_keys: false,
                    list_cache: false,
                },
                &make_instruments(),
            )
            .await
            .expect_err("a throttled branch latest must not resolve");
            assert_eq!(status.code(), tonic::Code::ResourceExhausted);
        }))
        .await;
    }

    /// With `list_cache = false`, a signature lookup that would
    /// otherwise hit the cache must instead walk directly. The walker
    /// is still segment-aligned.
    #[tokio::test]
    async fn list_cache_disabled_signature_uses_direct_walk() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (_branch, signatures) = create_branch_with_history(&repository_context, 250).await;

            // Anchor rev 150, mid-segment 200. Cached, but we disable.
            let anchor = signatures[250 - 150];
            let acceleration = crate::grpc::server::RevisionListAcceleration {
                step_keys: true,
                list_cache: false,
            };
            let response = handler(
                make_request_signature(repository, anchor),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                acceleration,
                &make_instruments(),
            )
            .await
            .expect("Request failed");
            assert_eq!(
                response
                    .metadata()
                    .get(REVISION_LIST_STRATEGY_HEADER)
                    .map(|v| v.to_str().unwrap()),
                Some("direct"),
            );
            let inner = response.into_inner();
            // Segment-aligned walk: rev 150 down to floor 101.
            assert_eq!(inner.items.len(), 50);
            assert_eq!(inner.items[0].number, 150);
            assert_eq!(inner.items[49].number, 101);
        }))
        .await;
    }

    /// Every revision that exists resolves by number, including on a branch
    /// whose numbering has a gap left by a merge.
    #[tokio::test]
    async fn finds_revision_below_a_jump_that_skipped_a_boundary() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            // 1..=99, then a merge jumping to 105, then 106..=150.
            let (branch_id, revisions) =
                create_branch_with_jump_history(&repository_context, 99, 104, 45).await;
            assert!(revisions.contains_key(&105));
            assert_eq!(revisions.keys().next_back(), Some(&150));
            // 100..=104 were skipped by the jump.
            assert!(!revisions.contains_key(&100));

            for number in [99, 105, 120, 150] {
                let response = handler(
                    make_request_identifier(repository, branch_id, number),
                    immutable_store.clone(),
                    mutable_store.clone(),
                    DEFAULT_HISTORY_STEP_SIZE,
                    crate::grpc::server::RevisionListAcceleration::default(),
                    &make_instruments(),
                )
                .await
                .unwrap_or_else(|err| panic!("revision {number} should resolve: {err}"))
                .into_inner();
                assert_eq!(response.items[0].number, number);
                assert_eq!(
                    Hash::from(response.items[0].signature.as_ref()),
                    revisions[&number],
                );
            }
        }))
        .await;
    }

    /// With several boundaries skipped in one jump, each sealed boundary
    /// answers with the highest revision at or below it, so lookups inside
    /// the pre-jump segment still resolve. A request served from the list
    /// cache returns that whole segment, so the requested revision is located
    /// within the items rather than assumed to head them.
    #[tokio::test]
    async fn finds_revisions_below_a_multi_boundary_jump() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            // 1..=150, then a merge jumping to 400 (skipping boundaries 200
            // and 300), then 401..=405.
            let (branch_id, revisions) =
                create_branch_with_jump_history(&repository_context, 150, 399, 5).await;
            assert!(revisions.contains_key(&400));

            for number in [101, 120, 150, 400, 405] {
                let response = handler(
                    make_request_identifier(repository, branch_id, number),
                    immutable_store.clone(),
                    mutable_store.clone(),
                    DEFAULT_HISTORY_STEP_SIZE,
                    crate::grpc::server::RevisionListAcceleration::default(),
                    &make_instruments(),
                )
                .await
                .unwrap_or_else(|err| panic!("revision {number} should resolve: {err}"))
                .into_inner();
                let item = response
                    .items
                    .iter()
                    .find(|item| item.number == number)
                    .unwrap_or_else(|| panic!("revision {number} missing from response"));
                assert_eq!(Hash::from(item.signature.as_ref()), revisions[&number]);
            }
        }))
        .await;
    }

    /// A revision number a merge skipped does not exist. A sealed boundary
    /// whose anchor is numbered below the request proves that absence.
    #[tokio::test]
    async fn revision_number_skipped_by_a_jump_is_not_found() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (branch_id, revisions) =
                create_branch_with_jump_history(&repository_context, 150, 399, 5).await;
            assert!(!revisions.contains_key(&250));

            let err = handler(
                make_request_identifier(repository, branch_id, 250),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect_err("skipped revision number should not resolve");
            assert_eq!(err.code(), tonic::Code::NotFound);
        }))
        .await;
    }

    /// The segment holding the branch head stays open until the head moves
    /// past it, so a lookup inside it resolves by iteration.
    #[tokio::test]
    async fn jump_does_not_seal_the_segment_it_landed_in() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            // Head lands at 105 and stops, leaving segment 200 open.
            let (branch_id, revisions) =
                create_branch_with_jump_history(&repository_context, 99, 104, 0).await;

            let (key, key_type) = branch::revision_step_key(
                lore_revision::repository::SALT_LORE,
                repository,
                branch_id,
                200,
                DEFAULT_HISTORY_STEP_SIZE,
            );
            let err = mutable_store
                .clone()
                .load(repository, key, key_type)
                .await
                .expect_err("segment 200 holds the head and must not be sealed");
            assert!(
                matches!(err, StoreError::AddressNotFound(_)),
                "an unsealed boundary must read as missing, got {err:?}",
            );

            let (crossed_key, crossed_key_type) = branch::revision_step_key(
                lore_revision::repository::SALT_LORE,
                repository,
                branch_id,
                100,
                DEFAULT_HISTORY_STEP_SIZE,
            );
            let sealed = mutable_store
                .load(repository, crossed_key, crossed_key_type)
                .await
                .expect("boundary 100 was crossed and must be sealed");
            assert_eq!(
                sealed, revisions[&99],
                "boundary 100 holds the highest revision numbered at or below it",
            );
        }))
        .await;
    }

    /// The forward cursor resolves to the real next revision across a
    /// boundary the branch's numbering skipped, rather than assuming
    /// `items[0].number + 1` exists.
    #[tokio::test]
    async fn forward_cursor_finds_the_real_revision_across_a_jump() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            // 1..=150, then a merge jumping to 400, then 401..=405 —
            // the last five pushes seal boundary 400, so the target
            // sits behind a sealed skip pointer rather than the open
            // latest band.
            let (branch_id, revisions) =
                create_branch_with_jump_history(&repository_context, 150, 399, 5).await;
            assert!(revisions.contains_key(&400));

            let response = handler(
                make_request_identifier(repository, branch_id, 150),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("Request failed")
            .into_inner();
            assert_eq!(response.items[0].number, 150);
            let forward = response
                .signature_forward
                .expect("forward cursor across the jump");
            assert_eq!(Hash::from(forward.as_ref()), revisions[&400]);
        }))
        .await;
    }

    /// Several consecutive empty step boundaries above the current page
    /// must all be skipped, and the real target beyond them still found.
    #[tokio::test]
    async fn forward_cursor_skips_several_empty_bands_to_find_the_target() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            // 1..=150, then a merge jumping to 700 — skipping the empty
            // boundaries 200, 300, 400, 500 and 600 — then 701..=705 to
            // seal boundary 700.
            let (branch_id, revisions) =
                create_branch_with_jump_history(&repository_context, 150, 699, 5).await;
            assert!(revisions.contains_key(&700));

            let response = handler(
                make_request_identifier(repository, branch_id, 150),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("Request failed")
            .into_inner();
            assert_eq!(response.items[0].number, 150);
            let forward = response
                .signature_forward
                .expect("forward cursor past five empty bands");
            assert_eq!(Hash::from(forward.as_ref()), revisions[&700]);
        }))
        .await;
    }

    /// A jump wide enough to leave dozens of consecutive sealed boundaries
    /// pointing at the same pre-jump revision must still resolve in a
    /// bounded number of probes: the binary search finds the real target's
    /// boundary directly rather than degrading into a walk proportional
    /// to the branch's history since the jump.
    #[tokio::test]
    async fn forward_cursor_resolves_a_jump_spanning_many_boundaries() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            // 1..=150, then a merge jumping to 5100 — sealing 48
            // consecutive empty boundaries (200..=5000) at the pre-jump
            // revision — then 5101..=5110 to seal boundary 5100 itself.
            let (branch_id, revisions) =
                create_branch_with_jump_history(&repository_context, 150, 5099, 10).await;
            assert!(revisions.contains_key(&5100));

            let response = handler(
                make_request_identifier(repository, branch_id, 150),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("Request failed")
            .into_inner();
            assert_eq!(response.items[0].number, 150);
            let forward = response
                .signature_forward
                .expect("forward cursor across a wide jump");
            assert_eq!(Hash::from(forward.as_ref()), revisions[&5100]);
        }))
        .await;
    }

    /// Repeatedly following `signature_forward` from a page anchored well
    /// before a jump must reach the branch's latest revision, and the
    /// union of every page visited along the way must cover every
    /// revision that exists — proving the cursor never skips a real
    /// revision on its way there.
    #[tokio::test]
    async fn paging_forward_via_signature_forward_reaches_every_revision() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            // 1..=150, then a merge jumping to 400, then 401..=405.
            let (branch_id, revisions) =
                create_branch_with_jump_history(&repository_context, 150, 399, 5).await;

            let mut seen: BTreeSet<u64> = BTreeSet::new();
            let mut request = make_request_identifier(repository, branch_id, 1);
            let mut reached_latest = false;

            // Bounded generously above the number of pages this fixture
            // can produce; a bug that stalls forward progress should fail
            // loudly here rather than hang the test.
            for _ in 0..30 {
                let response = handler(
                    request,
                    immutable_store.clone(),
                    mutable_store.clone(),
                    DEFAULT_HISTORY_STEP_SIZE,
                    crate::grpc::server::RevisionListAcceleration::default(),
                    &make_instruments(),
                )
                .await
                .expect("paginated request failed")
                .into_inner();

                seen.extend(response.items.iter().map(|item| item.number));

                let Some(forward) = response.signature_forward else {
                    reached_latest = response.items.iter().any(|item| item.number == 405);
                    break;
                };
                request = make_request_signature(repository, Hash::from(forward.as_ref()));
            }

            assert!(
                reached_latest,
                "paging forward never reached the branch's latest revision"
            );
            let expected: BTreeSet<u64> = revisions.keys().copied().collect();
            assert_eq!(seen, expected, "paging forward skipped some revisions");
        }))
        .await;
    }

    /// Evicting the anchor band's cached list — but not its skip pointer
    /// — must not cause the forward cursor to skip past revisions that
    /// exist. It falls back to walking the band directly instead.
    #[tokio::test]
    async fn forward_cursor_survives_an_evicted_anchor_list_cache() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (branch_id, signatures) =
                create_branch_with_history(&repository_context, 250).await;

            // Evict segment 200's cached list — the anchor band above
            // revision 100 — leaving its skip pointer intact.
            let (key, key_type) = branch::revision_list_step_key(
                lore_revision::repository::SALT_LORE,
                repository,
                branch_id,
                200,
                DEFAULT_HISTORY_STEP_SIZE,
            );
            mutable_store
                .clone()
                .store(repository, key, Hash::default(), key_type)
                .await
                .expect("evict anchor list cache");

            let response = handler(
                make_request_identifier(repository, branch_id, 100),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("Request failed")
            .into_inner();
            assert_eq!(response.items[0].number, 100);
            let forward = response
                .signature_forward
                .expect("forward cursor via band walk");
            assert_eq!(Hash::from(forward.as_ref()), signatures[250 - 101]);
        }))
        .await;
    }

    /// A step boundary can go missing below a boundary that is still
    /// present — the seal write is best-effort, and the key type has been
    /// renamed once already, orphaning older entries. The forward cursor
    /// must still find the real successor rather than treat the missing
    /// boundary as proof its band is empty and anchor on a boundary
    /// further up, which would silently skip every revision in between.
    /// The missing pointer is also repaired as a side effect, so a
    /// second call resolves the fast path directly rather than repeating
    /// the gap descent.
    #[tokio::test]
    async fn forward_cursor_repairs_a_missing_skip_pointer_below_a_found_anchor() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (branch_id, signatures) =
                create_branch_with_history(&repository_context, 350).await;

            // Evict ONLY boundary 200's skip pointer (REVISION_NUMBER_STEP),
            // leaving its list cache and boundary 300's skip pointer intact.
            let (key, key_type) = branch::revision_step_key(
                lore_revision::repository::SALT_LORE,
                repository,
                branch_id,
                200,
                DEFAULT_HISTORY_STEP_SIZE,
            );
            mutable_store
                .clone()
                .store(repository, key, Hash::default(), key_type)
                .await
                .expect("evict boundary 200 skip pointer");

            let response = handler(
                make_request_identifier(repository, branch_id, 100),
                immutable_store,
                mutable_store.clone(),
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect("Request failed")
            .into_inner();
            assert_eq!(response.items[0].number, 100);
            let forward = response
                .signature_forward
                .expect("forward cursor across the missing pointer");
            assert_eq!(Hash::from(forward.as_ref()), signatures[350 - 101]);

            // The gap descent repaired boundary 200's skip pointer: it now
            // points at revision 200, the highest revision at or below it.
            let repaired = mutable_store
                .load(repository, key, key_type)
                .await
                .expect("boundary 200 should be repaired");
            assert_eq!(repaired, signatures[350 - 200]);
        }))
        .await;
    }

    /// When the missing boundary is the very first one `forward_anchor`
    /// probes (not one it only reaches after a fast binary-search hit),
    /// the gap descent walks the full, uncapped distance down to
    /// `first_number` and — since that distance is exactly what it walks
    /// — necessarily crosses and repairs that same boundary. A second,
    /// identical request then takes the fast path directly, for both the
    /// page resolution and the forward cursor: the first call's cost
    /// heals the exact spot future requests need, not just spots nearer
    /// the branch's latest revision.
    #[tokio::test]
    async fn forward_cursor_repairs_the_boundary_nearest_first_number() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (branch_id, signatures) =
                create_branch_with_history(&repository_context, 250).await;

            // Evict boundary 100 — the very first boundary `forward_anchor`
            // would probe for a page anchored at revision 50.
            let (key, key_type) = branch::revision_step_key(
                lore_revision::repository::SALT_LORE,
                repository,
                branch_id,
                100,
                DEFAULT_HISTORY_STEP_SIZE,
            );
            mutable_store
                .clone()
                .store(repository, key, Hash::default(), key_type)
                .await
                .expect("evict boundary 100 skip pointer");

            // Disable the list cache so the page resolves to exactly
            // revision 50 (not the whole cached segment headed at 100),
            // landing `first_number` well below the evicted boundary.
            let acceleration = crate::grpc::server::RevisionListAcceleration {
                step_keys: true,
                list_cache: false,
            };

            let first_response = handler(
                make_request_identifier(repository, branch_id, 50),
                immutable_store.clone(),
                mutable_store.clone(),
                DEFAULT_HISTORY_STEP_SIZE,
                acceleration,
                &make_instruments(),
            )
            .await
            .expect("first request failed");
            assert_eq!(
                first_response
                    .metadata()
                    .get(REVISION_LIST_STRATEGY_HEADER)
                    .map(|v| v.to_str().unwrap()),
                Some("full-iteration"),
                "no acceleration is usable yet for the first call",
            );
            let first_response = first_response.into_inner();
            assert_eq!(first_response.items[0].number, 50);
            let forward = first_response
                .signature_forward
                .expect("forward cursor via the full, uncapped gap descent");
            assert_eq!(Hash::from(forward.as_ref()), signatures[250 - 51]);

            // The descent crossed boundary 100 on its way down and
            // repaired it — the boundary nearest `first_number`, not just
            // ones nearer the branch's latest revision.
            let repaired = mutable_store
                .clone()
                .load(repository, key, key_type)
                .await
                .expect("boundary 100 should be repaired");
            assert_eq!(repaired, signatures[250 - 100]);

            // An identical second request now takes the fast path for
            // both the page and the forward cursor, with no gap descent
            // needed at all.
            let second_response = handler(
                make_request_identifier(repository, branch_id, 50),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                acceleration,
                &make_instruments(),
            )
            .await
            .expect("second request failed");
            assert_eq!(
                second_response
                    .metadata()
                    .get(REVISION_LIST_STRATEGY_HEADER)
                    .map(|v| v.to_str().unwrap()),
                Some("history-step"),
                "the repaired boundary now serves the page directly",
            );
            let second_response = second_response.into_inner();
            assert_eq!(second_response.items[0].number, 50);
            let forward = second_response
                .signature_forward
                .expect("forward cursor via the now-repaired boundary");
            assert_eq!(Hash::from(forward.as_ref()), signatures[250 - 51]);
        }))
        .await;
    }

    /// `MutableStore` wrapper that fails `load` for one specific key with
    /// a non-retryable, non-`AddressNotFound` error, delegating
    /// everything else to `inner`. Used to exercise the forward cursor's
    /// error path, which a real store failure should reach unmasked.
    struct FailingMutableStore {
        inner: Arc<dyn lore_storage::MutableStore>,
        fail_key: Hash,
    }

    #[async_trait::async_trait]
    impl lore_storage::MutableStore for FailingMutableStore {
        async fn load(
            self: Arc<Self>,
            partition: lore_storage::Partition,
            key: Hash,
            key_type: lore_storage::KeyType,
        ) -> Result<Hash, lore_storage::StoreError> {
            if key == self.fail_key {
                return Err(lore_storage::StoreError::from(
                    lore_storage::errors::Maintenance,
                ));
            }
            self.inner.clone().load(partition, key, key_type).await
        }

        async fn store(
            self: Arc<Self>,
            partition: lore_storage::Partition,
            key: Hash,
            value: Hash,
            key_type: lore_storage::KeyType,
        ) -> Result<(), lore_storage::StoreError> {
            self.inner
                .clone()
                .store(partition, key, value, key_type)
                .await
        }

        async fn compare_and_swap(
            self: Arc<Self>,
            partition: lore_storage::Partition,
            key: Hash,
            expected: Hash,
            value: Hash,
            key_type: lore_storage::KeyType,
        ) -> Result<Hash, lore_storage::StoreError> {
            self.inner
                .clone()
                .compare_and_swap(partition, key, expected, value, key_type)
                .await
        }

        async fn list(
            self: Arc<Self>,
            partition: lore_storage::Partition,
            key_type: lore_storage::KeyType,
        ) -> Result<lore_storage::KeyValueStream, lore_storage::StoreError> {
            self.inner.clone().list(partition, key_type).await
        }

        async fn flush(self: Arc<Self>, sync_data: bool) -> Result<(), lore_storage::StoreError> {
            self.inner.clone().flush(sync_data).await
        }
    }

    /// A genuine store failure while probing for the forward cursor must
    /// surface as an error, not silently collapse into "no newer page".
    #[tokio::test]
    async fn forward_cursor_propagates_a_genuine_store_failure() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (branch_id, _) = create_branch_with_history(&repository_context, 250).await;

            // Fail the probe for the anchor band above revision 100
            // (boundary 200) with something other than `AddressNotFound`
            // or `SlowDown`.
            let (fail_key, _) = branch::revision_step_key(
                lore_revision::repository::SALT_LORE,
                repository,
                branch_id,
                200,
                DEFAULT_HISTORY_STEP_SIZE,
            );
            let failing_store: Arc<dyn lore_storage::MutableStore> =
                Arc::new(FailingMutableStore {
                    inner: mutable_store,
                    fail_key,
                });

            let err = handler(
                make_request_identifier(repository, branch_id, 100),
                immutable_store,
                failing_store,
                DEFAULT_HISTORY_STEP_SIZE,
                crate::grpc::server::RevisionListAcceleration::default(),
                &make_instruments(),
            )
            .await
            .expect_err("a genuine store failure must surface, not become a missing cursor");
            assert_eq!(err.code(), tonic::Code::Internal);
        }))
        .await;
    }

    /// With `step_keys` disabled, the forward cursor must never read the
    /// skip pointer at all — not just tolerate it being absent. Corrupts
    /// the pointer with a value that would produce a visibly wrong
    /// answer if read, and confirms the real answer comes back anyway,
    /// via the same walk `resolve_start` falls back to for the main page
    /// when step keys are off.
    #[tokio::test]
    async fn forward_cursor_step_keys_disabled_never_reads_a_wrong_skip_pointer() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (branch_id, signatures) =
                create_branch_with_history(&repository_context, 250).await;

            // Corrupt boundary 100's skip pointer to point at revision 1
            // instead of revision 100. If this is read despite step_keys
            // being disabled, the forward cursor would resolve to
            // something other than revision 51.
            let (key, key_type) = branch::revision_step_key(
                lore_revision::repository::SALT_LORE,
                repository,
                branch_id,
                100,
                DEFAULT_HISTORY_STEP_SIZE,
            );
            mutable_store
                .clone()
                .store(repository, key, signatures[250 - 1], key_type)
                .await
                .expect("corrupt boundary 100 skip pointer");

            let acceleration = crate::grpc::server::RevisionListAcceleration {
                step_keys: false,
                list_cache: false,
            };
            let response = handler(
                make_request_identifier(repository, branch_id, 50),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                acceleration,
                &make_instruments(),
            )
            .await
            .expect("Request failed")
            .into_inner();
            assert_eq!(response.items[0].number, 50);
            let forward = response
                .signature_forward
                .expect("forward cursor via the uncapped walk");
            assert_eq!(Hash::from(forward.as_ref()), signatures[250 - 51]);
        }))
        .await;
    }

    /// With `list_cache` disabled, the forward cursor must never read the
    /// cached segment list for a sealed anchor — not just tolerate it
    /// being missing or malformed. Plants a well-formed but wrong cached
    /// list at the anchor's boundary and confirms the real answer, from
    /// the direct walk, comes back instead.
    #[tokio::test]
    async fn forward_cursor_list_cache_disabled_never_reads_a_wrong_cached_list() {
        use zerocopy::IntoBytes;

        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository_context = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository,
            ));
            let (branch_id, signatures) =
                create_branch_with_history(&repository_context, 250).await;

            // Overwrite segment 200's cached list with a well-formed blob
            // whose sole item is a fabricated revision numbered far above
            // anything real. If this is read despite list_cache being
            // disabled, the forward cursor would resolve to that bogus
            // signature instead of the real revision 101.
            let bogus_header = branch::CachedRevisionListHeader {
                magic: branch::CACHED_REVISION_LIST_MAGIC,
                version: branch::CACHED_REVISION_LIST_VERSION,
            };
            let bogus_item = branch::CachedRevisionItem {
                number: 9999,
                signature: Hash::from(random::<[u8; 32]>()),
                metadata: Hash::default(),
                state: lore_revision::state::StateData::default(),
            };
            let mut buffer = bytes::BytesMut::new();
            buffer.extend_from_slice(bogus_header.as_bytes());
            buffer.extend_from_slice([bogus_item].as_bytes());
            let address = lore_revision::immutable::write(
                repository_context.clone(),
                lore_storage::Context::default(),
                buffer.freeze(),
                lore_revision::immutable::write_options_from_repository(repository_context.clone()),
            )
            .await
            .expect("write bogus blob");
            let (key, key_type) = branch::revision_list_step_key(
                lore_revision::repository::SALT_LORE,
                repository,
                branch_id,
                200,
                DEFAULT_HISTORY_STEP_SIZE,
            );
            mutable_store
                .clone()
                .store(repository, key, address.hash, key_type)
                .await
                .expect("install bogus cached list");

            let acceleration = crate::grpc::server::RevisionListAcceleration {
                step_keys: true,
                list_cache: false,
            };
            let response = handler(
                make_request_identifier(repository, branch_id, 100),
                immutable_store,
                mutable_store,
                DEFAULT_HISTORY_STEP_SIZE,
                acceleration,
                &make_instruments(),
            )
            .await
            .expect("Request failed")
            .into_inner();
            assert_eq!(response.items[0].number, 100);
            let forward = response
                .signature_forward
                .expect("forward cursor via the direct walk");
            assert_eq!(Hash::from(forward.as_ref()), signatures[250 - 101]);
        }))
        .await;
    }
}
