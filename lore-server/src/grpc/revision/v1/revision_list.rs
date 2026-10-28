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
use lore_revision::metadata::BRANCH;
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
use lore_telemetry::tracing::fields::BRANCH_ID;
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
use crate::grpc::revision::v1::branch_lineage::BranchLineage;
use crate::grpc::revision::v1::branch_lineage::HistoryOwner;
use crate::grpc::revision::v1::service::RevisionListInstruments;
use crate::grpc::warn_error_to_status;
use crate::util::setup_execution;

#[lore_macro::test_pub]
const MAX_REVISION_LIST_RESPONSE_ITEMS: usize = 100;
const METRICS_START_KEY: &str = "start_type";
const METRICS_LIST_STRATEGY_KEY: &str = "list_strategy";
/// Revisions walked between progress reports from the forward-cursor gap
/// descent. The descent reports its totals when it finishes, and the
/// handler timeout can end it before it ever gets there, so a descent long
/// enough to be worth diagnosing reports as it goes.
const GAP_DESCENT_PROGRESS_HOPS: u64 = 10_000;

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
/// Returns a page of revisions newer-to-older that holds the `start`
/// anchor, plus optional cursors for the adjacent pages. A walked page
/// starts at the anchor; a page served from a cached segment starts at
/// the segment's top, so it also holds revisions newer than the anchor.
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
            // A signature names a revision and not a branch to view it from, so
            // only an identifier start has a lineage.
            let lineage = match &start {
                Start::Identifier(identifier) => {
                    Some(BranchLineage::new(BranchId::from(&identifier.branch_id)))
                }
                Start::Signature(_) => None,
            };

            let resolved = {
                let labels = smallvec![KeyValue::new(
                    METRICS_START_KEY,
                    start_to_metric_value(&start),
                )];
                resolve_start(
                    start,
                    lineage.as_ref(),
                    &repository,
                    history_step_size,
                    acceleration,
                )
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
                    debug!({REVISION} = %start, %strategy, "Walking revisions");
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

            debug!(walked_items = walked.items.len(), %strategy, "Finished walk");

            let signature_forward = forward_cursor(
                &repository,
                &walked,
                lineage.as_ref(),
                history_step_size,
                acceleration,
            )
            .await?;
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
    lineage: Option<&BranchLineage>,
    repository: &Arc<RepositoryContext>,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
) -> Result<ResolveStart, Status> {
    match start {
        Start::Signature(signature) => {
            let hash = crate::grpc::revision_signature(signature)?;
            debug!({REVISION} = %hash, "resolve_start - Signature");
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
            debug!({BRANCH_ID} = %branch, revision_number = identifier.number, "resolve_start - Identifier");
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

            let requested = HistoryOwner::requested(branch);
            if let Some(accelerated) = accelerated_start(
                repository,
                &requested,
                identifier.number,
                history_step_size,
                acceleration,
            )
            .await?
            {
                return Ok(accelerated);
            }

            // The requested branch's own data had nothing, so the number may
            // be at or below a branch point and described by an ancestor.
            if let Some(lineage) = lineage
                && let Some(owner) = lineage.owner_of(repository, identifier.number).await?
                && owner.branch != branch
                && let Some(accelerated) = accelerated_start(
                    repository,
                    &owner,
                    identifier.number,
                    history_step_size,
                    acceleration,
                )
                .await?
            {
                return Ok(accelerated);
            }

            let signature = format!("{branch}@{}", identifier.number);
            let hash = revision::resolve_boxed(
                repository.clone(),
                signature,
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

/// Builds the page for a cached segment of `owner`.
fn cached_start(
    owner: &HistoryOwner,
    segment: &[branch::CachedRevisionItem],
    strategy: RevisionListStrategy,
) -> ResolveStart {
    // The segment is the owner's and can run past the branch point: a branch
    // made at 150 does not contain the owner's 151..=200.
    let visible = owner.visible(segment);
    ResolveStart::Items {
        items: cached_to_proto(visible),
        branch: owner.branch,
        next_older: cached_next_older(visible),
        strategy,
    }
}

/// Serves an identifier lookup from the acceleration data of `owner`, or
/// `None` when that branch has none covering the number.
async fn accelerated_start(
    repository: &Arc<RepositoryContext>,
    owner: &HistoryOwner,
    number: u64,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
) -> Result<Option<ResolveStart>, Status> {
    if acceleration.list_cache {
        if let Some(cached) =
            cache::revision::load_cached_list(repository, owner.branch, number, history_step_size)
                .await
                .filter_slow_down()?
                .unwrap_or_default()
            && cached.items().iter().any(|item| item.number == number)
        {
            debug!(number, "Served revision list from cache");
            return Ok(Some(cached_start(
                owner,
                cached.items(),
                RevisionListStrategy::ListCache,
            )));
        }

        if let Some(cached) = cache::revision::try_backfill_segment(
            repository,
            owner.branch,
            number,
            history_step_size,
        )
        .await
        .filter_slow_down()?
        .unwrap_or_default()
            && cached.items().iter().any(|item| item.number == number)
        {
            debug!(number, "Backfilled revision list cache");
            return Ok(Some(cached_start(
                owner,
                cached.items(),
                RevisionListStrategy::ListCacheBackfill,
            )));
        }
    }

    if !acceleration.step_keys {
        return Ok(None);
    }
    let step_key_hit =
        cache::revision::resolve_via_step_key(repository, owner.branch, number, history_step_size)
            .await
            .filter_slow_down()?
            .unwrap_or_default();
    Ok(step_key_hit.map(|hash| ResolveStart::Walk {
        start: hash,
        strategy: RevisionListStrategy::HistoryStep,
    }))
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
/// orphaning older entries under the previous name. Only `Empty` may
/// narrow a search towards a single-band answer: treating `Unknown` as
/// `Empty` would let that answer span a boundary that is genuinely
/// non-empty but whose pointer was simply lost.
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
    /// No boundary above `first_number` is sealed at all: the target, if
    /// any, lives in the branch's latest revision's own (always-open)
    /// band, likewise bounded to `history_step_size` revisions. The latest
    /// revision is not numbered above `first_number` when the branch does
    /// not reach the revision numbered `first_number`.
    LatestBand { anchor: Hash },
    /// A boundary somewhere in the search could not be read, so `Empty`'s
    /// monotonicity no longer covers the boundaries below it and the gap
    /// between `first_number` and `anchor` cannot be trusted to hold a
    /// single band. `anchor` is the revision held by the lowest boundary
    /// the search went on to prove `Found`, which is not necessarily the
    /// lowest sealed boundary there is: narrowing past an unreadable
    /// boundary skips whatever lies beneath it. Where the search proved
    /// none, `anchor` is the branch's latest revision, numbered above
    /// `first_number` unless the branch does not reach that revision. The
    /// descent walks every revision between the two, so how low `anchor`
    /// sits sets the cost.
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
/// Hitting `Unknown` — the boundary's true state cannot be determined —
/// rules out both single-band answers for the rest of the search, since
/// `Empty`'s monotonicity no longer covers the boundaries below it. The
/// search continues upward regardless and reports
/// [`ForwardAnchor::UnverifiedGap`] anchored on the lowest boundary it
/// goes on to prove `Found`, or on `latest` when it proves none.
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

    let mut saw_unreadable_boundary = false;

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
        // Records the gap without ending the search: a `Found` boundary
        // above this one is still a far lower descent anchor than `latest`.
        BoundaryProbe::Unknown => saw_unreadable_boundary = true,
    }

    let latest_state = state::State::deserialize(repository.clone(), latest)
        .await
        .filter_slow_down()?
        .map_err(|err| warn_error_to_status(&err, |e| Status::internal(e.to_string())))?;
    let latest_boundary =
        latest_state.revision_number().div_ceil(history_step_size) * history_step_size;

    if latest_boundary <= first_boundary {
        // `latest` is at or below `first_boundary`, itself at most one step
        // above `first_number`: any target sits in latest's band whatever the
        // probe above reported.
        return Ok(ForwardAnchor::LatestBand { anchor: latest });
    }

    // Invariant: `low` is a boundary proven `Empty`, or one that could
    // not be read; `high` is either
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
            // An unreadable boundary leaves the half below `mid`
            // unexcluded, so narrowing upward can only overshoot the
            // lowest `Found` boundary, never settle on a bad anchor:
            // every `Found` is proven above `first_number` on its own,
            // however the search reached it.
            BoundaryProbe::Unknown => {
                saw_unreadable_boundary = true;
                low = mid;
            }
        }
    }

    match (saw_unreadable_boundary, high_anchor) {
        // Both single-band answers rest on `Empty` holding for every
        // boundary at or below `low`, which one unreadable boundary
        // anywhere in the search breaks.
        (true, high_anchor) => Ok(ForwardAnchor::UnverifiedGap {
            anchor: high_anchor.unwrap_or(latest),
        }),
        (false, Some(revision)) if high < latest_boundary => Ok(ForwardAnchor::Sealed {
            anchor: revision,
            boundary: high,
        }),
        (false, _) => Ok(ForwardAnchor::LatestBand { anchor: latest }),
    }
}

/// Descends from a [`ForwardAnchor::Sealed`] or [`ForwardAnchor::LatestBand`]
/// anchor to the lowest revision numbered strictly above `first_number`, or
/// `None` when the anchor is not numbered above it, as the branch's latest
/// revision is not for a revision the branch does not reach.
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
) -> Result<Option<Hash>, Status> {
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
        return Ok(Some(item.signature));
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
        .map(|item| item.signature))
}

/// Walks `parent_self` from `anchor` down to the lowest revision numbered
/// strictly above `first_number`, without assuming the gap fits in one
/// band, or `None` when `anchor` is not numbered above it. When `step_keys` acceleration is enabled, writes the
/// step-boundary skip pointer for every boundary the walk crosses, so a
/// request that pays this walk's cost once leaves the fast path in place
/// for later requests instead of leaving every subsequent lookup to
/// rediscover the same gap.
///
/// Deliberately uncapped by hop count: revision numbers strictly decrease
/// along `parent_self`, so this always terminates at `first_number` or the
/// root, bounded only by the branch's real history depth — the same
/// guarantee `resolve_start`'s `FullIteration` fallback already relies on
/// via `revision::resolve` when no acceleration is
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
) -> Result<Option<Hash>, Status> {
    let mut hash = anchor;
    let mut prev_state: Option<Arc<state::State>> = None;
    // Invariant: `last_above` is the most recently visited hash whose
    // number was confirmed `> first_number`, if any.
    let mut last_above = None;
    let mut hops: u64 = 0;
    let mut sealed: u64 = 0;

    loop {
        let current_state = state::State::deserialize(repository.clone(), hash)
            .await
            .filter_slow_down()?
            .map_err(|err| warn_error_to_status(&err, |e| Status::internal(e.to_string())))?;
        let number = current_state.revision_number();

        hops += 1;
        if hops.is_multiple_of(GAP_DESCENT_PROGRESS_HOPS) {
            debug!(
                {BRANCH} = %branch, hops, number, first_number, sealed,
                "Forward-cursor gap descent still walking",
            );
        }

        if step_keys_enabled
            && let Some(previous_state) = &prev_state
            && let Some((lowest_b, highest_b)) = cache::revision::sealed_boundaries(
                number,
                previous_state.revision_number(),
                history_step_size,
            )
        {
            for boundary in (lowest_b..=highest_b).step_by(history_step_size as usize) {
                // Counts what reached the store, not what was attempted:
                // a descent reporting repairs it did not make reads as a
                // fast path restored, when the next request will walk the
                // same distance again.
                if cache::revision::seal_boundary_revision_number(
                    repository.clone(),
                    branch,
                    history_step_size,
                    boundary,
                    &current_state,
                    previous_state,
                )
                .await
                .is_ok()
                {
                    sealed += 1;
                }
            }
        }

        if number <= first_number {
            debug!(
                {BRANCH} = %branch, %anchor, first_number, hops, sealed,
                "Forward-cursor gap descent complete",
            );
            return Ok(last_above);
        }
        last_above = Some(hash);
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
/// numbering gaps). Returns `Ok(None)` when no revision of the branch is
/// numbered above items[0]: items[0] is the branch's latest revision, or
/// one numbered above it that the branch does not reach, such as a
/// revision a client stored but has not pushed.
async fn forward_cursor(
    repository: &Arc<RepositoryContext>,
    walked: &Walked,
    lineage: Option<&BranchLineage>,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
) -> Result<Option<Hash>, Status> {
    let Some(first) = walked.items.first() else {
        return Ok(None);
    };
    let Some(page_branch) = walked.branch else {
        return Ok(None);
    };
    let first_number = first.number;
    // A page created on the requested branch has its successor there too. A
    // page from an ancestor ends at a branch point, whose successor sits on
    // whichever branch holds the next number in the requested branch's history.
    let owner = match lineage {
        Some(lineage) if lineage.branch() != page_branch => {
            lineage
                .owner_of(repository, first_number.saturating_add(1))
                .await?
        }
        _ => None,
    };
    let branch = owner.map_or(page_branch, |owner| owner.branch);
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
        debug!({BRANCH} = %branch, first_number, "forward_cursor - descend_unverified_gap");
        return descend_unverified_gap(
            repository,
            branch,
            latest,
            first_number,
            history_step_size,
            false,
        )
        .await;
    }

    debug!({BRANCH} = %branch, first_number, "forward_cursor - find forward anchor");
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
            debug!({BRANCH} = %branch, boundary, "forward_cursor - calculating from sealed");
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
            debug!({BRANCH} = %branch, "forward_cursor - calculating from latest band");
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
            debug!(
                {BRANCH} = %branch, first_number, %anchor,
                "forward_cursor - calculating from unverified gap",
            );
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
    Ok(target)
}
