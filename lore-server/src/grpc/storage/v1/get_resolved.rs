// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `GetResolved`: resolve a mutable key under `KeyType::Resolve` and return the immutable blob it
//! names, in one round trip.
//!
//! Streaming for the same reason `Get` is — the storage API resolves keys in batches — so this
//! mirrors [`super::get`]'s shape: one task per request item, bounded by
//! [`super::STREAM_PROCESS_LIMIT`], each recording the same handler-latency histogram.
//!
//! Errors carry the *key* address in `Status` details, never the resolved one: the client cannot
//! match on a hash it has not learned yet.
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

/// One decoded request item: the key address to resolve and the flags it was requested with.
#[derive(Debug)]
struct ParsedRequest {
    key_address: Address,
    flags: u32,
}

/// The request's `key` field is an [`Address`] whose `hash` is a mutable key rather than a content
/// hash. A missing `key` is a malformed request, not a zero key.
fn parse_request(request: storage_v1::GetResolvedRequest) -> Result<ParsedRequest, Status> {
    let Some(key) = request.key else {
        return Err(Status::invalid_argument(
            "get_resolved: request missing key address",
        ));
    };
    Ok(ParsedRequest {
        key_address: Address::from(&key),
        flags: request.flags,
    })
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
                                Ok(request) => parse_request(request),
                                Err(stream_error) => Err(interpret_streaming_error(stream_error)),
                            };
                            let parsed_address = parsed.as_ref().ok().map(|p| p.key_address);

                            let response = match parsed {
                                Ok(parsed) => {
                                    resolve_item(
                                        parsed,
                                        repository,
                                        correlation_id,
                                        user_id,
                                        mutable_store,
                                        immutable_store,
                                    )
                                    .await
                                }
                                Err(status) => Err(status),
                            };

                            let code = match &response {
                                Ok(_) => Code::Ok,
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
/// the QUIC path; the key address goes in the details either way so the client can route the error.
async fn resolve_item(
    parsed: ParsedRequest,
    repository: lore_revision::lore::RepositoryId,
    correlation_id: String,
    user_id: String,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
) -> Result<storage_v1::GetResolvedResponse, Status> {
    let ParsedRequest { key_address, flags } = parsed;
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
            key: Some(key_address.into()),
            flags,
            resolved: bytes::Bytes::copy_from_slice(response.resolved.as_ref()),
            fragment: Some(response.fragment.into()),
            payload: response.payload,
        }),
        Ok(_) => Err(Status::internal(
            "GetResolved handler returned the wrong response type",
        )),
        Err(e) => Err(match &e {
            MessageHandleError::MutableDataNotFound(_) => Status::with_details(
                Code::NotFound,
                format!("Mutable key not found: {key}"),
                key_address.into(),
            ),
            MessageHandleError::FragmentNotFound => Status::with_details(
                Code::NotFound,
                format!("Key {key} resolved to content that was not found"),
                key_address.into(),
            ),
            err => map_message_handle_error_to_status(
                err,
                Some(format!("Error from get_resolved handler: {e}")),
                Some(key_address.into()),
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
    use lore_proto::lore::model::v1 as model_v1;
    use lore_storage::StoreMatch;
    use rand::random;
    use zerocopy::IntoBytes;

    use super::*;
    use crate::store::test_store_create;

    fn request_for(key_address: Address, flags: u32) -> storage_v1::GetResolvedRequest {
        storage_v1::GetResolvedRequest {
            key: Some(key_address.into()),
            flags,
        }
    }

    #[test]
    fn parse_request_rejects_missing_key() {
        let status = parse_request(storage_v1::GetResolvedRequest {
            key: None,
            flags: 0,
        })
        .expect_err("a request without a key address is malformed");
        assert_eq!(status.code(), Code::InvalidArgument);
    }

    #[test]
    fn parse_request_decodes_key_context_and_flags() {
        let key_address = Address {
            hash: Hash::hash_buffer(b"grpc-resolve-key"),
            context: random::<Context>(),
        };
        let parsed = parse_request(request_for(key_address, 0x00AB_CDEF)).expect("well-formed");
        assert_eq!(parsed.key_address, key_address);
        assert_eq!(parsed.flags, 0x00AB_CDEF);
    }

    /// The client routes stream errors by decoding `Status` details back into the *key* address, so
    /// a miss that omits them (or carries the resolved hash instead) would hang the caller.
    #[tokio::test]
    async fn resolve_item_missing_key_is_not_found_carrying_key_details() {
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
        assert_eq!(
            Address::from(status.details()),
            key_address,
            "details must carry the key address the client requested"
        );
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
        assert_eq!(Address::from(status.details()), key_address);
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
                            },
                            StoreMatch::MatchFull
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
            response.key,
            Some(model_v1::Address::from(key_address)),
            "the response must echo the request key so the client can correlate it"
        );
        assert_eq!(response.flags, 0);
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
