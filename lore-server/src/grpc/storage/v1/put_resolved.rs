// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `PutResolved`: store a fragment and publish a mutable key naming it, in one round trip. The
//! write side of [`super::get_resolved`].
//!
//! Shares that module's shape exactly — per-item outcomes in the response's `status` field,
//! correlated by `request_id`, with `Err(Status)` reserved for requests that cannot be answered
//! in-band. See [`super::get_resolved`] for why the `Err(Status)` form the other storage streams
//! use cannot express a per-item failure.
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Fragment;
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
use crate::protocol::storage::messages::LoreResponse;
use crate::protocol::storage::put::Put;
use crate::protocol::storage::put::UnvalidatedPut;
use crate::protocol::storage::put_resolved::handle_put_resolved;
use crate::telemetry::StorageProtocol;
use crate::telemetry::Transport;
use crate::util::setup_execution;

pub type PutResolvedResponseStream =
    Pin<Box<dyn Stream<Item = Result<storage_v1::PutResolvedResponse, Status>> + Send>>;

const METRICS_STREAMING_MESSAGE_HANDLER_LATENCY: &str = "stream.message.handler.duration";

/// One decoded request item.
#[derive(Debug)]
struct ParsedRequest {
    request_id: u64,
    key: Hash,
    address: Address,
    /// `None` when `address.hash` is zero, i.e. the request removes the mapping.
    put: Option<Put>,
}

/// A request that cannot be correlated back to a caller; see [`super::get_resolved`].
#[derive(Debug)]
struct Uncorrelatable(Status);

/// A zero `request_id` is uncorrelatable. Everything else — a missing address or fragment, a zero
/// key, fragment metadata that fails validation — is reported in-band against the id.
///
/// A zero `address.hash` is a deletion: no fragment is required or validated, because there is
/// nothing to store.
fn parse_request(
    request: storage_v1::PutResolvedRequest,
) -> Result<Result<ParsedRequest, (u64, Status)>, Uncorrelatable> {
    if request.request_id == 0 {
        return Err(Uncorrelatable(Status::invalid_argument(
            "put_resolved: request_id must be non-zero",
        )));
    }
    let request_id = request.request_id;

    let key = Hash::from(&request.key[..]);
    if key.is_zero() {
        return Ok(Err((
            request_id,
            Status::invalid_argument("put_resolved: key must be non-zero"),
        )));
    }

    let Some(address) = request.address else {
        return Ok(Err((
            request_id,
            Status::invalid_argument("put_resolved: request missing address"),
        )));
    };
    let address = Address::from(&address);

    let put = if address.hash.is_zero() {
        None
    } else {
        let Some(fragment) = request.fragment else {
            return Ok(Err((
                request_id,
                Status::invalid_argument("put_resolved: request missing fragment"),
            )));
        };
        if request.payload.is_empty() {
            return Ok(Err((
                request_id,
                Status::invalid_argument("put_resolved: publishing requires a payload"),
            )));
        }
        let payload = Some(request.payload);
        match (UnvalidatedPut {
            address,
            fragment: Fragment::from(&fragment),
            payload,
        })
        .validate()
        {
            Ok(put) => Some(put),
            Err(err) => {
                return Ok(Err((
                    request_id,
                    Status::invalid_argument(format!("put_resolved: invalid fragment: {err}")),
                )));
            }
        }
    };

    Ok(Ok(ParsedRequest {
        request_id,
        key,
        address,
        put,
    }))
}

/// Build the in-band failure response for `request_id`.
fn error_response(request_id: u64, status: &Status) -> storage_v1::PutResolvedResponse {
    storage_v1::PutResolvedResponse {
        request_id,
        status: Some(lore_proto::lore::model::v1::ItemStatus {
            code: status.code() as u32,
            message: status.message().to_string(),
        }),
    }
}

#[tracing::instrument(name = "StorageServiceV1::PutResolved", skip_all)]
pub async fn handler(
    request: Request<Streaming<storage_v1::PutResolvedRequest>>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<PutResolvedResponseStream>, Status> {
    let repository = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let mut stream = request.into_inner();

    let (tx, rx) = mpsc::channel(super::STREAM_PROCESS_LIMIT);

    let execution = setup_execution(module_path!(), correlation_id.clone(), user_id.clone());

    let histogram = Arc::new(
        instrument_provider.latency_histogram_ms(METRICS_STREAMING_MESSAGE_HANDLER_LATENCY),
    );

    LORE_CONTEXT
        .scope(execution, async move {
            lore_spawn!(async move {
                let task_limiter = Arc::new(Semaphore::new(super::STREAM_PROCESS_LIMIT));
                while let Some(request) = stream.next().await {
                    let permit = match Arc::clone(&task_limiter).acquire_owned().await {
                        Ok(p) => p,
                        Err(error) => {
                            debug!(?error, "Error acquiring put_resolved task permit");
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
                        "StoragePutResolvedItemTask",
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
                            let metric_context = create_operation_context_attribute("put_resolved");

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
                                .map(|p| p.address);

                            let response = match parsed {
                                Ok(Ok(parsed)) => {
                                    let request_id = parsed.request_id;
                                    match store_item(
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
                                    "Error sending response for published key"
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
        Box::pin(recv_stream) as PutResolvedResponseStream
    ))
}

/// Store one item's fragment and publish its key. The returned `Status` is reported in-band
/// against the request id by the caller.
async fn store_item(
    parsed: ParsedRequest,
    repository: lore_revision::lore::RepositoryId,
    correlation_id: String,
    user_id: String,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
) -> Result<storage_v1::PutResolvedResponse, Status> {
    let ParsedRequest {
        request_id,
        key,
        address,
        put,
    } = parsed;

    match handle_put_resolved(
        key,
        put.as_ref(),
        address,
        repository,
        correlation_id,
        user_id,
        mutable_store,
        immutable_store,
    )
    .await
    {
        Ok(LoreResponse::PutResolved(_)) => Ok(storage_v1::PutResolvedResponse {
            request_id,
            status: None,
        }),
        Ok(_) => Err(Status::internal(
            "PutResolved handler returned the wrong response type",
        )),
        Err(e) => Err(map_message_handle_error_to_status(
            &e,
            Some(format!("Error from put_resolved handler: {e}")),
            None,
        )),
    }
}

#[cfg(test)]
mod tests {
    use lore_base::types::FragmentFlags;
    use lore_base::types::KeyType;
    use rand::random;

    use super::*;
    use crate::store::test_store_create;

    const TEST_REQUEST_ID: u64 = 11;

    fn request_for(key: Hash, payload: &[u8]) -> storage_v1::PutResolvedRequest {
        let address = Address {
            hash: lore_storage::hash_slice(payload),
            context: Default::default(),
        };
        let fragment = Fragment {
            flags: FragmentFlags::PayloadStoredLocal.bits(),
            size_payload: payload.len() as u32,
            size_content: payload.len() as u64,
        };
        storage_v1::PutResolvedRequest {
            request_id: TEST_REQUEST_ID,
            key: bytes::Bytes::copy_from_slice(key.as_ref()),
            address: Some(address.into()),
            fragment: Some(fragment.into()),
            payload: bytes::Bytes::copy_from_slice(payload),
        }
    }

    #[test]
    fn parse_request_rejects_zero_request_id_as_uncorrelatable() {
        let mut request = request_for(Hash::hash_buffer(b"k"), b"payload");
        request.request_id = 0;
        let Uncorrelatable(status) =
            parse_request(request).expect_err("a zero id has nowhere to send an in-band failure");
        assert_eq!(status.code(), Code::InvalidArgument);
    }

    #[test]
    fn parse_request_reports_zero_key_in_band() {
        let request = request_for(Hash::default(), b"payload");
        let (request_id, status) = parse_request(request)
            .expect("a zero key is correlatable, so not stream-fatal")
            .expect_err("a zero key is not storable");
        assert_eq!(request_id, TEST_REQUEST_ID);
        assert_eq!(status.code(), Code::InvalidArgument);
    }

    #[test]
    fn parse_request_reports_missing_address_in_band() {
        let mut request = request_for(Hash::hash_buffer(b"k"), b"payload");
        request.address = None;
        let (request_id, status) = parse_request(request)
            .expect("correlatable")
            .expect_err("an address is required");
        assert_eq!(request_id, TEST_REQUEST_ID);
        assert_eq!(status.code(), Code::InvalidArgument);
    }

    #[test]
    fn parse_request_reports_missing_payload_in_band() {
        let mut request = request_for(Hash::hash_buffer(b"k"), b"payload");
        request.payload = bytes::Bytes::new();
        let (request_id, status) = parse_request(request)
            .expect("correlatable")
            .expect_err("publishing without a payload would leave a dangling mapping");
        assert_eq!(request_id, TEST_REQUEST_ID);
        assert_eq!(status.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn store_item_publishes_the_key() {
        let repository = random::<lore_revision::lore::RepositoryId>();
        let payload = b"grpc-put-resolved".as_slice();
        let key = Hash::hash_buffer(b"grpc-publish-key");
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let parsed = parse_request(request_for(key, payload))
            .expect("correlatable")
            .expect("well-formed");
        let expected_hash = parsed.address.hash;

        LORE_CONTEXT
            .scope(execution, async move {
                let response = store_item(
                    parsed,
                    repository,
                    String::new(),
                    String::new(),
                    mutable_store.clone(),
                    immutable_store,
                )
                .await
                .expect("put_resolved must succeed");

                assert_eq!(response.request_id, TEST_REQUEST_ID);
                assert!(
                    response.status.is_none(),
                    "a successful item carries no status"
                );

                let mapped = mutable_store
                    .load(repository, key, KeyType::Resolve)
                    .await
                    .expect("the key must be published");
                assert_eq!(mapped, expected_hash);
            })
            .await;
    }
}
