// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use lore_base::error::AddressNotFound;
use lore_base::lore_drain_tasks;
use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_proto::BranchPushRequest;
use lore_proto::BranchPushResponse;
use lore_revision::branch;
use lore_revision::branch::BranchError;
use lore_revision::branch::LATEST;
use lore_revision::branch::PROTECT;
use lore_revision::branch::push;
use lore_revision::interface::ExecutionContext;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::metadata::Metadata;
use lore_revision::notification::NotificationSender;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryMetadata;
use lore_revision::state;
use lore_revision::state::State;
use lore_revision::util::request_tracker::StoreRequestTracker;
use lore_storage::StoreError;
use lore_storage::StoreMatch;
use lore_storage::StoreMatchResult;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::tracing::fields::ADDRESS;
use lore_telemetry::tracing::fields::BRANCH_ID;
use lore_telemetry::tracing::fields::REPOSITORY_ID;
use lore_telemetry::tracing::fields::REVISION;
use lore_transport::grpc::address_not_found_status;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use opentelemetry::metrics::Histogram;
use parking_lot::Mutex;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::Instrument;
use tracing::Level;
use tracing::debug;
use tracing::info;
use tracing::instrument;
use tracing::span;
use tracing::warn;

use crate::authnz::repository_authorizer::Action;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::cache::revision::store_history_step;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::ServerResultExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::get_write_token;
use crate::grpc::hook_error_to_status;
use crate::grpc::no_repository_access_status;
use crate::grpc::none_or_status;
use crate::grpc::warn_error_to_status;
use crate::hooks::HookContext;
use crate::hooks::HookDispatcher;
use crate::hooks::HookPoint;
use crate::hooks::context::METADATA_BRANCH_NAME;
use crate::util::setup_execution;

struct BranchPushInstrumentProvider;

impl InstrumentProvider for BranchPushInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "lore.branch_push"
    }
}

static PEAK_FRAGMENT_LOOKUPS_IN_FLIGHT: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    BranchPushInstrumentProvider
        .length_histogram("peak_fragment_lookups_in_flight", peak_boundaries())
});

static FRAGMENT_LOOKUPS: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    BranchPushInstrumentProvider.length_histogram("fragment_lookups", count_boundaries())
});

static PEAK_METADATA_READS_IN_FLIGHT: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    BranchPushInstrumentProvider
        .length_histogram("peak_metadata_reads_in_flight", doubling_boundaries())
});

// Zero gets a bucket of its own, for the pushes whose walk read no metadata blob.
static METADATA_READS: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    BranchPushInstrumentProvider.length_histogram(
        "metadata_reads",
        vec![0., 1., 10., 100., 1_000., 10_000., 100_000., 1_000_000.],
    )
});

static CONCURRENT_QUERY_BATCHES: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    BranchPushInstrumentProvider.length_histogram("concurrent_query_batches", peak_boundaries())
});

static COLLECTED_ADDRESSES: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    BranchPushInstrumentProvider.length_histogram("collected_addresses", count_boundaries())
});

static PHASE_DURATION: LazyLock<Histogram<f64>> =
    LazyLock::new(|| push_duration_histogram("phase_duration"));

static CANCELLED_PHASE: LazyLock<Counter<u64>> =
    LazyLock::new(|| BranchPushInstrumentProvider.counter("cancelled_phase"));

static STORE_LATEST_ATTEMPTS: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    BranchPushInstrumentProvider.length_histogram(
        "store_latest_attempts",
        vec![1., 2., 3., 5., 10., 25., 50., 100.],
    )
});

static VERIFY_COLLECT_DURATION: LazyLock<Histogram<f64>> =
    LazyLock::new(|| push_duration_histogram("verify_collect_duration"));

static VERIFY_EXIST_DURATION: LazyLock<Histogram<f64>> =
    LazyLock::new(|| push_duration_histogram("verify_exist_duration"));

static FAST_FORWARD_MERGE_CHANGES: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    BranchPushInstrumentProvider.length_histogram(
        "fast_forward_merge_changes",
        vec![
            0., 1., 2., 5., 10., 50., 100., 1_000., 10_000., 100_000., 1_000_000.,
        ],
    )
});

/// A push that takes at least this long is logged at info with its breakdown, as is every
/// cancelled or failed push.
const SLOW_PUSH_LOG_THRESHOLD: Duration = Duration::from_secs(10);

/// A millisecond histogram for a part of a push. Its buckets are dense from 30 s up so that, for
/// a push near the handler timeout, each part's share of the time stays visible.
fn push_duration_histogram(name: &'static str) -> Histogram<f64> {
    BranchPushInstrumentProvider.latency_histogram_ms_with_boundaries(
        name,
        vec![
            10., 50., 100., 250., 500., 1_000., 2_500., 5_000., 10_000., 20_000., 30_000., 40_000.,
            45_000., 50_000., 55_000., 60_000., 90_000., 120_000., 150_000., 180_000.,
        ],
    )
}

fn peak_boundaries() -> Vec<f64> {
    vec![
        1., 10., 50., 100., 250., 500., 1_000., 2_500., 5_000., 10_000., 25_000., 50_000.,
        100_000., 250_000.,
    ]
}

fn doubling_boundaries() -> Vec<f64> {
    (0..=16).map(|power| f64::from(1u32 << power)).collect()
}

fn count_boundaries() -> Vec<f64> {
    vec![
        10., 100., 1_000., 10_000., 50_000., 100_000., 500_000., 1_000_000., 5_000_000.,
    ]
}

/// The service a push arrived through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushHandler {
    Legacy,
    V1,
}

impl PushHandler {
    fn label(self) -> &'static str {
        match self {
            PushHandler::Legacy => "legacy",
            PushHandler::V1 => "v1",
        }
    }
}

/// A stage of a push timed by [`PushReport`]. Steps a push runs concurrently stay within one
/// phase. The response hooks' repository metadata, read beside the whole push, is not a phase
/// of its own: a wait for it once the push returns falls in `Respond`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushPhase {
    /// Authorization, loading the branch, its head and the incoming revision, the pre-hooks,
    /// and the protection check.
    Prepare,
    /// The branch name check beside deciding how the head moves and, when it moves, loading the
    /// incoming revision's parents and verifying the fragments it adds.
    Verify,
    /// Building a merge revision onto a head that moved since the client branched.
    FastForwardMerge,
    /// Rewriting the incoming revision when its number is wrong, storing the new head, and its
    /// history step.
    Commit,
    /// From the push returning, with success or an error, to the response: the wait for the
    /// response hooks' repository metadata and the hooks themselves.
    Respond,
}

impl PushPhase {
    /// Every phase, in the order a push can pass through them.
    pub const ALL: [PushPhase; 5] = [
        PushPhase::Prepare,
        PushPhase::Verify,
        PushPhase::FastForwardMerge,
        PushPhase::Commit,
        PushPhase::Respond,
    ];

    const fn index(self) -> usize {
        match self {
            PushPhase::Prepare => 0,
            PushPhase::Verify => 1,
            PushPhase::FastForwardMerge => 2,
            PushPhase::Commit => 3,
            PushPhase::Respond => 4,
        }
    }

    fn label(self) -> &'static str {
        match self {
            PushPhase::Prepare => "prepare",
            PushPhase::Verify => "verify",
            PushPhase::FastForwardMerge => "fast_forward_merge",
            PushPhase::Commit => "commit",
            PushPhase::Respond => "respond",
        }
    }
}

// Each phase's `index` is its position in `ALL`, which orders the per-phase durations.
const _: () = {
    let mut i = 0;
    while i < PushPhase::ALL.len() {
        assert!(PushPhase::ALL[i].index() == i);
        i += 1;
    }
};

/// How a push timed by [`PushReport`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushOutcome {
    /// The handler returned a response.
    Completed,
    /// The push left the head where it was for the client to resolve: not a fast-forward and
    /// not forced, or a fast-forward merge with conflicts.
    Declined,
    /// The handler returned an error status for anything else, including a request rejected by
    /// authorization or for a zero revision, or it panicked. A request without a valid repository
    /// header is rejected before its report exists and is not recorded.
    Error,
    /// The handler future was dropped before it returned, such as by the handler timeout or a
    /// server shutting down. A client that disconnects does not drop it: the handler runs on to
    /// completion on the core runtime.
    Cancelled,
}

impl PushOutcome {
    /// How something that returned, or was dropped before it returned, ended. A push declined
    /// and answered with an error status is declined.
    #[lore_macro::test_pub]
    fn of(finished: bool, declined: bool, errored: bool) -> Self {
        match (finished, declined, errored) {
            // Dropped while unwinding: it failed, rather than being cut off from outside.
            (false, _, _) if std::thread::panicking() => PushOutcome::Error,
            (false, _, _) => PushOutcome::Cancelled,
            (true, true, _) => PushOutcome::Declined,
            (true, false, true) => PushOutcome::Error,
            (true, false, false) => PushOutcome::Completed,
        }
    }

    fn label(self) -> &'static str {
        match self {
            PushOutcome::Completed => "completed",
            PushOutcome::Declined => "declined",
            PushOutcome::Error => "error",
            PushOutcome::Cancelled => "cancelled",
        }
    }
}

/// The time one push spends in each [`PushPhase`] and the store requests its fragment
/// verification makes, recorded once when dropped, so a push cancelled part-way still reports
/// what it did and the phase it was in.
pub struct PushReport {
    handler: PushHandler,
    repository: RepositoryId,
    target: Option<(BranchId, Hash)>,
    started: Instant,
    phase: PushPhase,
    phase_started: Instant,
    /// When the push returned, `None` while it runs or once it is dropped part-way.
    finished_at: Option<Instant>,
    phase_durations: [Option<Duration>; PushPhase::ALL.len()],
    store_latest_attempts: u64,
    fast_forward_merge_changes: Option<u64>,
    /// The fragment lookups and metadata blob reads the verification's tree walk makes.
    store_requests: Arc<StoreRequestTracker>,
    collected_addresses: Option<u64>,
    concurrent_query_batches: Option<u64>,
    /// When the verification's tree walk started. It runs until the existence checks start.
    collect_started: Option<Instant>,
    /// When the verification's existence checks, slow-down retries included, started.
    exist_started: Option<Instant>,
    /// When the verification returned, with success or an error.
    verified_at: Option<Instant>,
    verification_failed: bool,
    errored: bool,
    /// Whether the push left the head where it was for the client to resolve.
    declined: bool,
}

/// A snapshot of a [`PushReport`], with the running phase counted up to the moment it was taken.
pub struct PushSummary {
    pub outcome: PushOutcome,
    pub elapsed: Duration,
    /// The phase the push was in when the snapshot was taken, or ended in.
    pub phase: PushPhase,
    phase_durations: [Option<Duration>; PushPhase::ALL.len()],
    pub store_latest_attempts: u64,
    /// The changes in the latest fast-forward merge diff computed without conflicts, whether or
    /// not that merge reaches the branch head. `None` without one, or once a later diff finds
    /// conflicts.
    pub fast_forward_merge_changes: Option<u64>,
    pub num_lookups: u64,
    pub peak_lookups_in_flight: u64,
    pub num_metadata_reads: u64,
    pub peak_metadata_reads_in_flight: u64,
    /// The time the verification's tree walk took, counted up to the snapshot while it runs.
    /// `None` if it never started.
    pub verify_collect: Option<Duration>,
    /// The time the verification's existence checks took, counted up to the snapshot while they
    /// run. `None` if they never started.
    pub verify_exist: Option<Duration>,
}

impl PushSummary {
    /// The time spent in `phase`, `None` if the push never entered it.
    pub fn phase_duration(&self, phase: PushPhase) -> Option<Duration> {
        self.phase_durations[phase.index()]
    }
}

impl PushReport {
    /// Starts timing a push to `repository` in [`PushPhase::Prepare`].
    pub fn new(handler: PushHandler, repository: RepositoryId) -> Self {
        let now = Instant::now();
        Self {
            handler,
            repository,
            target: None,
            started: now,
            phase: PushPhase::Prepare,
            phase_started: now,
            finished_at: None,
            phase_durations: [None; PushPhase::ALL.len()],
            store_latest_attempts: 0,
            fast_forward_merge_changes: None,
            store_requests: Arc::default(),
            collected_addresses: None,
            concurrent_query_batches: None,
            collect_started: None,
            exist_started: None,
            verified_at: None,
            verification_failed: false,
            errored: false,
            declined: false,
        }
    }

    /// Names the branch and revision pushed in the slow or cancelled push log.
    pub fn set_target(&mut self, branch: BranchId, revision: Hash) {
        self.target = Some((branch, revision));
    }

    /// Closes the running phase and starts `phase`.
    pub fn enter(&mut self, phase: PushPhase) {
        let now = Instant::now();
        self.close_phase(now);
        self.phase = phase;
        self.phase_started = now;
    }

    /// Runs `body` in `execution` with the report, and marks the push completed or errored by
    /// what `body` returns. A future dropped before then drops the report unfinished, which
    /// records the push as cancelled.
    pub async fn run<T, E>(
        mut self,
        execution: Arc<ExecutionContext>,
        body: impl AsyncFnOnce(&mut PushReport) -> Result<T, E>,
    ) -> Result<T, E> {
        LORE_CONTEXT
            .scope(execution, async move {
                let result = body(&mut self).await;
                self.errored = result.is_err();
                self.finish();
                result
            })
            .await
    }

    /// Closes the running phase and marks the push as returned rather than cancelled.
    pub fn finish(&mut self) {
        let now = Instant::now();
        self.close_phase(now);
        self.finished_at = Some(now);
    }

    /// What the report holds now, as the drop records it.
    pub fn summary(&self) -> PushSummary {
        let mut phase_durations = self.phase_durations;
        let end = if let Some(finished_at) = self.finished_at {
            finished_at
        } else {
            let now = Instant::now();
            add_phase_time(&mut phase_durations, self.phase, now - self.phase_started);
            now
        };
        let outcome = PushOutcome::of(self.finished_at.is_some(), self.declined, self.errored);
        let verify_end = self.verified_at.unwrap_or(end);
        let verify_collect = self
            .collect_started
            .map(|started| self.exist_started.unwrap_or(verify_end) - started);
        let verify_exist = self.exist_started.map(|started| verify_end - started);
        PushSummary {
            outcome,
            elapsed: end - self.started,
            phase: self.phase,
            phase_durations,
            store_latest_attempts: self.store_latest_attempts,
            fast_forward_merge_changes: self.fast_forward_merge_changes,
            num_lookups: self.store_requests.lookups(),
            peak_lookups_in_flight: self.store_requests.peak_lookups_in_flight(),
            num_metadata_reads: self.store_requests.metadata_reads(),
            peak_metadata_reads_in_flight: self.store_requests.peak_metadata_reads_in_flight(),
            verify_collect,
            verify_exist,
        }
    }

    fn close_phase(&mut self, now: Instant) {
        if self.finished_at.is_none() {
            add_phase_time(
                &mut self.phase_durations,
                self.phase,
                now - self.phase_started,
            );
        }
    }
}

fn add_phase_time(
    durations: &mut [Option<Duration>; PushPhase::ALL.len()],
    phase: PushPhase,
    time: Duration,
) {
    let duration = &mut durations[phase.index()];
    *duration = Some(duration.unwrap_or_default() + time);
}

impl Drop for PushReport {
    fn drop(&mut self) {
        let summary = self.summary();
        let outcome = summary.outcome.label();
        let handler = KeyValue::new("handler", self.handler.label());
        let handler_and_outcome = [handler.clone(), KeyValue::new("outcome", outcome)];
        self.record_verification(&summary, &handler);

        for phase in PushPhase::ALL {
            if let Some(duration) = summary.phase_duration(phase) {
                let [handler, outcome] = handler_and_outcome.clone();
                PHASE_DURATION.record(
                    duration.as_secs_f64() * 1000.,
                    &[KeyValue::new("phase", phase.label()), handler, outcome],
                );
            }
        }
        let cancelled = summary.outcome == PushOutcome::Cancelled;
        if cancelled {
            CANCELLED_PHASE.add(1, &[KeyValue::new("phase", summary.phase.label()), handler]);
        }
        if summary.store_latest_attempts != 0 {
            STORE_LATEST_ATTEMPTS.record(summary.store_latest_attempts, &handler_and_outcome);
        }
        if let Some(changes) = summary.fast_forward_merge_changes {
            FAST_FORWARD_MERGE_CHANGES.record(changes, &handler_and_outcome);
        }

        if cancelled
            || summary.outcome == PushOutcome::Error
            || summary.elapsed >= SLOW_PUSH_LOG_THRESHOLD
        {
            let (branch, revision) = self.target.unzip();
            let millis = |phase| summary.phase_duration(phase).map(|d| d.as_millis());
            info!(
                handler = self.handler.label(),
                {REPOSITORY_ID} = %self.repository,
                {BRANCH_ID} = branch.map(tracing::field::display),
                {REVISION} = revision.map(tracing::field::display),
                outcome,
                cancelled_in = cancelled.then(|| summary.phase.label()),
                elapsed_ms = summary.elapsed.as_millis(),
                prepare_ms = millis(PushPhase::Prepare),
                verify_ms = millis(PushPhase::Verify),
                verify_collect_ms = summary.verify_collect.map(|d| d.as_millis()),
                verify_exist_ms = summary.verify_exist.map(|d| d.as_millis()),
                fast_forward_merge_ms = millis(PushPhase::FastForwardMerge),
                commit_ms = millis(PushPhase::Commit),
                respond_ms = millis(PushPhase::Respond),
                store_latest_attempts = summary.store_latest_attempts,
                fast_forward_merge_changes = summary.fast_forward_merge_changes,
                num_lookups = summary.num_lookups,
                peak_lookups_in_flight = summary.peak_lookups_in_flight,
                num_metadata_reads = summary.num_metadata_reads,
                peak_metadata_reads_in_flight = summary.peak_metadata_reads_in_flight,
                num_collected_addresses = self.collected_addresses,
                num_concurrent_query_batches = self.concurrent_query_batches,
                "Slow, cancelled or failed branch push"
            );
        }
    }
}

impl PushReport {
    /// Records the verification's timings, labelled with how the verification itself ended, and
    /// its store requests.
    fn record_verification(&self, summary: &PushSummary, handler: &KeyValue) {
        let outcome = PushOutcome::of(self.verified_at.is_some(), false, self.verification_failed);
        let completed_with_nothing_collected =
            outcome == PushOutcome::Completed && self.collected_addresses.unwrap_or(0) == 0;
        let attributes = [handler.clone(), KeyValue::new("outcome", outcome.label())];

        // A verification that completed having collected and looked up nothing had nothing to
        // verify, and would only skew the timings. One that failed or was cut off is recorded
        // whatever it reached, since its time is what explains the push's end.
        if !(completed_with_nothing_collected && summary.num_lookups == 0) {
            if let Some(collect) = summary.verify_collect {
                VERIFY_COLLECT_DURATION.record(collect.as_secs_f64() * 1000., &attributes);
            }
            // A verification that completed without collecting an address asked the store nothing.
            if let Some(exist) = summary.verify_exist
                && !completed_with_nothing_collected
            {
                VERIFY_EXIST_DURATION.record(exist.as_secs_f64() * 1000., &attributes);
            }
        }

        // A verification that made no lookup, such as one for a revision that adds no fragment,
        // would only skew the lookup distributions, which carry no outcome to filter on.
        if summary.num_lookups == 0 {
            return;
        }
        PEAK_FRAGMENT_LOOKUPS_IN_FLIGHT.record(summary.peak_lookups_in_flight, &[]);
        FRAGMENT_LOOKUPS.record(summary.num_lookups, &[]);
        METADATA_READS.record(summary.num_metadata_reads, &[]);
        // A walk that read no metadata blob has no peak to report, and would only skew the peaks
        // toward zero.
        if summary.num_metadata_reads != 0 {
            PEAK_METADATA_READS_IN_FLIGHT.record(summary.peak_metadata_reads_in_flight, &[]);
        }
        if let Some(collected_addresses) = self.collected_addresses {
            COLLECTED_ADDRESSES.record(collected_addresses, &[]);
        }
        if let Some(concurrent_query_batches) = self.concurrent_query_batches {
            CONCURRENT_QUERY_BATCHES.record(concurrent_query_batches, &[]);
        }
    }
}

#[lore_macro::test_pub]
pub(crate) fn extract_client_ip<T>(request: &Request<T>) -> Option<IpAddr> {
    // try to get the LAST entry from XFF metadata header (injected by ALB)
    if let Some(ip_str) = request
        .metadata()
        .get("x-forwarded-for")
        .and_then(|header_value| header_value.to_str().ok())
        .and_then(|ip_list| ip_list.rsplit(',').next()) // use the last value, if multiple are present
        .map(str::trim)
        && let Ok(ip) = ip_str.parse::<IpAddr>()
    {
        return Some(ip);
    }

    // if XFF is not available, fallback to using remote_addr()
    request.remote_addr().map(|socket_addr| socket_addr.ip())
}

#[allow(clippy::too_many_arguments)]
#[tracing::instrument(name = "BranchPush::handle", skip_all)]
pub async fn handler(
    request: Request<BranchPushRequest>,
    authorizer: Arc<dyn RepositoryAuthorizer>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    notification: Arc<dyn NotificationSender>,
    hook_dispatcher: &HookDispatcher,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<BranchPushResponse>, Status> {
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let repository = get_repository(request.metadata())?;
    let execution = setup_execution(module_path!(), correlation_id.clone(), user_id.clone());

    PushReport::new(PushHandler::Legacy, repository)
        .run(execution, async move |report| {
            let access = PushAccess::of(&authorizer, request.extensions(), repository).await?;

            let client_ip: Option<String> =
                extract_client_ip(&request).map(|ip_addr| ip_addr.to_string());
            let req = request.into_inner();
            let branch = BranchId::from(req.branch);
            let revision = Hash::from(req.revision);
            let force = req.force;
            let fast_forward_merge = req.fast_forward_merge;
            report.set_target(branch, revision);

            if revision.is_zero() {
                warn!("Invalid branch push request, revision is zero");
                return Err(Status::invalid_argument("Invalid revision"));
            }

            debug!(
                {REVISION} = %revision,
                ?access,
                {BRANCH_ID} = %branch,
                force,
                fast_forward_merge,
                "Handling branch push request",
            );

            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store,
                mutable_store,
                repository,
            ));

            let repository_id: RepositoryId = repository.id;

            let mut ctx_builder = HookContext::builder()
                .correlation_id(correlation_id.clone())
                .hook_point(HookPoint::BranchPush)
                .repository(repository_id)
                .user(user_id.clone())
                .branch(branch)
                .revision(revision);

            if let Some(ip) = client_ip {
                ctx_builder = ctx_builder.metadata("client_ip", ip);
            }

            let mut hook_ctx = ctx_builder.build();

            hook_dispatcher
                .dispatch_pre(HookPoint::BranchPush, &hook_ctx)
                .await
                .map_err(hook_error_to_status)?;

            let (pushed, repository_metadata) = tokio::join!(
                async {
                    let pushed = push(
                        repository.clone(),
                        branch,
                        revision,
                        access,
                        force,
                        fast_forward_merge,
                        history_step_size,
                        acceleration,
                        &mut *report,
                    )
                    .await;
                    report.enter(PushPhase::Respond);
                    pushed
                },
                load_response_repository_metadata(hook_dispatcher, repository.clone()),
            );
            let PushResult {
                success,
                fast_forward_merged,
                revision,
                revision_number,
                branch_name,
            } = pushed?;

            if success {
                lore_spawn!({
                    let user_id = user_id.clone();
                    async move {
                        notification
                            .branch_pushed(
                                repository_id,
                                branch,
                                &user_id,
                                revision,
                                revision_number,
                            )
                            .instrument(span!(Level::DEBUG, "publish_notification"))
                            .await;
                    }
                    .in_current_span()
                });

                // Post-hook dispatch (async, non-blocking)
                hook_ctx.set_revision_number(revision_number);
                hook_dispatcher.spawn_post(HookPoint::BranchPush, hook_ctx);
            }

            let num_branches_pushed = instrument_provider.counter("num_branches_pushed");
            num_branches_pushed.add(1, &[]);

            let message = if success {
                dispatch_response_message(
                    hook_dispatcher,
                    &correlation_id,
                    &user_id,
                    repository_id,
                    branch,
                    &branch_name,
                    revision,
                    repository_metadata.as_ref(),
                )
            } else {
                None
            };

            Ok(Response::new(BranchPushResponse {
                success,
                fast_forward_merged,
                revision: revision.into(),
                revision_number,
                message,
            }))
        })
        .await
}

/// The repository metadata the response hooks of a push read, or `None` when no hook is
/// registered to read it.
///
/// A read that fails is `None` as well: the metadata only decorates the hook context.
pub(crate) async fn load_response_repository_metadata(
    hook_dispatcher: &HookDispatcher,
    repository: Arc<RepositoryContext>,
) -> Option<RepositoryMetadata> {
    if !hook_dispatcher.has_hooks(HookPoint::BranchPush) {
        return None;
    }

    // no filter_slow_down()? usage here: a throttled read leaves the hook context
    // without the repository fields rather than failing the push.
    let metadata_hash = repository::metadata_hash(repository.clone()).await.ok()?;
    repository::metadata(repository, metadata_hash).await.ok()
}

/// Dispatches the response hooks of a successful push to generate an optional message for the
/// client.
///
/// Without `repository_metadata`, the hook context carries no repository keys.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_response_message(
    hook_dispatcher: &HookDispatcher,
    correlation_id: &str,
    user_id: &str,
    repository_id: RepositoryId,
    branch: BranchId,
    branch_name: &str,
    revision: Hash,
    repository_metadata: Option<&RepositoryMetadata>,
) -> Option<String> {
    let mut builder = HookContext::builder()
        .correlation_id(correlation_id)
        .hook_point(HookPoint::BranchPush)
        .repository(repository_id)
        .user(user_id)
        .branch(branch)
        .revision(revision)
        .metadata(METADATA_BRANCH_NAME, branch_name);

    if let Some(repository_metadata) = repository_metadata {
        builder = builder
            .metadata("repository_name", repository_metadata.name.clone())
            .metadata(
                "default_branch_name",
                &repository_metadata.default_branch_name,
            )
            .metadata(
                "is_default_branch",
                if repository_metadata.default_branch == branch {
                    "true"
                } else {
                    "false"
                },
            );
    }

    let response_ctx = builder.build();
    hook_dispatcher
        .dispatch_response(HookPoint::BranchPush, &response_ctx)
        .message
}

pub struct PushResult {
    pub success: bool,
    pub fast_forward_merged: bool,
    pub revision: Hash,
    pub revision_number: u64,
    /// The name the branch metadata held when the push checked it, empty when it holds none.
    pub branch_name: String,
}

struct HeadUpdate {
    success: bool,
    fast_forward_merged: bool,
    /// The latest revision of the branch when the push is answered.
    revision: Hash,
    revision_number: u64,
}

impl HeadUpdate {
    /// A push that leaves the head at `current` for the client to resolve.
    fn declined(current: Hash) -> Self {
        Self {
            success: false,
            fast_forward_merged: false,
            revision: current,
            revision_number: 0,
        }
    }
}

/// Which pushes the caller may make: to an unprotected branch with `write`,
/// to a protected one with `push-protected`.
#[derive(Clone, Copy, Debug)]
pub struct PushAccess {
    pub write: bool,
    pub push_protected: bool,
}

impl PushAccess {
    /// Denies with [`no_repository_access_status`] when the caller may make
    /// neither kind of push, before anything about the branch is read.
    pub async fn of(
        authorizer: &Arc<dyn RepositoryAuthorizer>,
        extensions: &tonic::Extensions,
        repository: RepositoryId,
    ) -> Result<Self, Status> {
        let access = Self {
            write: authorizer
                .permits(extensions, repository, Action::Write)
                .await,
            push_protected: authorizer
                .permits(extensions, repository, Action::PushProtected)
                .await,
        };
        if access.write || access.push_protected {
            Ok(access)
        } else {
            Err(no_repository_access_status())
        }
    }
}

pub(crate) struct HeadAndIncoming {
    /// The latest revision of the branch, zero when it has none.
    head: Hash,
    /// The state of the revision being pushed.
    incoming: Arc<State>,
}

enum PreparedPush {
    /// The push is answered without moving the head.
    Done(HeadUpdate),
    /// The fragments are verified, and the head moves as `head_move` says.
    Move {
        loaded: HeadAndIncoming,
        head_move: HeadMove,
        parents: VerifiedParents,
    },
}

struct VerifiedParents {
    parent: Arc<State>,
    /// The second parent, set for a merge revision only.
    other: Option<Arc<State>>,
}

/// Pushes `latest` as the latest revision of `branch`. A branch whose name doesn't map to it, as
/// an archive leaves it, fails as not found.
#[allow(clippy::too_many_arguments)]
pub async fn push(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    latest: Hash,
    access: PushAccess,
    force: bool,
    fast_forward_merge: bool,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
    report: &mut PushReport,
) -> Result<PushResult, Status> {
    let (metadata, loaded) = load_branch_and_head(repository.clone(), branch, latest).await;
    push_loaded(
        repository,
        branch,
        latest,
        access,
        force,
        fast_forward_merge,
        history_step_size,
        acceleration,
        metadata?,
        loaded,
        report,
    )
    .await
}

/// Loads the metadata of `branch` beside its latest revision and the state of `latest`, the
/// reads a push makes before it checks anything.
pub(crate) async fn load_branch_and_head(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    latest: Hash,
) -> (Result<Metadata, Status>, Result<HeadAndIncoming, Status>) {
    tokio::join!(
        load_branch_metadata(repository.clone(), branch),
        load_head_and_incoming(repository, branch, latest),
    )
}

/// Pushes `latest` as the latest revision of `branch` from the branch `metadata` and the
/// `loaded` head read for it. A branch whose name doesn't map to it, as an archive leaves it,
/// fails as not found.
///
/// The name check runs beside the fragment verification of the incoming revision. Both complete
/// before the head moves, and a failed branch check is reported ahead of any failure of the
/// incoming revision.
#[allow(clippy::too_many_arguments)]
#[instrument(level = "debug", skip_all, fields(branch))]
pub(crate) async fn push_loaded(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    latest: Hash,
    access: PushAccess,
    force: bool,
    fast_forward_merge: bool,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
    metadata: Metadata,
    loaded: Result<HeadAndIncoming, Status>,
    report: &mut PushReport,
) -> Result<PushResult, Status> {
    // Zero hash is never valid to push as latest pointer
    if latest.is_zero() {
        return Err(Status::failed_precondition("invalid revision signature"));
    }

    check_push_access(&metadata, access)?;
    let branch_name = branch::name(&metadata).unwrap_or_default().to_string();

    // Fragment verification starts only once protection is checked, so a push refused as
    // protected never walks its revision.
    report.enter(PushPhase::Verify);
    let (name_mapped, prepared) = tokio::join!(
        ensure_name_maps_to_branch(repository.clone(), branch, &branch_name),
        async {
            prepare_push(
                repository.clone(),
                latest,
                loaded?,
                force,
                fast_forward_merge,
                &mut *report,
            )
            .await
        },
    );
    name_mapped?;

    let update = match prepared? {
        PreparedPush::Done(update) => update,
        PreparedPush::Move {
            loaded,
            head_move: HeadMove::Store,
            parents,
        } => {
            store_new_head(
                repository,
                branch,
                latest,
                loaded,
                parents,
                force,
                fast_forward_merge,
                history_step_size,
                acceleration,
                report,
            )
            .await?
        }
        PreparedPush::Move {
            loaded,
            head_move: HeadMove::Merge,
            ..
        } => {
            try_fast_forward_merge(
                repository,
                branch,
                loaded.incoming,
                loaded.head,
                history_step_size,
                acceleration,
                report,
            )
            .await?
        }
    };
    report.declined = !update.success;

    Ok(PushResult {
        success: update.success,
        fast_forward_merged: update.fast_forward_merged,
        revision: update.revision,
        revision_number: update.revision_number,
        branch_name,
    })
}

async fn load_branch_metadata(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
) -> Result<Metadata, Status> {
    let metadata_hash = branch::metadata_hash(repository.clone(), branch)
        .await
        .filter_slow_down()?
        .map_err(|_err| Status::not_found(format!("Branch {branch} not found")))?;
    branch::load_metadata(repository, metadata_hash)
        .await
        .filter_slow_down()?
        .warn_map_err(|err| Status::internal(format!("Failed to load branch metadata: {err}")))
}

/// Refuses a push the caller's `access` does not cover: a protected branch needs
/// `push-protected`, any other branch `write`.
fn check_push_access(metadata: &Metadata, access: PushAccess) -> Result<(), Status> {
    if metadata.get_bool(PROTECT).unwrap_or_default() {
        if !access.push_protected {
            warn!("Branch push failed, branch is protected");
            return Err(Status::permission_denied("protected"));
        }
        debug!("Push to protected branch");
    } else if !access.write {
        return Err(no_repository_access_status());
    }

    Ok(())
}

/// Fails unless `name` maps to `branch`, writing nothing: only a create restores a branch. A push
/// to an archived branch whose name maps to nothing, or to an id without metadata, fails as
/// `NOT_FOUND`, so the client restores the branch with a create and pushes again. One whose name
/// maps to another branch with metadata fails as in use, as that create would.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn ensure_name_maps_to_branch(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    name: &str,
) -> impl Future<Output = Result<(), Status>> {
    async move {
        if name.is_empty() {
            return Ok(());
        }

        let held = none_or_status(
            branch::load_name_to_id_local(repository.clone(), name).await,
            BranchError::is_branch_not_found,
        )?
        .unwrap_or_default();
        if held == branch {
            return Ok(());
        }
        if !held.is_zero() && holder_has_metadata(&repository, held, name).await? {
            info!(
                {BRANCH_ID} = %branch,
                %name,
                claimed_by = %held,
                "Cannot push to archived branch: name claimed by another branch",
            );
            return Err(Status::already_exists(format!(
                "Branch name '{name}' is in use by a different branch"
            )));
        }
        debug!({BRANCH_ID} = %branch, %name, "Push to an archived branch, which a create restores");
        Err(Status::not_found(format!("Branch '{name}' is archived")))
    }
}

/// True iff `holder`, the branch `name` maps to, has metadata, whatever name it holds: a create
/// refuses a name that maps to such a branch. A branch without metadata is a stale mapping, which a
/// create claims. Any other failure to read the metadata hash is reported rather than read as none.
async fn holder_has_metadata(
    repository: &Arc<RepositoryContext>,
    holder: BranchId,
    name: &str,
) -> Result<bool, Status> {
    match branch::metadata_hash(repository.clone(), holder)
        .await
        .filter_slow_down()?
    {
        Ok(_) => Ok(true),
        Err(err) if err.is_branch_not_found() => Ok(false),
        Err(err) => Err(err).warn_map_err(|err| holder_unreadable(name, err)),
    }
}

/// The failure of a push whose read of the metadata of the branch `name` maps to fails.
fn holder_unreadable(name: &str, err: &BranchError) -> Status {
    Status::internal(format!(
        "Failed to read the metadata of the branch '{name}' maps to: {err}"
    ))
}

/// The latest revision of `branch`, zero when it has none, and the state of `latest`.
async fn load_head_and_incoming(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    latest: Hash,
) -> Result<HeadAndIncoming, Status> {
    let (head, incoming) = tokio::join!(
        branch::load_latest_local(repository.clone(), branch),
        State::deserialize(repository, latest),
    );
    // Any failure to read the latest revision reads as none. The branch metadata check is what
    // tells an empty branch from a missing one.
    let head = head.filter_slow_down()?.unwrap_or_default();
    let incoming = incoming.filter_slow_down()?.warn_map_err(|err| {
        if err.is_not_found() {
            Status::not_found(format!(
                "Revision '{latest}' to push to latest is not found"
            ))
        } else {
            Status::internal(format!("failed to load current latest state: {err}"))
        }
    })?;
    Ok(HeadAndIncoming { head, incoming })
}

enum HeadMove {
    /// The incoming revision descends from the head, or the push is forced.
    Store,
    /// The head moved past the parent of the incoming revision, and the push asked for a
    /// fast-forward merge.
    Merge,
}

/// How the head moves from `current` to the incoming revision `state`, or `None` when the head
/// moved past its parent and the push did not ask for a fast-forward merge.
fn decide_head_move(
    current: Hash,
    state: &State,
    force: bool,
    fast_forward_merge: bool,
) -> Result<Option<HeadMove>, Status> {
    if force || current == state.parent_self() {
        return Ok(Some(HeadMove::Store));
    }

    if current.is_zero() {
        warn!("Branch push failed, branch does not exist");
        return Err(Status::not_found("Branch not found"));
    }

    if !fast_forward_merge {
        return Ok(None);
    }

    Ok(Some(HeadMove::Merge))
}

/// Decides how the head moves to `latest`, verifying the fragments of the incoming revision
/// when it moves at all.
async fn prepare_push(
    repository: Arc<RepositoryContext>,
    latest: Hash,
    loaded: HeadAndIncoming,
    force: bool,
    fast_forward_merge: bool,
    report: &mut PushReport,
) -> Result<PreparedPush, Status> {
    // If the incoming revision is already the latest revision the push is a no-op
    if loaded.head == latest {
        return Ok(PreparedPush::Done(HeadUpdate {
            success: true,
            fast_forward_merged: false,
            revision: loaded.head,
            revision_number: loaded.incoming.revision_number(),
        }));
    }

    let Some(head_move) =
        decide_head_move(loaded.head, &loaded.incoming, force, fast_forward_merge)?
    else {
        return Ok(PreparedPush::Done(HeadUpdate::declined(loaded.head)));
    };

    // Both a plain push and a fast-forward merge verify the incoming revision against its own
    // parents, which do not depend on the head. Without this check a client could reference
    // fragments that were never fully uploaded.
    let parents = verify_incoming_fragments(repository, loaded.incoming.clone(), report).await?;

    Ok(PreparedPush::Move {
        loaded,
        head_move,
        parents,
    })
}

/// Loads the parent states of `state` and verifies that every fragment it introduces against
/// them exists.
async fn verify_incoming_fragments(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    report: &mut PushReport,
) -> Result<VerifiedParents, Status> {
    let parent = State::deserialize(repository.clone(), state.parent_self())
        .await
        .filter_slow_down()?
        .warn_map_err(|err| Status::internal(format!("Failed to load incoming state: {err}")))?;

    let other = load_other_parent_state(repository.clone(), &state).await?;

    let verified = verify_fragments(repository, parent.clone(), other.clone(), state, report).await;
    report.verified_at = Some(Instant::now());
    report.verification_failed = verified.is_err();
    verified?;

    Ok(VerifiedParents { parent, other })
}

/// Stores the verified incoming revision as the latest revision of `branch`, retrying while
/// the head moves underneath it.
#[allow(clippy::too_many_arguments)]
async fn store_new_head(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    latest: Hash,
    loaded: HeadAndIncoming,
    parents: VerifiedParents,
    force: bool,
    fast_forward_merge: bool,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
    report: &mut PushReport,
) -> Result<HeadUpdate, Status> {
    report.enter(PushPhase::Commit);
    let HeadAndIncoming {
        head: mut current,
        incoming,
    } = loaded;

    let revision_number = next_revision_number(
        parents.parent.revision_number(),
        parents.other.as_ref().map_or(0, |s| s.revision_number()),
    );

    let mut new_head = latest;
    if incoming.revision_number() != revision_number {
        // Rewrite the revision with a correct revision number
        incoming.set_revision_number(revision_number);
        let write_token = get_write_token();
        new_head = incoming
            .serialize(repository.clone(), &write_token)
            .await
            .filter_slow_down()?
            .warn_map_err(|err| Status::internal(format!("Failed to serialize state: {err}")))?;
    }

    loop {
        report.store_latest_attempts += 1;
        let previous_head = try_store_latest(repository.clone(), branch, current, new_head)
            .await
            .filter_slow_down()?
            .warn_map_err(|err| {
                Status::internal(format!("Failed to store new latest pointer: {err}"))
            })?;

        if previous_head == current {
            store_history_step(
                repository.clone(),
                branch,
                history_step_size,
                acceleration,
                parents.parent,
                incoming.clone(),
            )
            .await;

            return Ok(HeadUpdate {
                success: true,
                fast_forward_merged: false,
                revision: new_head,
                revision_number: incoming.revision_number(),
            });
        }

        // Latest pointer moved during the processing of this push call
        current = previous_head;

        // The fragments were verified against the parents of the incoming revision, which do
        // not depend on the head, so a retry does not verify them again.
        match decide_head_move(current, &incoming, force, fast_forward_merge)? {
            Some(HeadMove::Store) => {}
            None => return Ok(HeadUpdate::declined(current)),
            Some(HeadMove::Merge) => {
                return try_fast_forward_merge(
                    repository,
                    branch,
                    incoming,
                    current,
                    history_step_size,
                    acceleration,
                    report,
                )
                .await;
            }
        }
    }
}

/// Attempts a server-side fast-forward merge when the target branch head has moved
/// since the client created the merge revision. The fragments of the incoming revision
/// must already be verified.
///
/// Creates a new merge revision with:
/// - `parent_self` = current branch head (target branch)
/// - `parent_other` = the incoming merge revision
///
/// Uses a three-way diff between the original merge base, the incoming revision,
/// and the current head. If conflicts are detected, returns failure so the client
/// can resolve locally.
///
/// Retries via CAS loop if the branch head moves again during processing.
#[instrument(level = "debug", skip_all)]
async fn try_fast_forward_merge(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    incoming_state: Arc<State>,
    mut current_head: Hash,
    history_step_size: u64,
    acceleration: crate::grpc::server::RevisionListAcceleration,
    report: &mut PushReport,
) -> Result<HeadUpdate, Status> {
    let incoming_revision = incoming_state.revision();
    let original_base = incoming_state.parent_self();

    debug!(
        %incoming_revision, %original_base, %current_head,
        "Attempting fast-forward merge"
    );

    loop {
        report.enter(PushPhase::FastForwardMerge);

        // Three-way diff: base=original merge target, source=incoming merge, target=current head
        debug!(
            %original_base, %incoming_revision, %current_head,
            "Computing diff3 for fast-forward merge"
        );
        let diff_result = lore_revision::revision::diff3_collect(
            repository.clone(),
            original_base,
            incoming_revision,
            current_head,
            None,
            false,
        )
        .await
        .filter_slow_down()?
        .warn_map_err(|err| {
            Status::internal(format!(
                "Failed to compute diff3 for fast-forward merge: {err}"
            ))
        })?;
        debug!(
            changes = diff_result.changes.len(),
            conflicts = diff_result.conflicts.len(),
            "diff3 result for fast-forward merge"
        );

        if !diff_result.conflicts.is_empty() {
            debug!(
                conflicts = diff_result.conflicts.len(),
                "Fast-forward merge has conflicts, rejecting"
            );
            report.fast_forward_merge_changes = None;
            return Ok(HeadUpdate::declined(current_head));
        }
        // Recorded before the merge is applied, so a push cancelled while applying or storing it
        // still reports the size of the merge it was working on.
        report.fast_forward_merge_changes = Some(diff_result.changes.len() as u64);

        // Deserialize the current head state to use as base for the new merge revision
        let state_current = State::deserialize(repository.clone(), current_head)
            .await
            .filter_slow_down()?
            .warn_map_err(|err| {
                Status::internal(format!(
                    "Failed to deserialize current head for fast-forward merge: {err}"
                ))
            })?;

        lore_revision::commit::merge_tree_changes(
            repository.clone(),
            state_current.clone(),
            &diff_result.changes,
        )
        .await
        .filter_slow_down()?
        .warn_map_err(|err| {
            Status::internal(format!(
                "Failed to merge tree changes for fast-forward merge: {err}"
            ))
        })?;

        // Set parents: self=current head (target branch), other=incoming merge revision
        state_current.set_parent_self(current_head);
        state_current.set_parent_other(incoming_revision);

        // Compute revision number from both parents
        let parent_state = State::deserialize(repository.clone(), current_head)
            .await
            .filter_slow_down()?
            .warn_map_err(|err| {
                Status::internal(format!("Failed to load current head state: {err}"))
            })?;

        let revision_number = next_revision_number(
            parent_state.revision_number(),
            incoming_state.revision_number(),
        );
        state_current.set_revision_number(revision_number);

        // Copy metadata from the incoming revision and set merged-by to "server"
        let incoming_metadata_hash = incoming_state.metadata_hash();
        if !incoming_metadata_hash.is_zero() {
            let mut metadata = lore_revision::metadata::Metadata::deserialize(
                repository.clone(),
                incoming_metadata_hash,
            )
            .await
            .filter_slow_down()?
            .warn_map_err(|err| {
                Status::internal(format!("Failed to load incoming revision metadata: {err}"))
            })?;

            metadata
                .set_branch(branch)
                .warn_map_err(|_| Status::internal("Failed to set branch in metadata"))?;
            // Preserve the existing merged-by field if set, otherwise fall back to "server"
            if metadata
                .get_string(lore_revision::metadata::MERGED_BY)
                .is_err()
            {
                metadata
                    .set_string(lore_revision::metadata::MERGED_BY, "server")
                    .warn_map_err(|_| Status::internal("Failed to set merged-by in metadata"))?;
            }
            metadata
                .set_u64(lore_revision::metadata::FAST_FORWARD_MERGE, 1)
                .warn_map_err(|_| {
                    Status::internal("Failed to set fast-forward-merge in metadata")
                })?;

            let metadata_hash = metadata
                .serialize(repository.clone())
                .await
                .filter_slow_down()?
                .warn_map_err(|_| Status::internal("Failed to serialize metadata"))?;
            state_current.set_metadata_hash(metadata_hash);
        }

        // Serialize the new merge state
        let write_token = get_write_token();
        let new_revision = state_current
            .serialize(repository.clone(), &write_token)
            .await
            .filter_slow_down()?
            .warn_map_err(|err| {
                Status::internal(format!(
                    "Failed to serialize fast-forward merge state: {err}"
                ))
            })?;

        // CAS: attempt to set the new revision as branch head
        report.enter(PushPhase::Commit);
        report.store_latest_attempts += 1;
        let previous_head =
            try_store_latest(repository.clone(), branch, current_head, new_revision)
                .await
                .filter_slow_down()?
                .warn_map_err(|err| {
                    Status::internal(format!("Failed to store fast-forward merge latest: {err}"))
                })?;

        if previous_head == current_head {
            // CAS succeeded
            debug!(
                %new_revision, revision_number,
                "Fast-forward merge succeeded"
            );

            // Store acceleration index if needed
            store_history_step(
                repository.clone(),
                branch,
                history_step_size,
                acceleration,
                parent_state,
                state_current.clone(),
            )
            .await;

            return Ok(HeadUpdate {
                success: true,
                fast_forward_merged: true,
                revision: new_revision,
                revision_number,
            });
        }

        // CAS failed — branch head moved again, retry with updated head
        debug!(
            %previous_head, %current_head,
            "Fast-forward merge CAS failed, retrying"
        );
        current_head = previous_head;
    }
}

/// Compute the next revision number from the parent revision numbers.
/// The revision number is one greater than the maximum of the two parents.
fn next_revision_number(parent_self_number: u64, parent_other_number: u64) -> u64 {
    std::cmp::max(parent_self_number, parent_other_number) + 1
}

/// The first address in `batch` the store did not answer with a full match,
/// warned where it is found.
fn first_missing_fragment(batch: &[Address], answers: &[StoreMatchResult]) -> Option<Address> {
    let address = batch
        .iter()
        .zip(answers.iter())
        .find(|(_, answer)| answer.match_made != StoreMatch::MatchFull)
        .map(|(address, _)| *address)?;

    warn!({ADDRESS} = %address, "Branch push failed, fragment not found");
    Some(address)
}

/// The state of the second parent `state` names, or `None` when it names none.
///
/// Only a merge revision names one, and both the revision number the merge takes and the
/// fragments it is verified against come from that parent. A parent the store cannot
/// answer for is reported as `FAILED_PRECONDITION` naming its address, as a missing
/// fragment is. The address is the one the load asked for, since a state the store cannot
/// answer for is reported as a plain absence carrying no address of its own.
async fn load_other_parent_state(
    repository: Arc<RepositoryContext>,
    state: &State,
) -> Result<Option<Arc<State>>, Status> {
    if state.parent_other().is_zero() {
        return Ok(None);
    }

    let address = Address::zero_context_hash(state.parent_other());
    let other_parent_state = State::deserialize(repository, state.parent_other())
        .await
        .filter_slow_down()?
        .warn_map_err(|err| {
            if err.is_not_found() {
                return address_not_found_status(
                    &AddressNotFound::from(address),
                    format!("Missing fragment '{address}'"),
                );
            }

            Status::internal(format!("Failed to load other parent state: {err}"))
        })?;

    Ok(Some(other_parent_state))
}

#[lore_macro::test_pub]
async fn collect_new_addresses(
    repository: Arc<RepositoryContext>,
    parent_state: Arc<State>,
    other_parent_state: Option<Arc<State>>,
    state: Arc<State>,
    store_requests: Arc<StoreRequestTracker>,
) -> Result<Vec<Address>, Status> {
    // Each walk appends what it found rather than returning it, so the drain below can
    // be the shared one: it joins every task before reporting a failure. A walk must
    // never be aborted part-way.
    let collected: Arc<Mutex<Vec<Address>>> = Default::default();

    let collect = async |parent_state: Arc<State>,
                         repository: Arc<RepositoryContext>,
                         to_state: Arc<State>,
                         collected: Arc<Mutex<Vec<Address>>>,
                         store_requests: Arc<StoreRequestTracker>| {
        let parent = parent_state.revision();
        let mut fragments = state::collect_new_fragments(
            repository,
            parent_state,
            to_state,
            true, /* Ignore already durably stored fragments */
            store_requests,
        )
            .instrument(span!(Level::DEBUG, "collect_new_addresses"))
            .await
            .warn_map_err(|err| {
                if let Some(converted_error) = err.as_address_not_found() {
                    return address_not_found_status(
                        converted_error,
                        format!(
                            "Failed to collect new fragments for verification. Missing address '{converted_error}'"
                        ),
                    );
                }

                Status::internal(format!(
                    "Failed to collect new fragments for verification: {err}"
                ))
            })?;
        debug!(%parent, num_addresses = fragments.len(), "Collected new fragments against parent");

        collected.lock().append(&mut fragments);
        Ok(())
    };

    let mut collect_tasks = JoinSet::default();
    lore_spawn!(
        collect_tasks,
        collect(
            parent_state,
            repository.clone(),
            state.clone(),
            collected.clone(),
            store_requests.clone(),
        )
        .in_current_span()
    );
    if let Some(other_parent_state) = other_parent_state {
        collected
            .lock()
            .push(Address::zero_context_hash(state.parent_other()));
        lore_spawn!(
            collect_tasks,
            collect(
                other_parent_state,
                repository,
                state,
                collected.clone(),
                store_requests
            )
            .in_current_span()
        );
    }
    lore_drain_tasks!(
        collect_tasks,
        Status::internal("Error with collect_new_addresses task")
    )?;

    let mut new_fragments = std::mem::take(&mut *collected.lock());

    // each address vec appended is already sorted, so `sort()` can detect the linear runs
    // better than `sort_unstable()`
    new_fragments.sort();
    new_fragments.dedup();

    debug!(
        num_addresses = new_fragments.len(),
        "Collected new addresses for verification"
    );

    Ok(new_fragments)
}

/// Verify that all new fragments between `parent_state` and `state` exist in the
/// immutable store. Also includes the other parent hash if the state is a merge.
/// Returns an error if any fragment is missing.
///
/// A merge revision joins two lines of history, and what it names is new to this
/// branch against either of them, so `other_parent_state` is collected against as well.
/// Reading that parent's own tree is what refuses a peer that pushed the merge without
/// the tip of the line it merged.
///
/// A missing fragment is reported as `FAILED_PRECONDITION` naming the address,
/// whether the walk cannot read it or the store answers that it is absent.
/// `NOT_FOUND` is left to name an absent branch, which a caller restores with a create.
async fn verify_fragments(
    repository: Arc<RepositoryContext>,
    parent_state: Arc<State>,
    other_parent_state: Option<Arc<State>>,
    state: Arc<State>,
    report: &mut PushReport,
) -> Result<(), Status> {
    report.collect_started = Some(Instant::now());
    let mut new_fragments = collect_new_addresses(
        repository.clone(),
        parent_state,
        other_parent_state,
        state,
        report.store_requests.clone(),
    )
    .await?;
    report.exist_started = Some(Instant::now());
    report.collected_addresses = Some(new_fragments.len() as u64);

    let mut retry = lore_revision::util::time::retry(
        push::RETRY_START_DURATION,
        push::RETRY_MAX_DURATION,
        push::RETRY_MAX_ATTEMPTS,
    );

    let max_batch_size = repository
        .immutable_store()
        .max_query_batch()
        .unwrap_or(1000)
        .clamp(100, 10000);

    let mut tasks = JoinSet::new();
    while !new_fragments.is_empty() || !tasks.is_empty() {
        let num_addresses = new_fragments.len();
        let batch_span = span!(
            Level::DEBUG,
            "exist_batch",
            items = num_addresses,
            batch_size = max_batch_size
        );

        batch_span.in_scope(|| {
            while !new_fragments.is_empty() {
                let repository = repository.clone();
                let batch =
                    new_fragments.split_off(new_fragments.len().saturating_sub(max_batch_size));
                lore_spawn!(
                    tasks,
                    async move {
                        let mut resolved = vec![StoreMatchResult::default(); batch.len()];
                        let result = repository
                            .immutable_store()
                            .query(repository.id, batch.as_slice(), &mut resolved)
                            .await
                            .map(|()| resolved);
                        (batch, result)
                    }
                    .in_current_span()
                );
            }
        });
        // A wave after a slow-down resends only the throttled batches, so the first wave is
        // the one that holds the peak.
        report
            .concurrent_query_batches
            .get_or_insert(tasks.len() as u64);
        debug!(
            num_batches = tasks.len(),
            num_addresses, "Sent existence query batches"
        );

        let mut num_slow_downs = 0;
        while let Some(result) = tasks.join_next().await {
            let (mut batch, result) =
                result.warn_map_err(|err| Status::internal(format!("Query task failed: {err}")))?;
            match result {
                Ok(result) => {
                    if let Some(missing) = first_missing_fragment(&batch, &result) {
                        return Err(address_not_found_status(
                            &AddressNotFound::from(missing),
                            format!("Missing fragment '{missing}'"),
                        ));
                    }
                }
                Err(StoreError::SlowDown(_)) => {
                    new_fragments.append(&mut batch);
                    num_slow_downs += 1;
                }
                Err(err) => {
                    let response = warn_error_to_status(&err, |err| {
                        Status::internal(format!("Store query failed: {err}"))
                    });
                    return Err(response);
                }
            }
        }

        if !new_fragments.is_empty() && !retry.wait().await {
            warn!("Exhausted {num_slow_downs} fragment exist retries");
            return Err(Status::resource_exhausted("Slow down"));
        }
    }

    Ok(())
}

#[instrument(level = "debug", skip_all, fields(branch))]
pub async fn try_store_latest(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    current_expected_latest: Hash,
    new_latest: Hash,
) -> Result<Hash, BranchError> {
    lore_revision::branch::mutable_try_store(
        repository.clone(),
        LATEST,
        branch,
        current_expected_latest,
        new_latest,
    )
    .await
}
