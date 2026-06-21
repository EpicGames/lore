// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_proto::lore::storage::v1 as storage_v1;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::create_operation_context_attribute;
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

use crate::auth::jwt::verify_authorization;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_authorization;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::interpret_streaming_error;
use crate::grpc::log_server_error;
use crate::grpc::map_message_handle_error_to_status;
use crate::grpc::require_write_permission;
use crate::grpc::rpc_code_to_str;
use crate::protocol::storage::copy::handle_copy;
use crate::protocol::storage::messages::LoreResponse;
use crate::protocol::storage::messages::MessageHandleError;
use crate::telemetry::StorageProtocol;
use crate::telemetry::Transport;
use crate::util::setup_execution;

pub type CopyResponseStream =
    Pin<Box<dyn Stream<Item = Result<storage_v1::CopyResponse, Status>> + Send>>;

const METRICS_STREAMING_MESSAGE_HANDLER_LATENCY: &str = "stream.message.handler.duration";

#[tracing::instrument(name = "StorageServiceV1::Copy", skip_all)]
pub async fn handler(
    request: Request<Streaming<storage_v1::CopyRequest>>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<CopyResponseStream>, Status> {
    let destination_repository = get_repository(request.metadata())?;
    require_write_permission(request.extensions(), destination_repository)?;
    let auth_token = get_authorization(request.extensions()).ok();
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
                while let Some(req) = stream.next().await {
                    let permit = match Arc::clone(&task_limiter).acquire_owned().await {
                        Ok(p) => p,
                        Err(error) => {
                            debug!(?error, "Error acquiring copy task permit");
                            break;
                        }
                    };

                    let immutable_store = immutable_store.clone();
                    let tx = tx.clone();
                    let correlation_id = correlation_id.clone();
                    let user_id = user_id.clone();
                    let auth_token = auth_token.clone();
                    let histogram = histogram.clone();

                    let fragment_span = info_span!(
                        parent: None,
                        "StorageCopyItemTask",
                        { SAMPLING_TIER_LOW } = true,
                        { TRANSPORT } = %Transport::Grpc,
                        { PROTOCOL } = %StorageProtocol::StorageV1,
                        { REPOSITORY_ID } = %destination_repository,
                        { CORRELATION_ID } = correlation_id,
                        { USER_ID } = user_id,
                    );

                    lore_spawn!(
                        async move {
                            let start = Instant::now();
                            let metric_context = create_operation_context_attribute("copy");

                            let parsed;
                            if let Err(stream_error) = req {
                                parsed = Err(interpret_streaming_error(stream_error));
                            }
                            else {
                                parsed = req.and_then(|r| {
                                    let source_repository_id = r.source_repository_id.clone();
                                    let target_context_bytes = r.target_context.clone();
                                    r.source_address
                                        .ok_or_else(|| {
                                            Status::invalid_argument(
                                                "CopyRequest.source_address is required",
                                            )
                                        })
                                        .map(|addr| {
                                            let source_address: lore_storage::Address = addr.into();
                                            let target_context = if target_context_bytes.is_empty() {
                                                source_address.context
                                            } else {
                                                lore_storage::Context::from(&target_context_bytes[..])
                                            };
                                            (source_repository_id, source_address, target_context)
                                        })
                                });
                            };

                            let response = match parsed {
                                Ok((source_repo_id, source_address, target_context)) => {
                                    let source_repository: lore_revision::lore::RepositoryId =
                                        source_repo_id.clone().into();

                                    // urc/0.2 path: gRPC carries the JWT in request extensions, so the
                                    // authorization check happens here rather than via the SessionMap
                                    // that handle_copy uses for QUIC v4 callers.
                                    if let Some(token) = auth_token.as_ref()
                                        && let Err(err) =
                                            verify_authorization(token, source_repository)
                                    {
                                        let status = Status::with_details(
                                            Code::PermissionDenied,
                                            err.to_string(),
                                            source_address.into(),
                                        );
                                        Err(status)
                                    } else {
                                        match handle_copy(
                                            source_repository,
                                            source_address,
                                            destination_repository,
                                            target_context,
                                            correlation_id,
                                            user_id,
                                            None,
                                            immutable_store,
                                        )
                                        .await
                                        {
                                            Ok(LoreResponse::Copy(_)) => {
                                                Ok(storage_v1::CopyResponse {
                                                    source_repository_id: source_repo_id,
                                                    source_address: Some(source_address.into()),
                                                })
                                            }
                                            Ok(_) => Err(Status::internal(
                                                "Copy handler returned wrong response type",
                                            )),
                                            Err(err) => Err(match &err {
                                                MessageHandleError::FragmentNotFound => {
                                                    Status::with_details(
                                                        Code::NotFound,
                                                        format!(
                                                            "Source fragment not found: {source_address}"
                                                        ),
                                                        source_address.into(),
                                                    )
                                                }
                                                MessageHandleError::AuthorizationFailure(m) => {
                                                    Status::with_details(
                                                        Code::PermissionDenied,
                                                        m.clone(),
                                                        source_address.into(),
                                                    )
                                                }
                                                err => map_message_handle_error_to_status(
                                                    err,
                                                    Some(format!("Error copying fragment: {err}")),
                                                    Some(source_address.into()),
                                                ),
                                            }),
                                        }
                                    }
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
                                debug!("Error sending copy response: {err}");
                            }
                            drop(permit);
                        }
                        .instrument(fragment_span)
                    );
                }
            });
        })
        .await;

    let recv_stream = ReceiverStream::from(rx);
    Ok(Response::new(Box::pin(recv_stream) as CopyResponseStream))
}
