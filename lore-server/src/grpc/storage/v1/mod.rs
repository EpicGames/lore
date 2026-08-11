// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod copy;
pub mod get;
pub mod get_metadata;
pub mod mutable_compare_and_swap;
pub mod mutable_load;
pub mod mutable_store;
pub mod put;
pub mod query;
pub mod service;
pub mod verify;

#[cfg(test)]
pub(crate) mod test_utils;

/// Backpressure limit for streaming storage handlers — matches the QUIC public stream handler's 500 per stream × 8 streams = 4000 per connection so gRPC (single stream per connection) gets equivalent per-connection parallelism.
pub(crate) const STREAM_PROCESS_LIMIT: usize = 4000;

/// Shared by the streaming handlers because each reports an item's outcome from two places —
/// an in-band `ItemStatus` and a terminal `Status` — which must land in one histogram series.
pub(crate) fn record_latency(
    histogram: &opentelemetry::metrics::Histogram<f64>,
    start: std::time::Instant,
    code: tonic::Code,
    metric_context: opentelemetry::KeyValue,
) {
    histogram.record(
        start.elapsed().as_millis() as f64,
        &[
            opentelemetry::KeyValue::new(
                opentelemetry_semantic_conventions::attribute::RPC_GRPC_STATUS_CODE,
                crate::grpc::rpc_code_to_str(&code),
            ),
            metric_context,
        ],
    );
}

/// Lets the streaming handlers share one observability path: a per-item failure and a
/// stream-fatal one are logged and counted identically, even though only the latter ends the
/// stream.
pub(crate) trait ItemOutcome {
    /// The item's failure rebuilt as a `Status`, or `None` when the item succeeded.
    fn item_error(&self) -> Option<tonic::Status>;
}

impl ItemOutcome for lore_proto::lore::storage::v1::GetResponse {
    fn item_error(&self) -> Option<tonic::Status> {
        self.status
            .as_ref()
            .filter(|status| !status.is_ok())
            .map(Into::into)
    }
}

impl ItemOutcome for lore_proto::lore::storage::v1::PutResponse {
    fn item_error(&self) -> Option<tonic::Status> {
        self.status
            .as_ref()
            .filter(|status| !status.is_ok())
            .map(Into::into)
    }
}

impl ItemOutcome for lore_proto::lore::storage::v1::CopyResponse {
    fn item_error(&self) -> Option<tonic::Status> {
        self.status
            .as_ref()
            .filter(|status| !status.is_ok())
            .map(Into::into)
    }
}

/// Collapses the two failure shapes — `Ok` carrying an in-band `error`, and `Err` carrying a
/// stream-fatal `Status` — since both are server-visible failures that belong in the same log
/// stream and metric series.
pub(crate) fn log_and_code<T: ItemOutcome>(outcome: &Result<T, tonic::Status>) -> tonic::Code {
    match outcome {
        Ok(response) => match response.item_error() {
            Some(status) => {
                crate::grpc::log_server_error(&status);
                status.code()
            }
            None => tonic::Code::Ok,
        },
        Err(status) => {
            crate::grpc::log_server_error(status);
            status.code()
        }
    }
}
