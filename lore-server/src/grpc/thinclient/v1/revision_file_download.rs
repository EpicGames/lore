// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use http::HeaderValue;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Hash;
use lore_proto::lore::model::v1 as model_v1;
use lore_proto::lore::thin_client::v1 as thin_client_v1;
use lore_proto::lore::thin_client::v1::RevisionFileDownloadRequest;
use lore_proto::lore::thin_client::v1::RevisionFileDownloadResponse;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_revision::util::path::RelativePath;
use lore_storage::StoreMatch;
use lore_telemetry::tracing::fields::ADDRESS;
use lore_telemetry::tracing::fields::REPOSITORY_ID;
use lore_telemetry::tracing::fields::REVISION;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::warn;

use super::helpers::resolve_to_identifier;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_authorization;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::link_read_authorizer;
use crate::grpc::warn_error_to_status;
use crate::http::presign_token::CURRENT_TOKEN_VERSION;
use crate::http::presign_token::PresignTokenPayload;
use crate::http::presign_token::sign;
use crate::http::server::PresignConfig;
use crate::util::setup_execution;

const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";

/// `lore.thin_client.v1.ThinClientService.RevisionFileDownload` handler.
///
/// The gRPC request performs all mutable-name resolution and authorization.
/// Its result is pinned to a concrete revision and immutable content address;
/// redemption only streams that logical address through Lore's HTTP server.
#[tracing::instrument(name = "RevisionFileDownload::v1::handle", skip_all)]
pub async fn handler(
    request: Request<RevisionFileDownloadRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    presign_config: Option<PresignConfig>,
) -> Result<Response<RevisionFileDownloadResponse>, Status> {
    let repository_id = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let authorization = get_authorization(request.extensions()).ok();
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let request = request.into_inner();
    let presign_config = presign_config.ok_or_else(|| {
        Status::failed_precondition(
            "logical download URLs are disabled because the Lore HTTP presign feature is not configured",
        )
    })?;

    let Some(query) = request.query else {
        return Err(Status::invalid_argument(
            "RevisionFileDownloadRequest.query must be set (identifier or signature)",
        ));
    };
    let path = RelativePath::new_from_initial_path(&request.path)
        .map_err(|error| Status::invalid_argument(format!("invalid path: {error}")))?;
    if path.is_empty() {
        return Err(Status::invalid_argument("path must name a file"));
    }
    let content_type = if request.content_type.is_empty() {
        DEFAULT_CONTENT_TYPE.to_string()
    } else {
        request.content_type
    };
    HeaderValue::from_str(&content_type)
        .map_err(|_| Status::invalid_argument("content_type is not a valid HTTP header value"))?;

    let can_read = link_read_authorizer(authorization);
    let execution = setup_execution(module_path!(), correlation_id, user_id);
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store,
                repository_id,
            ));
            let (signature, identifier) = resolve_to_identifier(&repository, query.into()).await?;
            if signature.is_zero() {
                return Err(Status::invalid_argument(
                    "cannot download a file from a zeroed revision",
                ));
            }

            let state = load_state(&repository, signature).await?;
            let node_link = state
                .find_node_link(repository.clone(), path.as_str())
                .await
                .map_err(|error| map_path_error(repository_id, signature, &path, error))?;
            if !node_link.is_valid() || !can_read(node_link.repository) {
                return Err(Status::not_found(format!(
                    "file path {} was not found",
                    path.as_str()
                )));
            }

            let resolved_repository = if node_link.repository == repository.id {
                repository.clone()
            } else {
                Arc::new(repository.to_link_context(node_link.repository).await)
            };
            let resolved_state = if node_link.repository == repository.id
                && node_link.revision == state.revision()
            {
                state
            } else {
                load_state(&resolved_repository, node_link.revision).await?
            };
            let node = resolved_state
                .node(resolved_repository.clone(), node_link.node)
                .await
                .map_err(|error| {
                    warn!(
                        {REPOSITORY_ID} = %node_link.repository,
                        {REVISION} = %node_link.revision,
                        path = path.as_str(),
                        ?error,
                        "Failed to load resolved download node",
                    );
                    warn_error_to_status(&error, |e| Status::internal(e.to_string()))
                })?;
            if !node.is_file() {
                return Err(Status::failed_precondition(format!(
                    "path {} is not a file",
                    path.as_str()
                )));
            }

            validate_content_address(
                immutable_store,
                node_link.repository,
                node.address,
                node.size,
            )
            .await?;

            let ttl = if request.ttl_seconds == 0 {
                presign_config.default_ttl_seconds
            } else {
                request.ttl_seconds
            }
            .clamp(
                presign_config.min_ttl_seconds,
                presign_config.max_ttl_seconds,
            );
            let expires_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| Status::internal(format!("system clock error: {error}")))?
                .as_secs()
                .saturating_add(ttl);
            let file_name = path.name().to_string();
            let content_disposition = content_disposition(&file_name, request.inline);
            HeaderValue::from_str(&content_disposition).map_err(|_| {
                Status::internal("generated Content-Disposition is not a valid HTTP header value")
            })?;

            let repository_resolved = node_link.repository;
            let address = node.address;
            let payload = PresignTokenPayload {
                version: CURRENT_TOKEN_VERSION,
                key_id: presign_config.key_id,
                repository: repository_resolved.to_string(),
                address: address.to_string(),
                expires_at,
                content_type: Some(content_type),
                content_encoding: None,
                content_disposition: Some(content_disposition),
                content_length: Some(node.size),
            };
            let token = sign(&payload, &presign_config.hmac_key);
            let url_suffix = format!("/v1/presigned/{repository_resolved}/{address}?token={token}");

            Ok(Response::new(RevisionFileDownloadResponse {
                revision: Some(thin_client_v1::RevisionTreeHeader {
                    identifier: Some(identifier),
                    signature: signature.into(),
                }),
                repository_resolved: bytes::Bytes::copy_from_slice(repository_resolved.data()),
                address: Some(model_v1::Address {
                    hash: address.hash.into(),
                    context: address.context.into(),
                }),
                size: node.size,
                url_suffix,
                expires_at_epoch_seconds: expires_at,
                file_name,
                mode: node.mode as u32,
            }))
        })
        .await
}

async fn load_state(
    repository: &Arc<RepositoryContext>,
    signature: Hash,
) -> Result<Arc<State>, Status> {
    State::deserialize(repository.clone(), signature)
        .await
        .filter_slow_down()?
        .map_err(|error| {
            if error.is_not_found() || error.is_revision_not_found() {
                Status::not_found(format!("Revision {signature} not found"))
            } else {
                warn!(
                    {REPOSITORY_ID} = %repository.id,
                    {REVISION} = %signature,
                    ?error,
                    "Failed to deserialize revision for file download",
                );
                warn_error_to_status(&error, |e| Status::internal(e.to_string()))
            }
        })
}

fn map_path_error(
    repository: RepositoryId,
    revision: Hash,
    path: &RelativePath,
    error: lore_revision::state::StateError,
) -> Status {
    if error.is_not_found() || error.is_node_not_found() || error.is_link_not_found() {
        Status::not_found(format!("file path {} was not found", path.as_str()))
    } else {
        warn!(
            {REPOSITORY_ID} = %repository,
            {REVISION} = %revision,
            path = path.as_str(),
            ?error,
            "Failed to resolve file download path",
        );
        warn_error_to_status(&error, |e| Status::internal(e.to_string()))
    }
}

async fn validate_content_address(
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    repository: RepositoryId,
    address: lore_storage::Address,
    expected_size: u64,
) -> Result<(), Status> {
    if address.hash.is_zero() {
        return if expected_size == 0 {
            Ok(())
        } else {
            Err(Status::data_loss(
                "non-empty file has a zero content address",
            ))
        };
    }

    let query = immutable_store
        .get_metadata(repository, address)
        .await
        .map_err(|error| {
            warn!(
                {REPOSITORY_ID} = %repository,
                {ADDRESS} = %address,
                ?error,
                "Failed to query resolved file content",
            );
            Status::internal(format!("failed to query file content: {error}"))
        })?;
    if query.match_made != StoreMatch::MatchFull {
        return Err(Status::not_found("file content was not found"));
    }
    if query.fragment.size_content != expected_size {
        return Err(Status::data_loss(format!(
            "file node size {expected_size} does not match stored content size {}",
            query.fragment.size_content
        )));
    }
    Ok(())
}

fn content_disposition(file_name: &str, inline: bool) -> String {
    let kind = if inline { "inline" } else { "attachment" };
    let fallback: String = file_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let fallback = if fallback.is_empty() {
        "download"
    } else {
        &fallback
    };
    format!(
        "{kind}; filename=\"{fallback}\"; filename*=UTF-8''{}",
        encode_rfc5987(file_name)
    )
}

fn encode_rfc5987(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            )
        {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use lore_base::types::Context;
    use lore_proto::lore::thin_client::v1::revision_file_download_request::Query;
    use lore_revision::branch;
    use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
    use lore_revision::lore::BranchId;
    use lore_revision::metadata::Metadata;
    use lore_revision::node::Node;
    use lore_revision::node::NodeFlags;
    use lore_revision::node::ROOT_NODE;
    use lore_revision::state;
    use lore_storage::WriteOptions;
    use lore_storage::hash_string;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use tonic::Code;

    use super::*;
    use crate::grpc::get_write_token;
    use crate::grpc::handlers::branch_push;
    use crate::http::presign_token::verify;
    use crate::store::test_store_create;

    fn test_presign_config() -> PresignConfig {
        PresignConfig {
            hmac_key: ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &[7u8; 32]),
            key_id: "download_test_key".to_string(),
            min_ttl_seconds: 10,
            default_ttl_seconds: 300,
            max_ttl_seconds: 600,
            content_type_allowlist: crate::http::security_headers::ContentTypeAllowlist::default(),
        }
    }

    fn request(
        repository: RepositoryId,
        query: Query,
        path: &str,
    ) -> Request<RevisionFileDownloadRequest> {
        let mut request = Request::new(RevisionFileDownloadRequest {
            query: Some(query),
            path: path.to_string(),
            ttl_seconds: 60,
            content_type: "text/plain; charset=utf-8".to_string(),
            inline: false,
        });
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );
        request
    }

    async fn push_file(
        repository: &Arc<RepositoryContext>,
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        name: &str,
        content: Bytes,
    ) -> (BranchId, Hash, lore_storage::Address) {
        let write_token = get_write_token();
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        branch::create(
            repository.clone(),
            &write_token,
            branch_id,
            "download-test",
            branch::default_category(),
            "creator",
            1,
            vec![],
            false,
            false,
        )
        .await
        .expect("create branch");

        let address = lore_storage::write_content(
            immutable_store,
            repository.id,
            Context::from(uuid::Uuid::now_v7()),
            content.clone(),
            WriteOptions::default(),
            None,
            None,
            None,
        )
        .await
        .expect("write content");
        let mut metadata = Metadata::new();
        metadata.set_branch(branch_id).expect("set branch");
        let metadata_hash = metadata
            .serialize(repository.clone())
            .await
            .expect("serialize metadata");
        let state = state::State::new();
        state.set_revision_number(1);
        state.set_metadata_hash(metadata_hash);
        state
            .node_add(
                repository.clone(),
                ROOT_NODE,
                Node {
                    flags: NodeFlags::File.bits(),
                    mode: 1,
                    name_hash: hash_string(name),
                    size: content.len() as u64,
                    address,
                    ..Default::default()
                },
                name,
            )
            .await
            .expect("add file");
        let serialized = state
            .serialize(repository.clone(), &write_token)
            .await
            .expect("serialize state");
        let signature = branch_push::push(
            repository.clone(),
            branch_id,
            serialized,
            true,
            true,
            false,
            DEFAULT_HISTORY_STEP_SIZE,
            crate::grpc::server::RevisionListAcceleration::default(),
        )
        .await
        .expect("push revision")
        .revision;
        (branch_id, signature, address)
    }

    #[test]
    fn content_disposition_has_ascii_fallback_and_utf8_filename() {
        assert_eq!(
            content_disposition("résumé 2026.txt", false),
            "attachment; filename=\"r_sum__2026.txt\"; filename*=UTF-8''r%C3%A9sum%C3%A9%202026.txt"
        );
        assert_eq!(
            content_disposition("photo.png", true),
            "inline; filename=\"photo.png\"; filename*=UTF-8''photo.png"
        );
    }

    #[tokio::test]
    async fn resolves_branch_tip_and_signs_pinned_logical_download() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");
        LORE_CONTEXT
            .scope(execution, async move {
                let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
                let repository = Arc::new(RepositoryContext::new_server_context(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    repository_id,
                ));
                let content = Bytes::from_static(b"logical file bytes");
                let (branch_id, signature, address) = push_file(
                    &repository,
                    immutable_store.clone(),
                    "résumé.txt",
                    content.clone(),
                )
                .await;
                let config = test_presign_config();
                let response = handler(
                    request(
                        repository_id,
                        Query::Identifier(model_v1::RevisionIdentifier {
                            branch_id: branch_id.into(),
                            number: 0,
                        }),
                        "résumé.txt",
                    ),
                    immutable_store.clone(),
                    mutable_store.clone(),
                    Some(config.clone()),
                )
                .await
                .expect("download response")
                .into_inner();

                let revision = response.revision.expect("resolved revision");
                assert_eq!(Hash::from(revision.signature.as_ref()), signature);
                assert_eq!(revision.identifier.unwrap().number, 1);
                assert_eq!(response.size, content.len() as u64);
                assert_eq!(response.file_name, "résumé.txt");
                assert_eq!(response.mode, 1);
                assert_eq!(
                    lore_storage::Address::from(response.address.as_ref().unwrap()),
                    address
                );
                assert_eq!(response.repository_resolved.as_ref(), repository_id.data());

                let token = response
                    .url_suffix
                    .split_once("?token=")
                    .expect("signed token in URL")
                    .1;
                let payload = verify(
                    token,
                    &config.hmac_key,
                    &config.key_id,
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                )
                .expect("valid signed token");
                assert_eq!(payload.repository, repository_id.to_string());
                assert_eq!(payload.address, address.to_string());
                assert_eq!(payload.content_length, Some(content.len() as u64));
                assert_eq!(
                    payload.content_type.as_deref(),
                    Some("text/plain; charset=utf-8")
                );
                assert!(
                    payload
                        .content_disposition
                        .as_deref()
                        .unwrap()
                        .starts_with("attachment; filename=\"r_sum_.txt\"")
                );

                let error = handler(
                    request(
                        repository_id,
                        Query::Signature(signature.into()),
                        "missing.txt",
                    ),
                    immutable_store,
                    mutable_store,
                    Some(config),
                )
                .await
                .expect_err("missing file must not be signed");
                assert_eq!(error.code(), Code::NotFound);
            })
            .await;
    }

    #[tokio::test]
    async fn fails_when_logical_download_urls_are_not_configured() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("test stores");
        LORE_CONTEXT
            .scope(execution, async move {
                let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
                let error = handler(
                    request(
                        repository_id,
                        Query::Signature(Hash::hash_buffer(b"revision").into()),
                        "file.txt",
                    ),
                    immutable_store,
                    mutable_store,
                    None,
                )
                .await
                .expect_err("presign must be configured");
                assert_eq!(error.code(), Code::FailedPrecondition);
            })
            .await;
    }
}
