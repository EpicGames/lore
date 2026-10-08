// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::convert::Infallible;
use std::pin::Pin;
use std::pin::pin;
use std::sync::Arc;
use std::sync::LazyLock;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

use bytes::Bytes;
use http::Request;
use http::Response;
use http_body::Body as _;
use http_body::Frame;
use lore_telemetry::grpc_metrics::HTTP_SERVER_REQUEST_DURATION_METRIC;
use lore_telemetry::grpc_metrics::RPC_SERVER_DURATION_METRIC;
use lore_telemetry::grpc_metrics::RPC_SERVER_HANDLER_DURATION;
use lore_telemetry::grpc_metrics::RPC_SERVER_HANDLER_DURATION_METRIC;
use lore_telemetry::grpc_tower_layer::GrpcMetricsLayer;
use lore_telemetry::user_agent_filter::UserAgentFilter;
use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::metrics::PeriodicReader;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::metrics::data::AggregatedMetrics;
use opentelemetry_sdk::metrics::data::MetricData;
use opentelemetry_semantic_conventions::metric::RPC_SERVER_DURATION;
use opentelemetry_semantic_conventions::trace::RPC_METHOD;
use tokio::sync::mpsc;
use tonic::body::Body;
use tower::Layer;
use tower::Service;
use tower::service_fn;

struct Metrics {
    provider: SdkMeterProvider,
    exporter: InMemoryMetricExporter,
}

/// The histograms bind to whichever provider is current when first touched, so they are forced
/// here, while this provider is the current one.
static METRICS: LazyLock<Metrics> = LazyLock::new(|| {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    lore_telemetry::set_meter_provider(provider.clone());
    LazyLock::force(&HTTP_SERVER_REQUEST_DURATION_METRIC);
    LazyLock::force(&RPC_SERVER_DURATION_METRIC);
    LazyLock::force(&RPC_SERVER_HANDLER_DURATION_METRIC);
    Metrics { provider, exporter }
});

/// Returns the recorded count and sum of the named histogram for one RPC method.
fn histogram(name: &str, rpc_method: &str) -> (u64, f64) {
    METRICS.provider.force_flush().expect("flush metrics");
    let snapshots = METRICS
        .exporter
        .get_finished_metrics()
        .expect("exported metrics");
    let metrics = snapshots.last().expect("a metrics snapshot");
    let method = KeyValue::new(RPC_METHOD, rpc_method.to_owned());
    metrics
        .scope_metrics()
        .flat_map(|scope| scope.metrics())
        .filter(|metric| metric.name() == name)
        .find_map(|metric| match metric.data() {
            AggregatedMetrics::F64(MetricData::Histogram(histogram)) => histogram
                .data_points()
                .find(|point| point.attributes().any(|attribute| *attribute == method))
                .map(|point| (point.count(), point.sum())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no {name} data point for {rpc_method}"))
}

/// A request body fed from a channel: each message is a data frame, closing the channel ends
/// the stream.
struct ChannelBody(mpsc::UnboundedReceiver<Bytes>);

impl http_body::Body for ChannelBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.0
            .poll_recv(cx)
            .map(|chunk| chunk.map(|bytes| Ok(Frame::data(bytes))))
    }
}

async fn drain(body: Body) {
    let mut body = pin!(body);
    while let Some(frame) = std::future::poll_fn(|cx| body.as_mut().poll_frame(cx)).await {
        frame.expect("request body frame");
    }
}

fn request(rpc_method: &str) -> (Request<Body>, mpsc::UnboundedSender<Bytes>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let request = Request::builder()
        .uri(format!("/test.Service/{rpc_method}"))
        .body(Body::new(ChannelBody(rx)))
        .expect("build request");
    (request, tx)
}

fn layer() -> GrpcMetricsLayer {
    LazyLock::force(&METRICS);
    GrpcMetricsLayer::new(Arc::new(UserAgentFilter::default()))
}

mod handler_duration {
    use super::*;

    const BODY_DELAY: Duration = Duration::from_millis(500);
    const HANDLER_DELAY: Duration = Duration::from_millis(100);

    #[tokio::test]
    async fn counts_from_the_end_of_the_request_when_the_handler_reads_the_body() {
        const METHOD: &str = "ReadsBody";
        let mut service = layer().layer(service_fn(|request: Request<Body>| async move {
            drain(request.into_body()).await;
            tokio::time::sleep(HANDLER_DELAY).await;
            Ok::<_, Infallible>(Response::new(Body::empty()))
        }));
        let (request, tx) = request(METHOD);

        let send_body = async move {
            tokio::time::sleep(BODY_DELAY).await;
            tx.send(Bytes::from_static(b"payload")).expect("send body");
        };
        let (response, ()) = tokio::join!(service.call(request), send_body);
        response.expect("response");

        let (total_count, total) = histogram(RPC_SERVER_DURATION, METHOD);
        let (handler_count, handler) = histogram(RPC_SERVER_HANDLER_DURATION, METHOD);
        assert_eq!(total_count, 1);
        assert_eq!(handler_count, 1);
        assert!(
            total >= (BODY_DELAY + HANDLER_DELAY).as_secs_f64(),
            "total {total}s spans the body delay and the handler"
        );
        assert!(
            handler >= HANDLER_DELAY.as_secs_f64(),
            "handler {handler}s spans the handler"
        );
        assert!(
            handler < BODY_DELAY.as_secs_f64(),
            "handler {handler}s excludes the body delay"
        );
    }

    #[tokio::test]
    async fn counts_from_the_request_start_when_the_body_is_still_open() {
        const METHOD: &str = "LeavesBodyOpen";
        let mut service = layer().layer(service_fn(|_: Request<Body>| async move {
            tokio::time::sleep(HANDLER_DELAY).await;
            Ok::<_, Infallible>(Response::new(Body::empty()))
        }));
        let (request, _tx) = request(METHOD);

        service.call(request).await.expect("response");

        let (total_count, total) = histogram(RPC_SERVER_DURATION, METHOD);
        let (handler_count, handler) = histogram(RPC_SERVER_HANDLER_DURATION, METHOD);
        assert_eq!(total_count, 1);
        assert_eq!(handler_count, 1);
        assert!(
            total >= HANDLER_DELAY.as_secs_f64(),
            "total {total}s spans the handler"
        );
        assert_eq!(handler, total);
    }
}
