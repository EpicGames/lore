// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;

use lore_aws::aws::creds::Credentials;
use lore_aws::aws::s3::S3Client;
use lore_aws::clients::AwsClientBuilder;
use lore_aws::clients::HttpClientSettings;
use lore_aws::clients::StoreKind;
use lore_aws::s3::S3;
use lore_base::runtime::LORE_CONTEXT;
use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::metrics::PeriodicReader;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::metrics::data::AggregatedMetrics;
use opentelemetry_sdk::metrics::data::MetricData;

use crate::aws::test_server::StubResponse;
use crate::aws::test_server::StubServer;

/// A meter provider whose measurements are read back.
struct Metrics {
    provider: SdkMeterProvider,
    exporter: InMemoryMetricExporter,
}

/// The meter provider of this binary, installed when first forced.
///
/// A histogram binds to whichever provider is current when it is made, so a client whose
/// measurements a test reads is built after this is forced.
static METRICS: LazyLock<Metrics> = LazyLock::new(|| {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    lore_telemetry::set_meter_provider(provider.clone());
    Metrics { provider, exporter }
});

/// How many connection waits were recorded with every one of `labels`.
fn connection_waits(labels: &[KeyValue]) -> u64 {
    METRICS.provider.force_flush().expect("the metrics flush");
    let snapshots = METRICS
        .exporter
        .get_finished_metrics()
        .expect("the metrics export");

    snapshots
        .last()
        .into_iter()
        .flat_map(|metrics| metrics.scope_metrics())
        .flat_map(|scope| scope.metrics())
        .filter(|metric| metric.name() == "urc.aws.http.connection_wait")
        .filter_map(|metric| match metric.data() {
            AggregatedMetrics::F64(MetricData::Histogram(histogram)) => Some(histogram),
            _ => None,
        })
        .flat_map(|histogram| histogram.data_points())
        .filter(|point| {
            labels
                .iter()
                .all(|label| point.attributes().any(|attribute| attribute == label))
        })
        .map(|point| point.count())
        .sum()
}

/// The latest data point of `name` recorded with every one of `labels`, read by `value`.
fn latest<T>(
    name: &str,
    labels: &[KeyValue],
    value: impl Fn(&AggregatedMetrics) -> Vec<(Vec<KeyValue>, T)>,
) -> Option<T> {
    METRICS.provider.force_flush().expect("the metrics flush");
    let snapshots = METRICS
        .exporter
        .get_finished_metrics()
        .expect("the metrics export");

    snapshots
        .last()
        .into_iter()
        .flat_map(|metrics| metrics.scope_metrics())
        .flat_map(|scope| scope.metrics())
        .filter(|metric| metric.name() == name)
        .flat_map(|metric| value(metric.data()))
        .find(|(attributes, _)| labels.iter().all(|label| attributes.contains(label)))
        .map(|(_, value)| value)
}

/// How many requests are in flight on the client whose count carries every one of `labels`.
fn requests_in_flight(labels: &[KeyValue]) -> Option<i64> {
    latest(
        "lore.aws.http.requests_in_flight",
        labels,
        |data| match data {
            AggregatedMetrics::I64(MetricData::Sum(sum)) => sum
                .data_points()
                .map(|point| (point.attributes().cloned().collect(), point.value()))
                .collect(),
            _ => vec![],
        },
    )
}

/// The `max_connections` reported for the client whose gauge carries every one of `labels`.
fn max_connections(labels: &[KeyValue]) -> Option<u64> {
    latest("lore.aws.http.max_connections", labels, |data| match data {
        AggregatedMetrics::U64(MetricData::Gauge(gauge)) => gauge
            .data_points()
            .map(|point| (point.attributes().cloned().collect(), point.value()))
            .collect(),
        _ => vec![],
    })
}

/// The S3 client a store's builder makes, sending to `server` with at most `max_connections`
/// connections in use.
///
/// The `S3` facade is a mock in test builds, so the client is taken from the facade's constructor.
/// Its expectation is process-wide, so one client is built at a time.
async fn store_s3_client(
    server: &StubServer,
    store: StoreKind,
    max_connections: Option<usize>,
) -> S3Client {
    static BUILDING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _building = BUILDING.lock().await;

    let built = Arc::new(Mutex::new(None::<S3Client>));
    let new = S3::new_context();
    new.expect().returning({
        let built = Arc::clone(&built);
        move |client, _| {
            *built.lock().expect("the slot locks") = Some(client);
            S3::default()
        }
    });

    AwsClientBuilder::builder()
        .with_http_settings(&HttpClientSettings {
            pool_idle_timeout_seconds: 5,
            nodelay: true,
            max_connections: max_connections.and_then(NonZeroUsize::new),
            connection_acquire_timeout_millis: None,
        })
        .for_store(store)
        .with_static_credentials(Credentials::new("AKIDTEST", "secret-for-tests", None, None))
        .region("us-east-1")
        .endpoint(server.endpoint().as_str())
        .build_config()
        .s3_with_path_style(true)
        .build()
        .await
        .expect("the client builds");

    built
        .lock()
        .expect("the slot locks")
        .take()
        .expect("the builder made an S3 client")
}

/// Wait until the requests in flight under `labels` read `expected`, or fail after two seconds.
///
/// An abandoned request's task is cancelled on the runtime rather than in the caller, so its count
/// drops shortly after the caller lets it go.
async fn until_in_flight(labels: &[KeyValue], expected: i64) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while requests_in_flight(labels) != Some(expected) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "requests in flight stayed at {:?}, not {expected}",
            requests_in_flight(labels)
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// A request is counted in flight from when it is sent until it ends, whether or not the client
/// limits its connections, and the count is labelled with the store and the service.
#[tokio::test]
async fn a_store_client_counts_its_requests_in_flight() {
    LazyLock::force(&METRICS);

    for (store, max_connections) in [(StoreKind::Lock, None), (StoreKind::Mutable, Some(4))] {
        LORE_CONTEXT
            .scope(crate::store::setup_execution("test".to_string()), async {
                let labels = [
                    KeyValue::new("store", store.as_str()),
                    KeyValue::new("service", "s3"),
                ];
                let server = StubServer::silent().await;
                let s3 = store_s3_client(&server, store, max_connections).await;

                tokio::select! {
                    _ = s3.bucket_exists("bucket") => panic!("a silent server answers nothing"),
                    () = async {
                        server.wait_for_requests(1).await;
                        until_in_flight(&labels, 1).await;
                    } => {}
                }

                until_in_flight(&labels, 0).await;
            })
            .await;
    }
}

/// A client with a connection limit reports it, labelled with the store and the service, so its
/// requests in flight can be read as a share of it.
#[tokio::test]
async fn a_store_client_reports_its_connection_limit() {
    LazyLock::force(&METRICS);

    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::start(vec![StubResponse::empty(200)]).await;
            let _s3 = store_s3_client(&server, StoreKind::Lock, Some(7)).await;

            assert_eq!(
                max_connections(&[
                    KeyValue::new("store", "lock"),
                    KeyValue::new("service", "s3"),
                ]),
                Some(7)
            );
        })
        .await;
}

/// A client built for a store labels the time each of its requests waits for a connection with
/// the store and the service it sends to.
#[tokio::test]
async fn a_store_client_labels_its_connection_waits_with_the_store_and_service() {
    LazyLock::force(&METRICS);

    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::start(vec![StubResponse::empty(200)]).await;
            let s3 = store_s3_client(&server, StoreKind::Immutable, Some(4)).await;

            assert!(
                s3.bucket_exists("bucket")
                    .await
                    .expect("the service answers")
            );

            assert_eq!(
                connection_waits(&[
                    KeyValue::new("store", "immutable"),
                    KeyValue::new("service", "s3"),
                ]),
                1
            );
        })
        .await;
}
