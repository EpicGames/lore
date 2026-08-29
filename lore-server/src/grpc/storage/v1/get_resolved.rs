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
#[derive(Debug)]
struct ParsedRequest {
    request_id: u64,
    key_address: Address,
    flags: u32,
}

/// A request that cannot be correlated back to a caller. The only condition the server answers
/// with a stream-level error, because an in-band response would have nowhere to go.
#[derive(Debug)]
struct Uncorrelatable(Status);

/// The request's `key` field is an [`Address`] whose `hash` is a mutable key rather than a content
/// hash. A zero `request_id` is uncorrelatable; a missing `key` is reported in-band against the id.
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

#[cfg(test)]
mod tests {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Fragment;
    use lore_base::types::FragmentFlags;
    use lore_base::types::KeyType;
    use rand::random;
    use zerocopy::IntoBytes;

    use super::*;
    use crate::store::test_store_create;

    const TEST_REQUEST_ID: u64 = 7;

    fn request_for(key_address: Address, flags: u32) -> storage_v1::GetResolvedRequest {
        storage_v1::GetResolvedRequest {
            request_id: TEST_REQUEST_ID,
            key: Some(key_address.into()),
            flags,
        }
    }

    /// A missing key is answered in-band against the request id, not with a stream error — a
    /// stream error would discard every request queued behind it.
    #[test]
    fn parse_request_reports_missing_key_in_band() {
        let (request_id, status) = parse_request(storage_v1::GetResolvedRequest {
            request_id: TEST_REQUEST_ID,
            key: None,
            flags: 0,
        })
        .expect("a missing key is correlatable, so not stream-fatal")
        .expect_err("a request without a key address is malformed");
        assert_eq!(request_id, TEST_REQUEST_ID);
        assert_eq!(status.code(), Code::InvalidArgument);
    }

    /// A zero id is the one request that cannot be answered in-band, so it stays stream-fatal.
    #[test]
    fn parse_request_rejects_zero_request_id_as_uncorrelatable() {
        let Uncorrelatable(status) = parse_request(storage_v1::GetResolvedRequest {
            request_id: 0,
            key: Some(Address::default().into()),
            flags: 0,
        })
        .expect_err("a zero id has nowhere to send an in-band failure");
        assert_eq!(status.code(), Code::InvalidArgument);
    }

    #[test]
    fn parse_request_decodes_id_key_context_and_flags() {
        let key_address = Address {
            hash: Hash::hash_buffer(b"grpc-resolve-key"),
            context: random::<Context>(),
        };
        let parsed = parse_request(request_for(key_address, 0x00AB_CDEF))
            .expect("well-formed")
            .expect("well-formed");
        assert_eq!(parsed.request_id, TEST_REQUEST_ID);
        assert_eq!(parsed.key_address, key_address);
        assert_eq!(parsed.flags, 0x00AB_CDEF);
    }

    #[tokio::test]
    async fn resolve_item_missing_key_is_not_found() {
        let repository = random::<lore_revision::lore::RepositoryId>();
        let key_address = Address {
            hash: Hash::hash_buffer(b"grpc-missing-key"),
            context: random::<Context>(),
        };
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let status = LORE_CONTEXT
            .scope(execution, async move {
                resolve_item(
                    ParsedRequest {
                        request_id: TEST_REQUEST_ID,
                        key_address,
                        flags: 0,
                    },
                    repository,
                    String::new(),
                    String::new(),
                    mutable_store,
                    immutable_store,
                )
                .await
            })
            .await
            .expect_err("nothing maps this key");

        assert_eq!(status.code(), Code::NotFound);
    }

    /// The failure a caller actually receives: an ordinary stream item carrying the id and a
    /// non-OK status. If this were an `Err(Status)` item instead, tonic would send trailers and
    /// every request queued behind it would go unanswered.
    #[test]
    fn error_response_is_an_in_band_item_carrying_the_request_id() {
        let response = error_response(TEST_REQUEST_ID, &Status::not_found("no such key"));
        assert_eq!(response.request_id, TEST_REQUEST_ID);
        let status = response.status.expect("a failed item carries a status");
        assert_eq!(Code::from_i32(status.code as i32), Code::NotFound);
        assert_eq!(status.message, "no such key");
        assert!(response.payload.is_empty());
        assert!(response.resolved.is_empty());
        assert!(response.fragment.is_none());
    }

    #[tokio::test]
    async fn resolve_item_dangling_pointer_is_not_found() {
        let repository = random::<lore_revision::lore::RepositoryId>();
        let key_address = Address {
            hash: Hash::hash_buffer(b"grpc-dangling-key"),
            context: random::<Context>(),
        };
        let never_stored = Hash::hash_buffer(b"grpc-never-stored");
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let status = LORE_CONTEXT
            .scope(execution, async move {
                mutable_store
                    .clone()
                    .store(repository, key_address.hash, never_stored, KeyType::Resolve)
                    .await
                    .expect("store resolve mapping");
                resolve_item(
                    ParsedRequest {
                        request_id: TEST_REQUEST_ID,
                        key_address,
                        flags: 0,
                    },
                    repository,
                    String::new(),
                    String::new(),
                    mutable_store,
                    immutable_store,
                )
                .await
            })
            .await
            .expect_err("the mapping resolves but its blob was never stored");

        assert_eq!(status.code(), Code::NotFound);
    }

    #[tokio::test]
    async fn resolve_item_echoes_request_identity_with_payload() {
        let repository = random::<lore_revision::lore::RepositoryId>();
        let context = random::<Context>();
        let payload = bytes::Bytes::from_static(b"resolved content over grpc");
        let resolved = Hash::hash_buffer(payload.as_ref());
        let key_address = Address {
            hash: Hash::hash_buffer(b"grpc-good-key"),
            context,
        };
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let expected_payload = payload.clone();
        let response = LORE_CONTEXT
            .scope(execution, async move {
                let fragment = Fragment {
                    flags: FragmentFlags::PayloadStoredLocal.bits(),
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                };
                immutable_store
                    .clone()
                    .put(
                        repository,
                        Address {
                            hash: resolved,
                            context,
                        },
                        fragment,
                        Some(payload),
                        false,
                    )
                    .await
                    .expect("store blob");
                debug_assert!(
                    immutable_store
                        .clone()
                        .get(
                            repository,
                            Address {
                                hash: resolved,
                                context
                            }
                        )
                        .await
                        .is_ok()
                );
                mutable_store
                    .clone()
                    .store(repository, key_address.hash, resolved, KeyType::Resolve)
                    .await
                    .expect("store resolve mapping");
                resolve_item(
                    ParsedRequest {
                        request_id: TEST_REQUEST_ID,
                        key_address,
                        flags: 0,
                    },
                    repository,
                    String::new(),
                    String::new(),
                    mutable_store,
                    immutable_store,
                )
                .await
            })
            .await
            .expect("mapping and blob are both present");

        assert_eq!(
            response.request_id, TEST_REQUEST_ID,
            "the response must echo the request id so the client can correlate it"
        );
        assert!(
            response.status.is_none(),
            "a successful item carries no status"
        );
        assert_eq!(response.resolved.as_ref(), resolved.as_bytes());
        assert_eq!(response.payload, expected_payload);
    }

    /// Unknown flag bits currently surface as `Internal`, because the shared handler reports them
    /// as `MessageHandleError::NotImplemented`. Arguably they should be `InvalidArgument` — this
    /// asserts today's behavior so a deliberate fix in the shared handler shows up here.
    #[tokio::test]
    async fn resolve_item_unknown_flags_are_rejected() {
        let repository = random::<lore_revision::lore::RepositoryId>();
        let key_address = Address {
            hash: Hash::hash_buffer(b"grpc-flagged-key"),
            context: random::<Context>(),
        };
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let status = LORE_CONTEXT
            .scope(execution, async move {
                resolve_item(
                    ParsedRequest {
                        request_id: TEST_REQUEST_ID,
                        key_address,
                        flags: 1,
                    },
                    repository,
                    String::new(),
                    String::new(),
                    mutable_store,
                    immutable_store,
                )
                .await
            })
            .await
            .expect_err("no flag bits are defined yet");

        assert_eq!(status.code(), Code::Internal);
    }
}
