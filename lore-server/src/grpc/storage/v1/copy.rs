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
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::authnz::repository_authorizer::VerifiedTokenOwned;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::get_verified_token;
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
/// The source-partition check happens here rather than via the `SessionMap` that `handle_copy`
/// uses for QUIC v4 callers, because on the gRPC path the JWT rides in the request extensions.
/// The partition-access layer already checked the destination (the metadata partition); each
/// item's source is this handler's own question to the shared authorizer.
async fn copy_item(
    request: Result<storage_v1::CopyRequest, Status>,
    destination_repository: RepositoryId,
    auth_token: Option<Arc<VerifiedTokenOwned>>,
    repository_authorizer: Arc<dyn RepositoryAuthorizer>,
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

    // A cross-partition copy needs to authorize also the source partition
    let source_check = if source_repository == destination_repository {
        Ok(())
    } else {
        let token = auth_token.as_deref().map(VerifiedTokenOwned::as_token);
        repository_authorizer
            .check_repository_access(token.as_ref(), source_repository, None)
            .await
    };
    let outcome = if let Err(status) = source_check {
        Err(status)
    } else {
        match handle_copy(
            source_repository,
            source_address,
            destination_repository,
            target_context,
            correlation_id,
            user_id,
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
    repository_authorizer: Arc<dyn RepositoryAuthorizer>,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<CopyResponseStream>, Status> {
    let destination_repository = get_repository(request.metadata())?;
    // Owned halves of the verified token: the per-item tasks outlive the
    // request extensions the borrowed form points into.
    let auth_token = get_verified_token(request.extensions()).map(|token| Arc::new(token.owned()));
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
                    let repository_authorizer = repository_authorizer.clone();
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
                                repository_authorizer,
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

#[cfg(test)]
mod tests {
    use lore_base::runtime::LORE_CONTEXT;
    use rand::random;
    use zerocopy::IntoBytes;

    use super::*;
    use crate::auth::jwt::AuthorizationToken;
    use crate::auth::jwt::ResourcePermission;
    use crate::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
    use crate::authnz::repository_authorizer::AuthClientAuthorizer;
    use crate::store::test_store_create;

    fn copy_request(source_repository: RepositoryId) -> storage_v1::CopyRequest {
        storage_v1::CopyRequest {
            source_repository_id: source_repository.as_bytes().to_vec().into(),
            source_address: Some(lore_proto::lore::model::v1::Address {
                hash: vec![0u8; 32].into(),
                context: vec![0u8; 16].into(),
            }),
            target_context: Vec::new().into(),
        }
    }

    /// Both halves of a verified access token whose `resources` claim grants
    /// exactly `resource_ids`.
    fn access_token(resource_ids: &[String]) -> Arc<VerifiedTokenOwned> {
        Arc::new(VerifiedTokenOwned {
            raw: "raw.jwt".to_string(),
            claims: AuthorizationToken {
                resources: Some(
                    resource_ids
                        .iter()
                        .map(|resource_id| ResourcePermission {
                            resource_id: resource_id.clone(),
                            permission: vec![],
                        })
                        .collect(),
                ),
                ..Default::default()
            },
        })
    }

    async fn item_code(
        auth_token: Option<Arc<VerifiedTokenOwned>>,
        repository_authorizer: Arc<dyn RepositoryAuthorizer>,
        source_repository: RepositoryId,
        destination_repository: RepositoryId,
    ) -> i32 {
        let (store, _mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        LORE_CONTEXT
            .scope(execution, async move {
                let response = copy_item(
                    Ok(copy_request(source_repository)),
                    destination_repository,
                    auth_token,
                    repository_authorizer,
                    "correlation".to_string(),
                    "user".to_string(),
                    store,
                )
                .await
                .expect("per-item outcomes travel in-band");
                response.status.expect("every item reports a status").code as i32
            })
            .await
    }

    /// The cross-partition shape on the legacy tier: the access token's
    /// `resources` claim holds the destination but not the source, and the
    /// claim is answered in place — the URL points nowhere, so reaching for
    /// the network would error instead of denying.
    #[tokio::test]
    async fn source_without_a_grant_is_denied_in_band() {
        let source = random::<RepositoryId>();
        let destination = random::<RepositoryId>();
        let authorizer: Arc<dyn RepositoryAuthorizer> = Arc::new(AuthClientAuthorizer::new(
            "https://auth.invalid".to_string(),
        ));
        let code = item_code(
            Some(access_token(&[format!("urc-{destination}")])),
            authorizer,
            source,
            destination,
        )
        .await;
        assert_eq!(code, Code::PermissionDenied as i32);
    }

    /// The same claim with the source granted passes the check and reaches
    /// the store, which answers `NOT_FOUND` for the absent address — so the
    /// denial above is the source check, not the missing fragment.
    #[tokio::test]
    async fn granted_source_reaches_the_store() {
        let source = random::<RepositoryId>();
        let destination = random::<RepositoryId>();
        let authorizer: Arc<dyn RepositoryAuthorizer> = Arc::new(AuthClientAuthorizer::new(
            "https://auth.invalid".to_string(),
        ));
        let code = item_code(
            Some(access_token(&[
                format!("urc-{destination}"),
                format!("urc-{source}"),
            ])),
            authorizer,
            source,
            destination,
        )
        .await;
        assert_eq!(code, Code::NotFound as i32);
    }

    /// An in-partition item — the dedup hot path — never asks the
    /// authorizer: the partition-access layer already answered for the
    /// destination. Proven with a token granting nothing at all.
    #[tokio::test]
    async fn in_partition_item_skips_the_authorizer() {
        let repository = random::<RepositoryId>();
        let authorizer: Arc<dyn RepositoryAuthorizer> = Arc::new(AuthClientAuthorizer::new(
            "https://auth.invalid".to_string(),
        ));
        let code = item_code(Some(access_token(&[])), authorizer, repository, repository).await;
        assert_eq!(code, Code::NotFound as i32);
    }

    /// No `[server.auth]`: no interceptor ran, so no token — the allow-all
    /// authorizer keeps cross-partition copy open exactly as today.
    #[tokio::test]
    async fn tokenless_caller_stays_open_under_allow_all() {
        let code = item_code(
            None,
            Arc::new(AllowAllRepositoryAuthorizer),
            random::<RepositoryId>(),
            random::<RepositoryId>(),
        )
        .await;
        assert_eq!(code, Code::NotFound as i32);
    }
}
