// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::RepositoryId;
use lore_proto::lore::storage::v1 as storage_v1;
use lore_telemetry::InstrumentProvider;
use lore_telemetry::create_operation_context_attribute;
use lore_telemetry::tracing::fields::CORRELATION_ID;
use lore_telemetry::tracing::fields::PROTOCOL;
use lore_telemetry::tracing::fields::REPOSITORY_ID;
use lore_telemetry::tracing::fields::SAMPLING_TIER_LOW;
use lore_telemetry::tracing::fields::TRANSPORT;
use lore_telemetry::tracing::fields::USER_ID;
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

use super::log_and_code;
use super::record_latency;
use crate::auth::jwt::verify_authorization;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_authorization;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::interpret_streaming_error;
use crate::grpc::map_message_handle_error_to_status;
use crate::protocol::storage::copy::handle_copy;
use crate::protocol::storage::messages::LoreResponse;
use crate::protocol::storage::messages::MessageHandleError;
use crate::telemetry::StorageProtocol;
use crate::telemetry::Transport;
use crate::util::setup_execution;

pub type CopyResponseStream =
    Pin<Box<dyn Stream<Item = Result<storage_v1::CopyResponse, Status>> + Send>>;

const METRICS_STREAMING_MESSAGE_HANDLER_LATENCY: &str = "stream.message.handler.duration";

/// `Err` covers the two stream-fatal cases: a request that won't decode, and one with no source
/// address, neither of which can be attributed to an item. Everything else travels in-band — a
/// missing source fragment in particular is an expected outcome that the caller's tier-2 upload
/// fallback pattern-matches on, so it must not be fatal to the stream.
///
/// The authorization check happens here rather than via the `SessionMap` that `handle_copy`
/// uses for QUIC v4 callers, because on the urc/0.2 path gRPC carries the JWT in the request
/// extensions.
async fn copy_item(
    request: Result<storage_v1::CopyRequest, Status>,
    destination_repository: RepositoryId,
    auth_token: Option<crate::auth::jwt::AuthorizationToken>,
    correlation_id: String,
    user_id: String,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
) -> Result<storage_v1::CopyResponse, Status> {
    let request = request.map_err(interpret_streaming_error)?;
    let source_address: lore_storage::Address = request
        .source_address
        .ok_or_else(|| Status::invalid_argument("CopyRequest.source_address is required"))?
        .into();
    let target_context = if request.target_context.is_empty() {
        source_address.context
    } else {
        lore_storage::Context::from(&request.target_context[..])
    };
    let source_repository: RepositoryId = request.source_repository_id.clone().into();

    let outcome = if let Some(token) = auth_token.as_ref()
        && let Err(err) = verify_authorization(token, source_repository)
    {
        Err(Status::new(Code::PermissionDenied, err.to_string()))
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
            Ok(LoreResponse::Copy(_)) => Ok(()),
            Ok(_) => Err(Status::internal(
                "Copy handler returned wrong response type",
            )),
            Err(err) => Err(match &err {
                MessageHandleError::FragmentNotFound => Status::new(
                    Code::NotFound,
                    format!("Source fragment not found: {source_address}"),
                ),
                MessageHandleError::AuthorizationFailure(m) => {
                    Status::new(Code::PermissionDenied, m.clone())
                }
                err => map_message_handle_error_to_status(
                    err,
                    Some(format!("Error copying fragment: {err}")),
                    None,
                ),
            }),
        }
    };

    Ok(storage_v1::CopyResponse {
        source_repository_id: request.source_repository_id,
        source_address: Some(source_address.into()),
        status: Some(match outcome {
            Ok(()) => lore_proto::lore::model::v1::ItemStatus::ok(),
            Err(ref status) => status.into(),
        }),
    })
}

#[tracing::instrument(name = "StorageServiceV1::Copy", skip_all)]
pub async fn handler(
    request: Request<Streaming<storage_v1::CopyRequest>>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<CopyResponseStream>, Status> {
    let destination_repository = get_repository(request.metadata())?;
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

                            let outcome = copy_item(
                                req,
                                destination_repository,
                                auth_token,
                                correlation_id,
                                user_id,
                                immutable_store,
                            )
                            .await;

                            let code = log_and_code(&outcome);
                            record_latency(&histogram, start, code, metric_context);

                            if let Err(err) = tx.send(outcome).await {
                                debug!(err = ?err, "Error sending copy response");
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
