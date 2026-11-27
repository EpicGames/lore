// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::runtime::Cancellation;
use lore_server::util::core_hop::request_is_cancelled;
use lore_server::util::core_hop::with_request_cancellation;
use lore_server::util::setup_execution;
use lore_server::util::shared_read::MAX_SHARED_READ_WINDOW;
use lore_server::util::shared_read::ReadTaskFailed;
use lore_server::util::shared_read::Served;
use lore_server::util::shared_read::SharedReadMetrics;
use lore_server::util::shared_read::SharedReads;
use lore_server::util::shared_read::WindowTooLong;
use lore_telemetry::InstrumentProvider;
use opentelemetry::metrics::Meter;
use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::metrics::PeriodicReader;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::metrics::data::AggregatedMetrics;
use opentelemetry_sdk::metrics::data::MetricData;
use tokio::sync::watch;

const WINDOW: Duration = Duration::from_secs(1);

/// The failure of a test read.
#[derive(Clone, Debug, PartialEq)]
enum Failure {
    Read,
    Lost,
}

impl From<ReadTaskFailed> for Failure {
    fn from(_failed: ReadTaskFailed) -> Self {
        Self::Lost
    }
}

type Reads = SharedReads<u32, Arc<u32>, Failure>;

/// The future of a test read.
type ReadFuture = std::pin::Pin<Box<dyn Future<Output = Result<Arc<u32>, Failure>> + Send>>;

/// An instrument provider whose instruments a test collects.
struct Recorder {
    provider: SdkMeterProvider,
    exporter: InMemoryMetricExporter,
}

impl InstrumentProvider for Recorder {
    fn namespace(&self) -> &'static str {
        "test"
    }

    fn meter(&self) -> Meter {
        self.provider.meter("shared_read_test")
    }
}

impl Recorder {
    fn new() -> Self {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        Self { provider, exporter }
    }

    /// The value of each data point of the counter `name`, by its `served` label.
    fn counter_by_served(&self, name: &str) -> Vec<(String, u64)> {
        let mut points = self.collect(name, |data| match data {
            AggregatedMetrics::U64(MetricData::Sum(sum)) => sum
                .data_points()
                .map(|point| {
                    let served = point
                        .attributes()
                        .find(|attribute| attribute.key.as_str() == "served")
                        .map_or_else(String::new, |attribute| attribute.value.to_string());
                    (served, point.value())
                })
                .collect::<Vec<_>>(),
            other => panic!("{name} is not a u64 sum: {other:?}"),
        });
        points.sort();
        points
    }

    /// The value of the unlabelled counter `name`.
    fn counter(&self, name: &str) -> u64 {
        self.collect(name, |data| match data {
            AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                sum.data_points().map(|point| point.value()).sum()
            }
            other => panic!("{name} is not a u64 sum: {other:?}"),
        })
    }

    /// The number of values the f64 histogram `name` recorded.
    fn histogram_count(&self, name: &str) -> u64 {
        self.collect(name, |data| match data {
            AggregatedMetrics::F64(MetricData::Histogram(histogram)) => {
                histogram.data_points().map(|point| point.count()).sum()
            }
            other => panic!("{name} is not an f64 histogram: {other:?}"),
        })
    }

    fn collect<T>(&self, name: &str, read: impl FnOnce(&AggregatedMetrics) -> T) -> T {
        self.provider.force_flush().expect("flush metrics");
        let snapshots = self
            .exporter
            .get_finished_metrics()
            .expect("exported metrics");
        let metric = snapshots
            .last()
            .expect("a metrics snapshot")
            .scope_metrics()
            .flat_map(|scope| scope.metrics())
            .find(|metric| metric.name() == name)
            .unwrap_or_else(|| panic!("no {name} metric was exported"));
        read(metric.data())
    }
}

/// Registries whose instruments go nowhere.
fn reads(window: Duration) -> Arc<Reads> {
    SharedReads::new(window, SharedReadMetrics::new(&Recorder::new(), "read"))
        .expect("the window is below the maximum")
}

/// A read source that counts the reads it starts, holds each read until the gate opens, and
/// answers with `outcome`.
struct Source {
    started: Arc<AtomicUsize>,
    gate: watch::Sender<bool>,
}

impl Source {
    fn open() -> Self {
        Self {
            started: Arc::default(),
            gate: watch::Sender::new(true),
        }
    }

    fn closed() -> Self {
        Self {
            started: Arc::default(),
            gate: watch::Sender::new(false),
        }
    }

    fn started(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }

    /// A start function for [`SharedReads::read`] that answers `outcome` once the gate is open.
    fn start(
        &self,
        outcome: Result<u32, Failure>,
    ) -> impl FnOnce(uuid::Uuid) -> ReadFuture + Send + 'static {
        let started = self.started.clone();
        let mut gate = self.gate.subscribe();
        move |_read_id| {
            Box::pin(async move {
                started.fetch_add(1, Ordering::SeqCst);
                gate.wait_for(|open| *open)
                    .await
                    .expect("the source owns the gate");
                outcome.map(Arc::new)
            })
        }
    }
}

/// Lets the spawned read tasks run as far as they can without time passing.
async fn let_tasks_run() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_call_during_a_running_read_joins_it_whatever_its_age() {
    let reads = reads(WINDOW);
    let source = Source::closed();

    let first = reads.read(1, source.start(Ok(7)));
    tokio::time::advance(WINDOW * 5).await;
    let second = reads.read(1, source.start(Ok(8)));
    source.gate.send_replace(true);

    assert_eq!(first.served(), Served::New);
    assert_eq!(second.served(), Served::JoinedRunning);
    assert_eq!(first.id(), second.id());
    assert_eq!(*first.result().await.expect("the read succeeds"), 7);
    assert_eq!(*second.result().await.expect("the read succeeds"), 7);
    assert_eq!(source.started(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_call_within_the_window_of_a_finished_read_reuses_it() {
    let reads = reads(WINDOW);
    let source = Source::open();

    let first = reads.read(1, source.start(Ok(7)));
    first.result().await.expect("the read succeeds");
    tokio::time::advance(WINDOW - Duration::from_millis(1)).await;
    let second = reads.read(1, source.start(Ok(8)));

    assert_eq!(second.served(), Served::ReusedFinished);
    assert_eq!(*second.result().await.expect("the read succeeds"), 7);
    assert_eq!(source.started(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_call_once_the_window_has_passed_starts_a_new_read() {
    let reads = reads(WINDOW);
    let source = Source::open();

    let first = reads.read(1, source.start(Ok(7)));
    let first_id = first.id();
    first.result().await.expect("the read succeeds");
    tokio::time::advance(WINDOW).await;
    let second = reads.read(1, source.start(Ok(8)));

    assert_eq!(second.served(), Served::New);
    assert_ne!(second.id(), first_id);
    assert_eq!(*second.result().await.expect("the read succeeds"), 8);
    assert_eq!(source.started(), 2);
}

#[tokio::test(start_paused = true)]
async fn reads_of_different_keys_are_not_shared() {
    let reads = reads(WINDOW);
    let source = Source::open();

    let first = reads.read(1, source.start(Ok(7)));
    let second = reads.read(2, source.start(Ok(8)));

    assert_eq!(second.served(), Served::New);
    assert_eq!(*first.result().await.expect("the read succeeds"), 7);
    assert_eq!(*second.result().await.expect("the read succeeds"), 8);
}

/// The error is kept for the window from the start of the read, not from its end.
#[tokio::test(start_paused = true)]
async fn a_failed_read_s_error_is_served_until_the_window_has_passed_since_it_started() {
    let reads = reads(WINDOW);
    let source = Source::closed();
    let read_time = Duration::from_millis(300);

    let first = reads.read(1, source.start(Err(Failure::Read)));
    tokio::time::advance(read_time).await;
    source.gate.send_replace(true);
    assert_eq!(first.result().await, Err(Failure::Read));

    tokio::time::advance(WINDOW - read_time - Duration::from_millis(1)).await;
    let kept = reads.read(1, source.start(Ok(8)));
    assert_eq!(kept.served(), Served::KeptError);
    assert_eq!(kept.result().await, Err(Failure::Read));

    tokio::time::advance(Duration::from_millis(1)).await;
    let renewed = reads.read(1, source.start(Ok(8)));
    assert_eq!(renewed.served(), Served::New);
    assert_eq!(*renewed.result().await.expect("the read succeeds"), 8);
    assert_eq!(source.started(), 2);
}

/// The read runs to its end without the cancellation of the request that started it, and a later
/// caller in the window gets its result.
#[tokio::test(start_paused = true)]
async fn a_read_finishes_after_its_starting_caller_goes_away() {
    let reads = reads(WINDOW);
    let source = Source::closed();
    let cancellation = Cancellation::new();
    let saw_cancelled = Arc::new(std::sync::Mutex::new(None));

    let first = {
        let saw_cancelled = saw_cancelled.clone();
        let start = source.start(Ok(7));
        with_request_cancellation(cancellation.clone(), async {
            let execution = setup_execution("test", String::new(), String::new());
            LORE_CONTEXT
                .scope(execution, async {
                    reads.read(1, move |read_id| {
                        Box::pin(async move {
                            let outcome = start(read_id).await;
                            *saw_cancelled.lock().expect("not poisoned") =
                                Some(request_is_cancelled());
                            outcome
                        })
                    })
                })
                .await
        })
        .await
    };
    let_tasks_run().await;
    cancellation.cancel();
    drop(first);
    source.gate.send_replace(true);
    let_tasks_run().await;

    assert_eq!(*saw_cancelled.lock().expect("not poisoned"), Some(false));
    let later = reads.read(1, source.start(Ok(8)));
    assert_eq!(later.served(), Served::ReusedFinished);
    assert_eq!(*later.result().await.expect("the read succeeds"), 7);
    assert_eq!(source.started(), 1);
}

/// Once the window has passed, the registry holds nothing of the read: its result lives only as
/// long as a caller holds it.
#[tokio::test(start_paused = true)]
async fn a_read_removes_its_entry_once_the_window_has_passed() {
    let reads = reads(WINDOW);
    let source = Source::open();

    let result = reads
        .read(1, source.start(Ok(7)))
        .result()
        .await
        .expect("the read succeeds");
    let held = Arc::downgrade(&result);
    drop(result);
    tokio::time::advance(WINDOW - Duration::from_millis(1)).await;
    let_tasks_run().await;
    assert!(
        held.upgrade().is_some(),
        "the entry keeps the result in the window"
    );

    tokio::time::advance(Duration::from_millis(1)).await;
    let_tasks_run().await;
    assert!(
        held.upgrade().is_none(),
        "the entry is removed after the window"
    );
}

/// A window longer than the maximum is refused, so the end of every window is an `Instant`.
#[test]
fn a_window_above_the_maximum_is_refused() {
    let metrics = || SharedReadMetrics::new(&Recorder::new(), "read");

    assert!(Reads::new(MAX_SHARED_READ_WINDOW, metrics()).is_ok());
    for window in [
        MAX_SHARED_READ_WINDOW + Duration::from_nanos(1),
        Duration::MAX,
    ] {
        assert!(
            matches!(Reads::new(window, metrics()), Err(WindowTooLong { .. })),
            "{window:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn with_a_zero_window_only_running_reads_are_shared() {
    let reads = reads(Duration::ZERO);
    let source = Source::closed();

    let first = reads.read(1, source.start(Ok(7)));
    let joined = reads.read(1, source.start(Ok(8)));
    source.gate.send_replace(true);
    assert_eq!(joined.served(), Served::JoinedRunning);
    assert_eq!(*first.result().await.expect("the read succeeds"), 7);
    assert_eq!(*joined.result().await.expect("the read succeeds"), 7);

    let after = reads.read(1, source.start(Ok(8)));
    assert_eq!(after.served(), Served::New);
    assert_eq!(*after.result().await.expect("the read succeeds"), 8);
    assert_eq!(source.started(), 2);
}

/// A read whose task ends without a result fails its callers, and the next call starts a new read.
#[tokio::test(start_paused = true)]
async fn a_lost_read_fails_its_callers_and_is_replaced() {
    let reads = reads(WINDOW);
    let source = Source::open();

    let lost = reads.read(1, |_read_id| -> ReadFuture {
        Box::pin(async { panic!("the read task panics") })
    });
    assert_eq!(lost.result().await, Err(Failure::Lost));
    let_tasks_run().await;
    assert!(!reads.holds(&1), "the lost read removes its entry");

    let next = reads.read(1, source.start(Ok(8)));
    assert_eq!(next.served(), Served::New);
    assert_eq!(*next.result().await.expect("the read succeeds"), 8);
}

/// Callers of one key on many threads at once start one read, so the check of the key's entry and
/// the insert of a new one are one step. The race is repeated, each round on a key of its own, since
/// one round of a version that is not one step starts a single read often enough to pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_calls_for_one_key_start_one_read() {
    const ROUNDS: u32 = 20;
    const CALLERS: usize = 16;
    let reads = reads(WINDOW);
    let runtime = tokio::runtime::Handle::current();

    for key in 0..ROUNDS {
        let source = Source::closed();
        let barrier = std::sync::Barrier::new(CALLERS);
        let calls: Vec<_> = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..CALLERS)
                .map(|_| {
                    scope.spawn(|| {
                        let _runtime = runtime.enter();
                        barrier.wait();
                        reads.read(key, source.start(Ok(7)))
                    })
                })
                .collect();
            threads
                .into_iter()
                .map(|thread| thread.join().expect("the caller does not panic"))
                .collect()
        });
        source.gate.send_replace(true);

        let new = calls
            .iter()
            .filter(|call| call.served() == Served::New)
            .count();
        assert_eq!(new, 1, "one caller of key {key} starts the read");
        assert!(calls.iter().all(|call| call.id() == calls[0].id()));
        for call in calls {
            assert_eq!(*call.result().await.expect("the read succeeds"), 7);
        }
        assert_eq!(source.started(), 1, "one read of key {key} starts");
    }
}

#[tokio::test(start_paused = true)]
async fn the_metrics_count_reads_and_callers_by_how_they_were_served() {
    let recorder = Recorder::new();
    let reads: Arc<Reads> = SharedReads::new(WINDOW, SharedReadMetrics::new(&recorder, "read"))
        .expect("the window is below the maximum");
    let source = Source::closed();

    let new = reads.read(1, source.start(Ok(7)));
    let joined = reads.read(1, source.start(Ok(7)));
    source.gate.send_replace(true);
    new.result().await.expect("the read succeeds");
    joined.result().await.expect("the read succeeds");
    reads
        .read(1, source.start(Ok(7)))
        .result()
        .await
        .expect("the read succeeds");
    reads
        .read(2, source.start(Err(Failure::Read)))
        .result()
        .await
        .expect_err("the read fails");
    reads
        .read(2, source.start(Ok(7)))
        .result()
        .await
        .expect_err("the error is kept");
    reads
        .read(2, source.start(Ok(7)))
        .result()
        .await
        .expect_err("the error is kept");

    assert_eq!(recorder.counter("test.read.started"), 2);
    assert_eq!(
        recorder.counter_by_served("test.read.callers"),
        [
            ("joined_running".to_string(), 1),
            ("kept_error".to_string(), 2),
            ("new".to_string(), 2),
            ("reused_finished".to_string(), 1),
        ]
    );
    assert_eq!(recorder.histogram_count("test.read.duration"), 2);
}

#[tokio::test]
async fn the_future_of_a_result_holds_the_receiver_not_the_whole_read() {
    let reads = reads(WINDOW);
    let source = Source::open();
    let read = reads.read(1, source.start(Ok(7)));
    let (_publish, mut receiver) = watch::channel::<Option<Result<Arc<u32>, Failure>>>(None);
    let receiver_size = size_of_val(&receiver);
    let wait = receiver.wait_for(Option::is_some);

    let result = read.result();

    // The future holds one receiver beside the wait it awaits. A second receiver, which an
    // `async fn` that holds its argument keeps, or the whole read, reaches the bound.
    assert!(
        size_of_val(&result) < 2 * receiver_size + size_of_val(&wait),
        "the future of a result holds {} bytes for a receiver of {} and a wait of {}",
        size_of_val(&result),
        receiver_size,
        size_of_val(&wait)
    );
}
