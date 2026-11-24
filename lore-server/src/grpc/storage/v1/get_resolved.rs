// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `GetResolved`: resolve a mutable key under `KeyType::Resolve` and return the immutable blob it
//! names, in one round trip.
//!
//! Streaming for the same reason `Get` is — the storage API resolves keys in batches — so this
//! mirrors [`super::get`]'s shape: one task per request item, bounded by
//! [`super::STREAM_PROCESS_LIMIT`], each recording the same handler-latency histogram.
//!
//! Per-item outcomes are carried **in-band**, in the response's `status` field, and correlated by
//! `request_id`. This follows the QUIC transport rather than the other gRPC storage streams: QUIC's
//! `CommandHeader` reports each command's fate with an `error` bit plus `size_or_status` against a
//! `command_id`, and the stream survives. The `Err(Status)` shape used by `get`/`put`/`copy` cannot
//! express a per-item failure here — tonic turns a server stream's first `Err` into HTTP/2 trailers
//! and ends the stream, which for `get_resolved` would mean the first cache miss discards every
//! request queued behind it. A miss is the expected case for this command, not an exceptional one.
//!
//! So `Err(Status)` is reserved for genuinely stream-fatal conditions: a request that fails to
//! decode, or one whose `request_id` is zero and therefore cannot be answered in-band.
use std::pin::Pin;
use std::pin::pin;
use std::sync::Arc;
use std::time::Instant;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_proto::lore::storage::v1 as storage_v1;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::create_operation_context_attribute;
use lore_telemetry::tracing::fields::ADDRESS;
use lore_telemetry::tracing::fields::CORRELATION_ID;
use lore_telemetry::tracing::fields::PROTOCOL;
use lore_telemetry::tracing::fields::REPOSITORY_ID;
use lore_telemetry::tracing::fields::SAMPLING_TIER_LOW;
use lore_telemetry::tracing::fields::TRANSPORT;
use lore_telemetry::tracing::fields::USER_ID;
use opentelemetry::KeyValue;
use opentelemetry_semantic_conventions::attribute::RPC_GRPC_STATUS_CODE;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio_stream::Stream;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Code;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tonic::Streaming;
use tracing::Instrument;
use tracing::debug;
use tracing::info_span;

use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::interpret_streaming_error;
use crate::grpc::log_server_error;
use crate::grpc::map_message_handle_error_to_status;
use crate::grpc::rpc_code_to_str;
use crate::grpc::until_request_cancelled;
use crate::protocol::storage::get_resolved::handle_get_resolved;
use crate::protocol::storage::messages::LoreResponse;
use crate::protocol::storage::messages::MessageHandleError;
use crate::telemetry::StorageProtocol;
use crate::telemetry::Transport;
use crate::util::setup_execution;

pub type GetResolvedResponseStream =
    Pin<Box<dyn Stream<Item = Result<storage_v1::GetResolvedResponse, Status>> + Send>>;

const METRICS_STREAMING_MESSAGE_HANDLER_LATENCY: &str = "stream.message.handler.duration";

/// One decoded request item: the correlation handle, the key address to resolve, and the flags it
/// was requested with.
#[lore_macro::test_pub]
#[derive(Debug)]
struct ParsedRequest {
    request_id: u64,
    key_address: Address,
    flags: u32,
}

/// A request that cannot be correlated back to a caller. The only condition the server answers
/// with a stream-level error, because an in-band response would have nowhere to go.
#[lore_macro::test_pub]
#[derive(Debug)]
struct Uncorrelatable(Status);

/// The request's `key` field is an [`Address`] whose `hash` is a mutable key rather than a content
/// hash. A zero `request_id` is uncorrelatable; a missing `key` is reported in-band against the id.
#[lore_macro::test_pub]
fn parse_request(
    request: storage_v1::GetResolvedRequest,
) -> Result<Result<ParsedRequest, (u64, Status)>, Uncorrelatable> {
    if request.request_id == 0 {
        return Err(Uncorrelatable(Status::invalid_argument(
            "get_resolved: request_id must be non-zero",
        )));
    }
    let Some(key) = request.key else {
        return Ok(Err((
            request.request_id,
            Status::invalid_argument("get_resolved: request missing key address"),
        )));
    };
    Ok(Ok(ParsedRequest {
        request_id: request.request_id,
        key_address: Address::from(&key),
        flags: request.flags,
    }))
}

/// Build the in-band failure response for `request_id`. Payload-bearing fields stay unset.
#[lore_macro::test_pub]
fn error_response(request_id: u64, status: &Status) -> storage_v1::GetResolvedResponse {
    storage_v1::GetResolvedResponse {
        request_id,
        status: Some(lore_proto::lore::model::v1::ItemStatus {
            code: status.code() as u32,
            message: status.message().to_string(),
        }),
        resolved: bytes::Bytes::new(),
        fragment: None,
        payload: bytes::Bytes::new(),
    }
}

#[tracing::instrument(name = "StorageServiceV1::GetResolved", skip_all)]
pub async fn handler(
    request: Request<Streaming<storage_v1::GetResolvedRequest>>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<GetResolvedResponseStream>, Status> {
    let repository = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let stream = request.into_inner();

    let (tx, rx) = mpsc::channel(super::STREAM_PROCESS_LIMIT);

    let execution = setup_execution(module_path!(), correlation_id.clone(), user_id.clone());

    let histogram = Arc::new(
        instrument_provider.latency_histogram_ms(METRICS_STREAMING_MESSAGE_HANDLER_LATENCY),
    );

    LORE_CONTEXT
        .scope(execution, async move {
            lore_spawn!(async move {
                let task_limiter = Arc::new(Semaphore::new(super::STREAM_PROCESS_LIMIT));
                // Ends once nobody reads the answers. An item already started finishes, so no
                // store operation is cut off part way.
                let mut stream = pin!(until_request_cancelled(stream));
                while let Some(request) = stream.next().await {
                    let permit = match Arc::clone(&task_limiter).acquire_owned().await {
                        Ok(p) => p,
                        Err(error) => {
                            debug!(?error, "Error acquiring get_resolved task permit");
                            break;
                        }
                    };

                    let mutable_store = mutable_store.clone();
                    let immutable_store = immutable_store.clone();
                    let tx = tx.clone();
                    let correlation_id = correlation_id.clone();
                    let user_id = user_id.clone();
                    let histogram = histogram.clone();

                    let item_span = info_span!(
                        parent: None,
                        "StorageGetResolvedItemTask",
                        { SAMPLING_TIER_LOW } = true,
                        { TRANSPORT } = %Transport::Grpc,
                        { PROTOCOL } = %StorageProtocol::StorageV1,
                        { REPOSITORY_ID } = %repository,
                        { CORRELATION_ID } = correlation_id,
                        { USER_ID } = user_id,
                    );

                    lore_spawn!(
                        async move {
                            let start = Instant::now();
                            let metric_context = create_operation_context_attribute("get_resolved");

                            let parsed = match request {
                                Ok(request) => {
                                    parse_request(request).map_err(|Uncorrelatable(status)| status)
                                }
                                Err(stream_error) => Err(interpret_streaming_error(stream_error)),
                            };
                            let parsed_address = parsed
                                .as_ref()
                                .ok()
                                .and_then(|p| p.as_ref().ok())
                                .map(|p| p.key_address);

                            let response = match parsed {
                                Ok(Ok(parsed)) => {
                                    let request_id = parsed.request_id;
                                    match resolve_item(
                                        parsed,
                                        repository,
                                        correlation_id,
                                        user_id,
                                        mutable_store,
                                        immutable_store,
                                    )
                                    .await
                                    {
                                        Ok(response) => Ok(response),
                                        Err(status) => {
                                            log_server_error(&status);
                                            Ok(error_response(request_id, &status))
                                        }
                                    }
                                }
                                Ok(Err((request_id, status))) => {
                                    log_server_error(&status);
                                    Ok(error_response(request_id, &status))
                                }
                                Err(status) => Err(status),
                            };

                            let code = match &response {
                                Ok(response) => response
                                    .status
                                    .as_ref()
                                    .map_or(Code::Ok, |s| Code::from_i32(s.code as i32)),
                                Err(status) => {
                                    log_server_error(status);
                                    status.code()
                                }
                            };
                            let elapsed_ms = start.elapsed().as_millis() as f64;
                            histogram.record(
                                elapsed_ms,
                                &[
                                    KeyValue::new(RPC_GRPC_STATUS_CODE, rpc_code_to_str(&code)),
                                    metric_context,
                                ],
                            );

                            if let Err(err) = tx.send(response).await {
                                debug!(err = ?err,
                                    {{ ADDRESS }} = ?parsed_address,
                                    "Error sending response for resolved key"
                                );
                            }
                            drop(permit);
                        }
                        .instrument(item_span)
                    );
                }
            });
        })
        .await;

    let recv_stream = ReceiverStream::from(rx);
    Ok(Response::new(
        Box::pin(recv_stream) as GetResolvedResponseStream
    ))
}

/// Resolve one item. Both a missing key and a key whose blob is absent map to `NotFound`, matching
/// the QUIC path. The returned `Status` is reported in-band against the request id by the caller,
/// so no `Status` details are needed to route it.
#[lore_macro::test_pub]
async fn resolve_item(
    parsed: ParsedRequest,
    repository: lore_revision::lore::RepositoryId,
    correlation_id: String,
    user_id: String,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
) -> Result<storage_v1::GetResolvedResponse, Status> {
    let ParsedRequest {
        request_id,
        key_address,
        flags,
    } = parsed;
    let key: Hash = key_address.hash;
    let context: Context = key_address.context;

    match handle_get_resolved(
        key,
        context,
        flags,
        repository,
        correlation_id,
        user_id,
        mutable_store,
        immutable_store,
    )
    .await
    {
        Ok(LoreResponse::GetResolved(response)) => Ok(storage_v1::GetResolvedResponse {
            request_id,
            status: None,
            resolved: bytes::Bytes::copy_from_slice(response.resolved.as_ref()),
            fragment: Some(response.fragment.into()),
            payload: response.payload,
        }),
        Ok(_) => Err(Status::internal(
            "GetResolved handler returned the wrong response type",
        )),
        Err(e) => Err(match &e {
            MessageHandleError::MutableDataNotFound(_) => {
                Status::not_found(format!("Mutable key not found: {key}"))
            }
            MessageHandleError::FragmentNotFound => {
                Status::not_found(format!("Key {key} resolved to content that was not found"))
            }
            err => map_message_handle_error_to_status(
                err,
                Some(format!("Error from get_resolved handler: {e}")),
                None,
            ),
        }),
    }
}
