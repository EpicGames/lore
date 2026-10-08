// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::borrow::Cow;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::OnceLock;

use http::StatusCode;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Histogram;
use opentelemetry::metrics::Meter;
use opentelemetry::metrics::UpDownCounter;
use opentelemetry_semantic_conventions::attribute::RPC_GRPC_STATUS_CODE;
use opentelemetry_semantic_conventions::attribute::USER_AGENT_NAME;
use opentelemetry_semantic_conventions::metric::HTTP_SERVER_ACTIVE_REQUESTS;
use opentelemetry_semantic_conventions::metric::HTTP_SERVER_REQUEST_DURATION;
use opentelemetry_semantic_conventions::metric::RPC_SERVER_DURATION;
use opentelemetry_semantic_conventions::trace::HTTP_REQUEST_METHOD;
use opentelemetry_semantic_conventions::trace::HTTP_RESPONSE_STATUS_CODE;
use opentelemetry_semantic_conventions::trace::HTTP_ROUTE;
use opentelemetry_semantic_conventions::trace::RPC_METHOD;
use opentelemetry_semantic_conventions::trace::RPC_SERVICE;
use tonic::Code;

use super::meter;

const METER_SCOPE: &str = "grpc";
static METER: OnceLock<Meter> = OnceLock::new();

fn seconds_histogram(name: impl Into<Cow<'static, str>>) -> Histogram<f64> {
    METER
        .get_or_init(|| meter(METER_SCOPE))
        .f64_histogram(name)
        .with_boundaries(vec![
            0.01, 0.05, 0.1, 0.2, 0.3, 0.4, 0.5, 1.2, 2.0, 5.0, 8.0, 10.0, 15.0, 20.0, 32.0, 40.0,
            60.0,
        ])
        .with_unit("s")
        .build()
}

fn up_down_counter(name: impl Into<Cow<'static, str>>) -> UpDownCounter<i64> {
    METER
        .get_or_init(|| meter(METER_SCOPE))
        .i64_up_down_counter(name)
        .build()
}

// Http metrics
pub static HTTP_SERVER_ACTIVE_REQUESTS_METRIC: LazyLock<UpDownCounter<i64>> =
    LazyLock::new(|| up_down_counter(HTTP_SERVER_ACTIVE_REQUESTS));
pub static HTTP_SERVER_REQUEST_DURATION_METRIC: LazyLock<Histogram<f64>> =
    LazyLock::new(|| seconds_histogram(HTTP_SERVER_REQUEST_DURATION));

// Grpc metrics
pub static RPC_SERVER_DURATION_METRIC: LazyLock<Histogram<f64>> =
    LazyLock::new(|| seconds_histogram(RPC_SERVER_DURATION));
/// Latency of an RPC counted from the server reading the request body to its end, where the two
/// convention histograms count from the request headers arriving. A unary handler runs once its
/// request has been read, so the gap between them holds the client's transmission of the request
/// and any work done before the body is read, such as authorization, and a client slow to send
/// skews only those two.
pub const RPC_SERVER_HANDLER_DURATION: &str = "rpc.server.handler.duration";
pub static RPC_SERVER_HANDLER_DURATION_METRIC: LazyLock<Histogram<f64>> =
    LazyLock::new(|| seconds_histogram(RPC_SERVER_HANDLER_DURATION));

pub(crate) struct GrpcRequestMetrics {
    method: KeyValue,
    path: KeyValue,
    rpc_service: KeyValue,
    rpc_method: KeyValue,
    user_agent: KeyValue,
}

/// Helper struct to track the lifetime metrics for the request
impl GrpcRequestMetrics {
    pub fn new(method: &str, path: &str, user_agent: Arc<str>) -> Self {
        let mut path = path;
        if path.starts_with("/") {
            path = &path[1..];
        }
        let (rpc_service, rpc_method) = path.split_once('/').unwrap_or(("", path));

        Self {
            method: KeyValue::new(HTTP_REQUEST_METHOD, Arc::from(method)),
            path: KeyValue::new(HTTP_ROUTE, Arc::from(path)),
            rpc_service: KeyValue::new(RPC_SERVICE, Arc::from(rpc_service)),
            rpc_method: KeyValue::new(RPC_METHOD, Arc::from(rpc_method)),
            user_agent: KeyValue::new(USER_AGENT_NAME, user_agent),
        }
    }

    fn get_common_attributes(&self) -> [KeyValue; 5] {
        [
            self.method.clone(),
            self.path.clone(),
            self.rpc_service.clone(),
            self.rpc_method.clone(),
            self.user_agent.clone(),
        ]
    }

    pub fn request_started(&self) {
        HTTP_SERVER_ACTIVE_REQUESTS_METRIC.add(1, &self.get_common_attributes());
    }

    pub fn request_complete(
        &self,
        elapsed_seconds: f64,
        handler_elapsed_seconds: f64,
        rpc_code: Code,
        status: Option<StatusCode>,
    ) {
        let rpc_code = KeyValue::new(RPC_GRPC_STATUS_CODE, format!("{rpc_code:?}"));
        let mut attributes = [
            &self.get_common_attributes()[..],
            std::slice::from_ref(&rpc_code),
        ]
        .concat();

        if let Some(status) =
            status.map(|status| KeyValue::new(HTTP_RESPONSE_STATUS_CODE, format!("{status:?}")))
        {
            attributes.push(status);
        }

        HTTP_SERVER_REQUEST_DURATION_METRIC.record(elapsed_seconds, &attributes);
        RPC_SERVER_DURATION_METRIC.record(elapsed_seconds, &attributes);
        RPC_SERVER_HANDLER_DURATION_METRIC.record(handler_elapsed_seconds, &attributes);
    }

    pub fn request_finished(&self) {
        HTTP_SERVER_ACTIVE_REQUESTS_METRIC.add(-1, &self.get_common_attributes());
    }
}
