// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::cmp::Ordering;
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
use lore_revision::find::FindMatchResult;
use lore_revision::find::find_revision;
use lore_revision::lore::BranchId;
use lore_revision::metadata::Metadata;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision;
use lore_revision::revision::ResolveSearchLocation;
use lore_revision::state;
use lore_revision::util;
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
/// `parent_self` is items[0]'s signature, derived from the
/// `BranchLatestPointer` step keys; absent when no step key covers
/// the requested forward position (e.g. items[0] is the branch latest,
/// or its child sits inside a step block whose key wasn't recorded).
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

            let signature_forward = forward_cursor(&repository, &walked, history_step_size).await;
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
                    try_serve_signature_from_cache(repository, hash, history_step_size).await
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
                let (key, key_type) = branch::revision_step_key(
                    repository::SALT_LORE,
                    repository.id,
                    branch,
                    identifier.number,
                    history_step_size,
                );
                repository
                    .read_mutable_store()
                    .load(repository.id, key, key_type)
                    .await
                    .ok()
                    .map(|revision| (key, revision))
            } else {
                None
            };

            if let Some((key, block_revision)) = step_key_hit {
                debug!(number = identifier.number, key = %key, "Found history step key");
                let hash =
                    find_exact_revision(repository, branch, block_revision, identifier.number)
                        .await
                        .map_err(|err| Status::not_found(format!("Revision not found: {err}")))?;
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
) -> Option<ResolveStart> {
    let state = match state::State::deserialize(repository.clone(), signature).await {
        Ok(state) => state,
        Err(err) => {
            debug!(%signature, ?err, "Cache fast path: state deserialize failed");
            return None;
        }
    };
    let metadata = match Metadata::deserialize(repository.clone(), state.metadata_hash()).await {
        Ok(metadata) => metadata,
        Err(err) => {
            debug!(%signature, ?err, "Cache fast path: metadata deserialize failed");
            return None;
        }
    };
    let branch = match metadata.get_branch() {
        Ok(branch) => branch,
        Err(err) => {
            debug!(%signature, ?err, "Cache fast path: metadata missing branch");
            return None;
        }
    };
    let revision_number = state.revision_number();
    let (cached, strategy) = match cache::revision::load_cached_list(
        repository,
        branch,
        revision_number,
        history_step_size,
    )
    .await
    {
        Some(items) => (items, RevisionListStrategy::ListCache),
        None => (
            cache::revision::try_backfill_segment(
                repository,
                branch,
                revision_number,
                history_step_size,
            )
            .await?,
            RevisionListStrategy::ListCacheBackfill,
        ),
    };
    if !cached
        .items()
        .iter()
        .any(|item| item.signature == signature)
    {
        return None;
    }
    Some(ResolveStart::Items {
        items: cached_to_proto(cached.items()),
        branch,
        next_older: cached_next_older(cached.items()),
        strategy,
    })
}

async fn find_exact_revision(
    repository: &Arc<RepositoryContext>,
    branch: BranchId,
    block_revision: Hash,
    target_number: u64,
) -> Result<Hash, lore_revision::find::FindError> {
    find_revision(
        repository.clone(),
        branch,
        block_revision,
        false,
        None,
        |state, _metadata| match state.revision_number().cmp(&target_number) {
            Ordering::Equal => FindMatchResult::Match,
            Ordering::Less => FindMatchResult::Abort,
            Ordering::Greater => FindMatchResult::Continue,
        },
    )
    .await
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
            && let Ok(metadata) =
                Metadata::deserialize(repository.clone(), state.metadata_hash()).await
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

/// Looks up the revision whose `parent_self` is items[0]'s signature
/// — i.e. the cursor for the next newer page — by querying the
/// `BranchLatestPointer` step key for `items[0].number + 1` and then
/// walking that step block back to the exact target number. Returns
/// `None` when the step key isn't registered (the forward position
/// isn't covered by recorded boundaries) or when the walk inside the
/// block can't find the target.
async fn forward_cursor(
    repository: &Arc<RepositoryContext>,
    walked: &Walked,
    history_step_size: u64,
) -> Option<Hash> {
    let first = walked.items.first()?;
    let target_number = first.number.checked_add(1)?;
    let branch = walked.branch?;

    let (key, key_type) = branch::revision_step_key(
        repository::SALT_LORE,
        repository.id,
        branch,
        target_number,
        history_step_size,
    );
    let block_revision = repository
        .read_mutable_store()
        .load(repository.id, key, key_type)
        .await
        .ok()?;

    find_exact_revision(repository, branch, block_revision, target_number)
        .await
        .ok()
}

#[cfg(test)]
mod test {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Hash;
    use lore_revision::branch;
    use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
    use lore_revision::lore::RepositoryId;
    use lore_revision::metadata::Metadata;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::state::State;
    use lore_telemetry::InstrumentProvider;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use opentelemetry::KeyValue;
    use rand::random;
    use tonic::Request;

    use super::*;
    use crate::grpc::get_write_token;
    use crate::grpc::handlers::branch_push;
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
            // Cache serves items 200..101. Forward cursor targets rev
            // 201; seg 300's step key isn't registered (only 250 revs
            // exist), so no forward cursor.
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
            assert!(second_page.signature_forward.is_none());
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
    async fn forward_cursor_is_none_when_no_step_key_covers_target() {
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
            // ever registered (no boundary crossing happened).
            // Anchor at revision 25 — target 26 has no step key
            // registered, so forward cursor must be None.
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
            assert!(response.signature_forward.is_none());
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
            let (branch_id, _) =
                create_branch_with_jump_history(&repository_context, 99, 104, 0).await;

            let (key, key_type) = branch::revision_step_key(
                lore_revision::repository::SALT_LORE,
                repository,
                branch_id,
                200,
                DEFAULT_HISTORY_STEP_SIZE,
            );
            assert!(
                mutable_store
                    .clone()
                    .load(repository, key, key_type)
                    .await
                    .is_err(),
                "segment 200 holds the head and must not be sealed",
            );

            // Boundary 100 was crossed by the jump and is sealed with 99.
            let (crossed_key, crossed_key_type) = branch::revision_step_key(
                lore_revision::repository::SALT_LORE,
                repository,
                branch_id,
                100,
                DEFAULT_HISTORY_STEP_SIZE,
            );
            assert!(
                mutable_store
                    .load(repository, crossed_key, crossed_key_type)
                    .await
                    .is_ok(),
                "boundary 100 was crossed and must be sealed",
            );
        }))
        .await;
    }
}
