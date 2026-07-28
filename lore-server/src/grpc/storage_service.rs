// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use bytes::BytesMut;
use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::runtime::runtime;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_base::types::TypedBytesMut;
use lore_revision::lore::execution_context;
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
use tracing::instrument;
use zerocopy::IntoBytes;

use super::extract_correlation_id;
use super::interpret_streaming_error;
use super::metadata_to_attribute;
use super::rpc_code_to_str;
use super::send_err;
use super::simple_map_message_handle_error;
use super::warn_error_to_status;
use crate::grpc::get_user_id;
use crate::legacy::rpc::storage_service_server::StorageService;
use crate::protocol::attribute_map::get_user_id_from_context;
use crate::protocol::attribute_map::repository_id_from_context;
use crate::protocol::storage::messages::LoreResponse;
use crate::protocol::storage::messages::Message;
use crate::protocol::storage::messages::MessageHandleError;
use crate::protocol::storage::put::UnvalidatedPut;
use crate::protocol::storage::requests::*;
use crate::telemetry::StorageProtocol;
use crate::telemetry::Transport;
use crate::util::setup_execution;

type GetResponseStream =
    Pin<Box<dyn Stream<Item = Result<lore_proto::GetResponse, Status>> + Send>>;
type PutResponseStream =
    Pin<Box<dyn Stream<Item = Result<lore_proto::PutResponse, Status>> + Send>>;
type CopyResponseStream =
    Pin<Box<dyn Stream<Item = Result<lore_proto::CopyResponse, Status>> + Send>>;

const METRICS_STREAMING_MESSAGE_HANDLER_LATENCY: &str = "stream.message.handler.duration";

#[derive(Clone)]
pub struct LoreStorageService {
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    local_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
}

impl LoreStorageService {
    pub fn new(
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        local_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
    ) -> Self {
        Self {
            immutable_store,
            local_store,
            mutable_store,
        }
    }

    pub fn local_immutable_store(&self) -> &Arc<dyn lore_storage::ImmutableStore> {
        &self.local_store
    }

    pub fn immutable_store(&self) -> &Arc<dyn lore_storage::ImmutableStore> {
        &self.immutable_store
    }

    pub fn mutable_store(&self) -> &Arc<dyn lore_storage::MutableStore> {
        &self.mutable_store
    }
}

#[tonic::async_trait]
impl StorageService for LoreStorageService {
    type GetStream = GetResponseStream;

    #[instrument(name = "LoreStorageService::Get", skip_all)]
    async fn get(
        &self,
        request: Request<Streaming<lore_proto::Address>>,
    ) -> Result<Response<Self::GetStream>, Status> {
        let attrs = Arc::new(metadata_to_attribute(
            request.metadata(),
            request.extensions(),
        )?);
        let user_id = get_user_id(request.extensions());
        let correlation_id = extract_correlation_id(&request).unwrap_or_default();
        let mut stream = request.into_inner();

        // TODO(psharpe): Make channel capacity configurable
        let (tx, rx) = mpsc::channel(8192);
        let immutable_store = self.immutable_store.clone();

        let execution = setup_execution(module_path!(), correlation_id.clone(), user_id);

        let histogram =
            Arc::new(self.latency_histogram_ms(METRICS_STREAMING_MESSAGE_HANDLER_LATENCY));

        runtime().spawn(LORE_CONTEXT.scope(execution, async move {
            while let Some(request) = stream.next().await {
                let immutable_store = immutable_store.clone();
                let tx = tx.clone();
                let attrs = attrs.clone();
                let correlation_id = correlation_id.clone();

                let histogram = histogram.clone();

                let fragment_span = info_span!(
                    parent: None,
                    "StorageGetItemTask",
                    { SAMPLING_TIER_LOW } = true,
                    { TRANSPORT } = %Transport::Grpc,
                    { PROTOCOL } = %StorageProtocol::StorageV0,
                    { REPOSITORY_ID } = repository_id_from_context(&attrs),
                    { CORRELATION_ID } = correlation_id,
                    { USER_ID } = get_user_id_from_context(&attrs),
                );

                runtime().spawn(
                    LORE_CONTEXT.scope(
                        execution_context(),
                        async move {
                            let start = Instant::now();
                            let metric_context = create_operation_context_attribute("get");

                            let request = match request {
                                Ok(address) => Get {
                                    address: address.into(),
                                },
                                Err(status) => {
                                    let status = interpret_streaming_error(status);
                                    let elapsed_ms = start.elapsed().as_millis() as f64;
                                    histogram.record(
                                        elapsed_ms,
                                        &[
                                            KeyValue::new(
                                                RPC_GRPC_STATUS_CODE,
                                                rpc_code_to_str(&status.code()),
                                            ),
                                            metric_context,
                                        ],
                                    );
                                    return send_err(status, tx).await;
                                }
                            };

                            let response = match request.handle(attrs, immutable_store).await {
                                Ok(LoreResponse::Get(response)) => Ok(lore_proto::GetResponse {
                                    address: Some(request.address.into()),
                                    fragment: Some(response.fragment.into()),
                                    payload: response.payload,
                                }),
                                Ok(_) => panic!("Get handler returned the wrong response type"),
                                Err(e) => Err(match e {
                                    MessageHandleError::FragmentNotFound => Status::with_details(
                                        Code::NotFound,
                                        format!("Fragment not found: {}", request.address),
                                        request.address.into(),
                                    ),
                                    default => warn_error_to_status(&default, |err| {
                                        Status::with_details(
                                            Code::Internal,
                                            format!("Error from get handler: {err}"),
                                            request.address.into(),
                                        )
                                    }),
                                }),
                            };

                            let code = match &response {
                                Ok(_) => Code::Ok,
                                Err(status) => status.code(),
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
                                    {{ ADDRESS }} = %request.address,
                                    "Error sending response for fragment"
                                );
                            }
                        }
                        .instrument(fragment_span),
                    ),
                );
            }
        }));

        let recv_stream = ReceiverStream::from(rx);

        Ok(Response::new(Box::pin(recv_stream) as Self::GetStream))
    }

    async fn ping(
        &self,
        request: Request<lore_proto::PingRequest>,
    ) -> Result<Response<lore_proto::PingResponse>, Status> {
        let remote_addr = request.remote_addr();
        let req = request.into_inner();

        debug!("Ping from {:?}: {}", remote_addr, req.timestamp);

        Ok(Response::new(lore_proto::PingResponse {
            timestamp: req.timestamp,
        }))
    }

    type PutStream = PutResponseStream;

    #[instrument(name = "LoreStorageService::Put", skip_all)]
    async fn put(
        &self,
        request: Request<Streaming<lore_proto::PutRequest>>,
    ) -> Result<Response<Self::PutStream>, Status> {
        let attrs = Arc::new(metadata_to_attribute(
            request.metadata(),
            request.extensions(),
        )?);
        let user_id = get_user_id(request.extensions());
        let correlation_id = extract_correlation_id(&request).unwrap_or_default();

        // Stream of fragments
        let mut stream = request.into_inner();

        // TODO(psharpe): make this capacity configurable or unbounded
        let (tx, rx) = mpsc::channel(8192);
        let immutable_store = self.immutable_store.clone();

        let execution = setup_execution(module_path!(), correlation_id.clone(), user_id);

        let histogram =
            Arc::new(self.latency_histogram_ms(METRICS_STREAMING_MESSAGE_HANDLER_LATENCY));

        runtime().spawn(LORE_CONTEXT.scope(execution, async move {
            while let Some(req) = stream.next().await {
                let immutable_store = immutable_store.clone();
                let tx = tx.clone();
                let attrs = attrs.clone();
                let correlation_id = correlation_id.clone();

                let histogram = histogram.clone();

                let fragment_span = info_span!(
                    parent: None,
                    "StoragePutItemTask",
                    { SAMPLING_TIER_LOW } = true,
                    { TRANSPORT } = %Transport::Grpc,
                    { PROTOCOL } = %StorageProtocol::StorageV0,
                    { REPOSITORY_ID } = repository_id_from_context(&attrs),
                    { CORRELATION_ID } = correlation_id,
                    { USER_ID } = get_user_id_from_context(&attrs),
                );

                // Spawn task to store fragment - may want to just do this one at a time,
                // we'll see how this performs at scale
                runtime().spawn(
                    LORE_CONTEXT.scope(
                        execution_context(),
                        async move {
                                    let start = Instant::now();
                                    let metric_context = create_operation_context_attribute("put");

                            let request;
                            if let Err(streaming_error) = req {
                                request = Err(interpret_streaming_error(streaming_error));
                            }
                            else {
                                request = req.and_then(|r| {
                                    r.address
                                        .zip(r.fragment)
                                        .map(|(address, fragment)| UnvalidatedPut {
                                            address: address.into(),
                                            fragment: fragment.into(),
                                            payload: r.payload,
                                        })
                                        .ok_or(Status::invalid_argument(
                                            "Missing required field, both address and fragment must be present",
                                        )).and_then(|unvalidated| {
                                        unvalidated.validate().map_err(|_e| Status::invalid_argument("Payload failed validation"))
                                    })
                                });
                            }


                                    if let Err(err) = request {
                                        let elapsed_ms = start.elapsed().as_millis() as f64;
                                        histogram.record(
                                            elapsed_ms,
                                            &[
                                                KeyValue::new(
                                                    RPC_GRPC_STATUS_CODE,
                                                    rpc_code_to_str(&err.code()),
                                                ),
                                                metric_context,
                                            ],
                                        );
                                        return send_err(err, tx).await;
                                    }
                                    let request = request.unwrap();

                                    let response = request.handle(attrs, immutable_store).await;

                                    let response = match response {
                                        Ok(LoreResponse::Put(_)) => Ok(lore_proto::PutResponse {
                                            address: Some(request.address().into()),
                                        }),
                                        Ok(_) => Err(Status::internal(
                                            "Put handler returned the wrong response type",
                                        )),
                                        Err(err) => {
                                            let response = warn_error_to_status(&err, |err| {
                                                Status::internal(format!(
                                                    "Error storing fragment {}: {err}",
                                                    request.address()
                                                ))
                                            });
                                            Err(response)
                                        }
                                    };

                                    let code = match &response {
                                        Ok(_) => Code::Ok,
                                        Err(status) => status.code(),
                                    };
                                    let elapsed_ms = start.elapsed().as_millis() as f64;
                                    histogram.record(
                                        elapsed_ms,
                                        &[
                                            KeyValue::new(
                                                RPC_GRPC_STATUS_CODE,
                                                rpc_code_to_str(&code),
                                            ),
                                            metric_context,
                                        ],
                                    );

                                    if let Err(err) = tx.send(response).await {
                                        debug!(
                                            "Error sending put response for {}: {err}",
                                            request.address()
                                        );
                                    }
                                }
                                .instrument(fragment_span),
                            ),
                        );
                    }
                }
                .in_current_span(),
            ),
        );

        // Wrap the result channel in a stream for the client to read
        let recv_stream = ReceiverStream::from(rx);

        Ok(Response::new(Box::pin(recv_stream) as Self::PutStream))
    }

    #[instrument(name = "LoreStorageService::Query", skip_all)]
    async fn query(
        &self,
        request: Request<lore_proto::QueryRequest>,
    ) -> Result<Response<lore_proto::QueryResponse>, Status> {
        let user_id = get_user_id(request.extensions());
        let correlation_id = extract_correlation_id(&request).unwrap_or_default();

        let execution = setup_execution(module_path!(), correlation_id, user_id);

        LORE_CONTEXT
            .scope(execution, async move {
                let context = Arc::new(metadata_to_attribute(
                    request.metadata(),
                    request.extensions(),
                )?);
                let req = request.into_inner();

                if req.addresses.len() > crate::protocol::storage::query::MAX_FRAGMENTS {
                    return Err(Status::invalid_argument(format!(
                        "too many addresses: {} exceeds limit {}",
                        req.addresses.len(),
                        crate::protocol::storage::query::MAX_FRAGMENTS,
                    )));
                }

                let mut address = BytesMut::with_count_capacity::<Address>(req.addresses.len());
                for addr in req.addresses {
                    address.extend_from_slice(
                        Address {
                            hash: Hash::from(addr.hash),
                            context: Context::from(addr.context),
                        }
                        .as_bytes(),
                    );
                }

                let msg = Query {
                    address: address.freeze(),
                };

                msg.handle(context, self.immutable_store.clone())
                    .await
                    .map(|resp| {
                        let LoreResponse::Query(resp) = resp else {
                            panic!("Query handler returned the wrong response type");
                        };

                        let results = resp.results.iter().map(|res| *res as i32).collect();
                        Response::new(lore_proto::QueryResponse { results })
                    })
                    .map_err(simple_map_message_handle_error)
            })
            .await
    }

    type CopyStream = CopyResponseStream;

    #[instrument(name = "LoreStorageService::Copy", skip_all)]
    async fn copy(
        &self,
        request: Request<Streaming<lore_proto::CopyRequest>>,
    ) -> Result<Response<Self::CopyStream>, Status> {
        let attrs = Arc::new(metadata_to_attribute(
            request.metadata(),
            request.extensions(),
        )?);
        let user_id = get_user_id(request.extensions());
        let correlation_id = extract_correlation_id(&request).unwrap_or_default();

        let mut stream = request.into_inner();

        let (tx, rx) = mpsc::channel(8192);
        let immutable_store = self.immutable_store.clone();
        let execution = setup_execution(module_path!(), correlation_id.clone(), user_id);
        let histogram =
            Arc::new(self.latency_histogram_ms(METRICS_STREAMING_MESSAGE_HANDLER_LATENCY));

        runtime().spawn(
            LORE_CONTEXT.scope(
                execution,
                async move {
                    while let Some(req) = stream.next().await {
                        let immutable_store = immutable_store.clone();
                        let tx = tx.clone();
                        let attrs = attrs.clone();
                        let histogram = histogram.clone();

                        let fragment_span = info_span!(
                            parent: None,
                            "StorageCopyItemTask",
                            { SAMPLING_TIER_LOW } = true,
                            { TRANSPORT } = %Transport::Grpc,
                            { PROTOCOL } = %StorageProtocol::StorageV0,
                            { REPOSITORY_ID } = repository_id_from_context(&attrs),
                            { CORRELATION_ID } = correlation_id,
                            { USER_ID } = get_user_id_from_context(&attrs),
                        );

                        lore_spawn!(
                            async move {
                                let start = Instant::now();
                                let metric_context = create_operation_context_attribute("copy");

                                let request;
                                if let Err(streaming_error) = req {
                                    request = Err(interpret_streaming_error(streaming_error));
                                } else {
                                    request = req.and_then(|r| {
                                        let source_repository_id = r.source_repository_id.clone();
                                        let target_context_bytes = r.target_context.clone();
                                        r.source_address
                                            .ok_or_else(|| {
                                                Status::invalid_argument(
                                                    "CopyRequest.source_address is required",
                                                )
                                            })
                                            .map(|addr| {
                                                let source_address: lore_storage::Address =
                                                    addr.into();
                                                // Empty target_context preserves legacy semantics:
                                                // destination context = source context. Lore-storage/0.4
                                                // callers send a non-empty value to relocate the dedup
                                                // tag without payload transfer.
                                                let target_context =
                                                    if target_context_bytes.is_empty() {
                                                        source_address.context
                                                    } else {
                                                        lore_storage::Context::from(
                                                            &target_context_bytes[..],
                                                        )
                                                    };
                                                Copy {
                                                    source_repository: source_repository_id.into(),
                                                    source_address,
                                                    target_context,
                                                }
                                            })
                                    });
                                }

                                let response = match request {
                                    Ok(msg) => {
                                        let resp = msg.handle(attrs, immutable_store).await;
                                        match resp {
                                            Ok(LoreResponse::Copy(_)) => {
                                                Ok(lore_proto::CopyResponse {
                                                    source_address: Some(msg.source_address.into()),
                                                })
                                            }
                                            Ok(_) => Err(Status::internal(
                                                "Copy handler returned wrong response type",
                                            )),
                                            Err(err) => {
                                                // Encode the failed source address in the
                                                // status's details so the streaming client
                                                // can correlate the error to the right
                                                // inflight request. Without this, the client
                                                // (`storage_client::spawn_copy_stream`) has
                                                // no way to map the error back and the
                                                // request future hangs.
                                                let response = warn_error_to_status(&err, |err| {
                                                    match err {
                                                        MessageHandleError::FragmentNotFound => {
                                                            Status::with_details(
                                                                Code::NotFound,
                                                                format!(
                                                                    "Source fragment not found: {}",
                                                                    msg.source_address
                                                                ),
                                                                msg.source_address.into(),
                                                            )
                                                        }
                                                        MessageHandleError::AuthorizationFailure(
                                                            m,
                                                        ) => Status::with_details(
                                                            Code::PermissionDenied,
                                                            m.clone(),
                                                            msg.source_address.into(),
                                                        ),
                                                        _ => Status::with_details(
                                                            Code::Internal,
                                                            format!(
                                                                "Error copying fragment: {err}"
                                                            ),
                                                            msg.source_address.into(),
                                                        ),
                                                    }
                                                });
                                                Err(response)
                                            }
                                        }
                                    }
                                    Err(status) => Err(status),
                                };

                                let code = match &response {
                                    Ok(_) => Code::Ok,
                                    Err(status) => status.code(),
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
                                    debug!("Error sending copy response: {err}");
                                }
                            }
                            .instrument(fragment_span)
                        );
                    }
                }
                .in_current_span(),
            ),
        );

        let recv_stream = ReceiverStream::from(rx);
        Ok(Response::new(Box::pin(recv_stream) as Self::CopyStream))
    }

    #[instrument(name = "LoreStorageService::Verify", skip_all)]
    async fn verify(
        &self,
        request: Request<lore_proto::VerifyRequest>,
    ) -> Result<Response<lore_proto::VerifyResponse>, Status> {
        let user_id = get_user_id(request.extensions());
        let correlation_id = extract_correlation_id(&request).unwrap_or_default();

        let execution = setup_execution(module_path!(), correlation_id, user_id);

        LORE_CONTEXT
            .scope(
                execution,
                async move {
                    let context = Arc::new(metadata_to_attribute(
                        request.metadata(),
                        request.extensions(),
                    )?);
                    let req = request.into_inner();

                    let address: Address = req
                        .address
                        .ok_or_else(|| Status::invalid_argument("Missing address"))?
                        .into();

                    let msg = Verify {
                        address,
                        heal: if req.heal { 1 } else { 0 },
                    };

                    msg.handle(context, self.local_store.clone())
                        .await
                        .map(|resp| {
                            let LoreResponse::Verify(resp) = resp else {
                                panic!("Verify handler returned the wrong response type");
                            };

                            Response::new(lore_proto::VerifyResponse {
                                corrupted: resp.corrupted != 0,
                                healed: resp.healed as i32,
                            })
                        })
                        .map_err(simple_map_message_handle_error)
                }
                .in_current_span(),
            )
            .await
    }

    #[instrument(name = "LoreStorageService::MutableLoad", skip_all)]
    async fn mutable_load(
        &self,
        request: Request<lore_proto::MutableLoadRequest>,
    ) -> Result<Response<lore_proto::MutableLoadResponse>, Status> {
        let mutable_store = self.mutable_store.clone();

        let user_id = get_user_id(request.extensions());
        let correlation_id = extract_correlation_id(&request).unwrap_or_default();
        let execution = setup_execution(module_path!(), correlation_id, user_id);

        LORE_CONTEXT
            .scope(execution, async move {
                let context = Arc::new(metadata_to_attribute(
                    request.metadata(),
                    request.extensions(),
                )?);
                let req = request.into_inner();

                let key = Hash::from(&req.key[..]);
                let key_type = KeyType::try_from(req.key_type).map_err(|_err| {
                    Status::invalid_argument(format!("Invalid key_type: {}", req.key_type))
                })?;
                let msg = crate::protocol::storage::requests::MutableLoad { key, key_type };

                msg.handle_mutable(context, mutable_store)
                    .await
                    .map(|resp| {
                        let LoreResponse::MutableLoad(resp) = resp else {
                            panic!("MutableLoad handler returned the wrong response type");
                        };
                        Response::new(lore_proto::MutableLoadResponse {
                            value: bytes::Bytes::copy_from_slice(resp.value.as_ref()),
                        })
                    })
                    .map_err(|e| match e {
                        MessageHandleError::MutableDataNotFound(_) => {
                            Status::not_found("Mutable key not found")
                        }
                        other => simple_map_message_handle_error(other),
                    })
            })
            .await
    }

    #[instrument(name = "LoreStorageService::MutableStore", skip_all)]
    async fn mutable_store(
        &self,
        request: Request<lore_proto::MutableStoreRequest>,
    ) -> Result<Response<lore_proto::MutableStoreResponse>, Status> {
        let mutable_store = self.mutable_store.clone();

        let user_id = get_user_id(request.extensions());
        let correlation_id = extract_correlation_id(&request).unwrap_or_default();
        let execution = setup_execution(module_path!(), correlation_id, user_id);

        LORE_CONTEXT
            .scope(execution, async move {
                let context = Arc::new(metadata_to_attribute(
                    request.metadata(),
                    request.extensions(),
                )?);
                let req = request.into_inner();

                let key = Hash::from(&req.key[..]);
                let value = Hash::from(&req.value[..]);
                let key_type = KeyType::try_from(req.key_type).map_err(|_err| {
                    Status::invalid_argument(format!("Invalid key_type: {}", req.key_type))
                })?;
                let msg = crate::protocol::storage::requests::MutableStoreOp {
                    key,
                    value,
                    key_type,
                };

                msg.handle_mutable(context, mutable_store)
                    .await
                    .map(|_| Response::new(lore_proto::MutableStoreResponse {}))
                    .map_err(simple_map_message_handle_error)
            })
            .await
    }

    #[instrument(name = "LoreStorageService::MutableCompareAndSwap", skip_all)]
    async fn mutable_compare_and_swap(
        &self,
        request: Request<lore_proto::MutableCompareAndSwapRequest>,
    ) -> Result<Response<lore_proto::MutableCompareAndSwapResponse>, Status> {
        let mutable_store = self.mutable_store.clone();

        let user_id = get_user_id(request.extensions());
        let correlation_id = extract_correlation_id(&request).unwrap_or_default();
        let execution = setup_execution(module_path!(), correlation_id, user_id);

        LORE_CONTEXT
            .scope(execution, async move {
                let context = Arc::new(metadata_to_attribute(
                    request.metadata(),
                    request.extensions(),
                )?);
                let req = request.into_inner();

                let key = Hash::from(&req.key[..]);
                let expected = Hash::from(&req.expected[..]);
                let value = Hash::from(&req.value[..]);
                let key_type = KeyType::try_from(req.key_type).map_err(|_err| {
                    Status::invalid_argument(format!("Invalid key_type: {}", req.key_type))
                })?;
                let msg = crate::protocol::storage::requests::MutableCas {
                    key,
                    expected,
                    value,
                    key_type,
                };

                msg.handle_mutable(context, mutable_store)
                    .await
                    .map(|resp| {
                        let LoreResponse::MutableCas(resp) = resp else {
                            panic!(
                                "MutableCompareAndSwap handler returned the wrong response type"
                            );
                        };
                        Response::new(lore_proto::MutableCompareAndSwapResponse {
                            current_value: bytes::Bytes::copy_from_slice(
                                resp.current_value.as_ref(),
                            ),
                        })
                    })
                    .map_err(simple_map_message_handle_error)
            })
            .await
    }
}

impl InstrumentProvider for LoreStorageService {
    fn namespace(&self) -> &'static str {
        "urc.grpc.storage_service"
    }
}
