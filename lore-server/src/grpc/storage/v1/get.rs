// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
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
use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::interpret_streaming_error;
use crate::grpc::map_message_handle_error_to_status;
use crate::protocol::storage::get::handle_get;
use crate::protocol::storage::messages::LoreResponse;
use crate::protocol::storage::messages::MessageHandleError;
use crate::telemetry::StorageProtocol;
use crate::telemetry::Transport;
use crate::util::setup_execution;

pub type GetResponseStream =
    Pin<Box<dyn Stream<Item = Result<storage_v1::GetResponse, Status>> + Send>>;

const METRICS_STREAMING_MESSAGE_HANDLER_LATENCY: &str = "stream.message.handler.duration";

/// A failure attributable to this address is folded into the returned `GetResponse` as
/// `outcome = error`, so it costs only this request. `Err` is reserved for the stream-fatal
/// case: a request that won't decode leaves no address to attribute a failure to, and nothing
/// further can be read from the request stream.
async fn get_item(
    request: Result<lore_proto::lore::model::v1::Address, Status>,
    repository: RepositoryId,
    correlation_id: String,
    user_id: String,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
) -> Result<storage_v1::GetResponse, Status> {
    let address: Address = request.map_err(interpret_streaming_error)?.into();

    let outcome = match handle_get(
        address,
        repository,
        correlation_id,
        user_id,
        immutable_store,
    )
    .await
    {
        Ok(LoreResponse::Get(response)) => Ok(response),
        Ok(_) => Err(Status::internal(
            "Get handler returned the wrong response type",
        )),
        Err(e) => Err(match &e {
            MessageHandleError::FragmentNotFound => {
                Status::new(Code::NotFound, format!("Fragment not found: {address}"))
            }
            err => map_message_handle_error_to_status(
                err,
                Some(format!("Error from get handler: {e}")),
                None,
            ),
        }),
    };

    Ok(match outcome {
        Ok(response) => storage_v1::GetResponse {
            address: Some(address.into()),
            fragment: Some(response.fragment.into()),
            payload: response.payload,
            status: Some(lore_proto::lore::model::v1::ItemStatus::ok()),
        },
        Err(status) => storage_v1::GetResponse {
            address: Some(address.into()),
            status: Some((&status).into()),
            ..Default::default()
        },
    })
}

#[tracing::instrument(name = "StorageServiceV1::Get", skip_all)]
pub async fn handler(
    request: Request<Streaming<lore_proto::lore::model::v1::Address>>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<GetResponseStream>, Status> {
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
                            debug!(?error, "Error acquiring get task permit");
                            break;
                        }
                    };

                    let immutable_store = immutable_store.clone();
                    let tx = tx.clone();
                    let correlation_id = correlation_id.clone();
                    let user_id = user_id.clone();
                    let histogram = histogram.clone();

                    let fragment_span = info_span!(
                        parent: None,
                        "StorageGetItemTask",
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
                            let metric_context = create_operation_context_attribute("get");

                            let outcome = get_item(
                                request,
                                repository,
                                correlation_id,
                                user_id,
                                immutable_store,
                            )
                            .await;

                            let code = log_and_code(&outcome);
                            record_latency(&histogram, start, code, metric_context);

                            if let Err(err) = tx.send(outcome).await {
                                debug!(err = ?err, "Error sending response for fragment");
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
    Ok(Response::new(Box::pin(recv_stream) as GetResponseStream))
}
