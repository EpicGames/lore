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

/// A client built for a store labels the time each of its requests waits for a connection with
/// the store and the service it sends to.
///
/// The `S3` facade is a mock in test builds, so the request goes out on the `S3Client` the
/// builder hands the facade's constructor.
#[tokio::test]
async fn a_store_client_labels_its_connection_waits_with_the_store_and_service() {
    LazyLock::force(&METRICS);

    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let server = StubServer::start(vec![StubResponse::empty(200)]).await;
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
                    max_connections: NonZeroUsize::new(4),
                })
                .for_store(StoreKind::Immutable)
                .with_static_credentials(Credentials::new(
                    "AKIDTEST",
                    "secret-for-tests",
                    None,
                    None,
                ))
                .region("us-east-1")
                .endpoint(server.endpoint().as_str())
                .build_config()
                .s3_with_path_style(true)
                .build()
                .await
                .expect("the client builds");
            let s3 = built
                .lock()
                .expect("the slot locks")
                .take()
                .expect("the builder made an S3 client");

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
