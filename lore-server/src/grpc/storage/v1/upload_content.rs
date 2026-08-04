// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::io;
use std::sync::Arc;

use futures::StreamExt;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_proto::lore::storage::v1 as storage_v1;
use lore_proto::lore::storage::v1::upload_content_request::Part;
use lore_storage::ContentStreamError;
use lore_storage::options::WriteOptions;
use tokio_util::io::StreamReader;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tonic::Streaming;

use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::util::setup_execution;

fn uuid_v7(bytes: &[u8], field: &str) -> Result<uuid::Uuid, Status> {
    let id = uuid::Uuid::from_slice(bytes)
        .map_err(|error| Status::invalid_argument(format!("invalid {field}: {error}")))?;
    if id.get_version_num() != 7 {
        return Err(Status::invalid_argument(format!(
            "{field} must be a UUIDv7"
        )));
    }
    Ok(id)
}

fn storage_status(error: ContentStreamError, max_bytes: Option<u64>) -> Status {
    let ContentStreamError::Storage(error) = error else {
        return Status::invalid_argument(error.to_string());
    };
    if error.is_oversized() {
        match max_bytes {
            Some(max_bytes) => Status::resource_exhausted(format!(
                "upload exceeds configured limit of {max_bytes} bytes"
            )),
            None => Status::resource_exhausted(error.to_string()),
        }
    } else if error.is_slow_down() {
        Status::resource_exhausted(error.to_string())
    } else {
        Status::internal(error.to_string())
    }
}

/// Ingest one raw file stream. The first message is a required header; every
/// remaining message must carry bytes. The stream is adapted directly to
/// `AsyncRead`, preserving tonic/HTTP2 backpressure all the way into FastCDC.
#[tracing::instrument(name = "StorageServiceV1::UploadContent", skip_all)]
pub async fn handler(
    request: Request<Streaming<storage_v1::UploadContentRequest>>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    max_bytes: Option<u64>,
) -> Result<Response<storage_v1::UploadContentResponse>, Status> {
    let repository = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let stream = request.into_inner();
    let execution = setup_execution(module_path!(), correlation_id, user_id);
    let response = LORE_CONTEXT
        .scope(
            execution,
            ingest(stream, immutable_store, repository, max_bytes),
        )
        .await?;
    Ok(Response::new(response))
}

pub(crate) async fn ingest<S>(
    mut stream: S,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    repository: lore_base::types::Partition,
    max_bytes: Option<u64>,
) -> Result<storage_v1::UploadContentResponse, Status>
where
    S: futures::Stream<Item = Result<storage_v1::UploadContentRequest, Status>> + Unpin,
{
    let first = stream
        .next()
        .await
        .transpose()?
        .ok_or_else(|| Status::invalid_argument("upload stream is missing its header"))?;
    let header = match first.part {
        Some(Part::Header(header)) => header,
        _ => {
            return Err(Status::invalid_argument(
                "the first upload message must be the header",
            ));
        }
    };
    let _request_id = uuid_v7(header.request_id.as_ref(), "request_id")?;
    let context = if header.file_id.is_empty() {
        Context::from(uuid::Uuid::now_v7())
    } else {
        Context::from(uuid_v7(header.file_id.as_ref(), "file_id")?)
    };
    if let (Some(expected_size), Some(max_bytes)) = (header.expected_size, max_bytes)
        && expected_size > max_bytes
    {
        return Err(Status::resource_exhausted(format!(
            "declared upload size {expected_size} exceeds configured limit of {max_bytes} bytes"
        )));
    }

    let bytes = stream.map(|message| match message {
        Ok(message) => match message.part {
            Some(Part::Chunk(chunk)) if !chunk.is_empty() => Ok(chunk),
            Some(Part::Chunk(_)) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "upload chunks must not be empty",
            )),
            Some(Part::Header(_)) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "upload header may appear only once and must be first",
            )),
            None => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "upload message is missing its part",
            )),
        },
        Err(status) => Err(io::Error::other(status.to_string())),
    });
    let reader = StreamReader::new(bytes);
    let result = lore_storage::write_content_stream(
        immutable_store,
        repository,
        context,
        reader,
        header.expected_size,
        max_bytes,
        WriteOptions::default().with_local_cache_priority(),
        None,
        None,
    )
    .await
    .map_err(|error| storage_status(error, max_bytes))?;

    Ok(storage_v1::UploadContentResponse {
        address: Some(result.0.into()),
        size: result.2,
    })
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures::stream;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Partition;
    use lore_proto::lore::storage::v1::UploadContentHeader;
    use lore_proto::lore::storage::v1::UploadContentRequest;
    use lore_proto::lore::storage::v1::upload_content_request::Part;
    use lore_storage::options::ReadOptions;

    use super::*;
    use crate::store::test_store_create;

    fn header(request_id: uuid::Uuid, file_id: uuid::Uuid, size: u64) -> UploadContentRequest {
        UploadContentRequest {
            part: Some(Part::Header(UploadContentHeader {
                file_id: Bytes::copy_from_slice(file_id.as_bytes()),
                expected_size: Some(size),
                request_id: Bytes::copy_from_slice(request_id.as_bytes()),
            })),
        }
    }

    fn chunk(bytes: Bytes) -> UploadContentRequest {
        UploadContentRequest {
            part: Some(Part::Chunk(bytes)),
        }
    }

    #[tokio::test]
    async fn ingest_streams_large_content_and_is_address_idempotent() {
        let repository = Partition::from(uuid::Uuid::now_v7());
        let file_id = uuid::Uuid::now_v7();
        let request_id = uuid::Uuid::now_v7();
        let payload = Bytes::from(vec![0x5a; lore_storage::FRAGMENT_SIZE_THRESHOLD * 3 + 19]);
        let (immutable, _mutable, execution) = test_store_create().await.expect("stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let messages = vec![
                Ok(header(request_id, file_id, payload.len() as u64)),
                Ok(chunk(payload.slice(..100_000))),
                Ok(chunk(payload.slice(100_000..))),
            ];
            let first = ingest(stream::iter(messages), immutable.clone(), repository, None)
                .await
                .expect("first upload");
            assert_eq!(first.size, payload.len() as u64);

            let retry = ingest(
                stream::iter(vec![
                    Ok(header(request_id, file_id, payload.len() as u64)),
                    Ok(chunk(payload.clone())),
                ]),
                immutable.clone(),
                repository,
                None,
            )
            .await
            .expect("idempotent upload retry");
            assert_eq!(retry.address, first.address);

            let address = Address::from(first.address.expect("address"));
            assert_eq!(address.context, Context::from(file_id));
            let restored = lore_storage::read(
                immutable,
                repository,
                address,
                None,
                ReadOptions::default(),
                None,
            )
            .await
            .expect("read uploaded content");
            assert_eq!(restored, payload);
        }))
        .await;
    }

    #[tokio::test]
    async fn ingest_enforces_actual_size_and_header_framing() {
        let repository = Partition::from(uuid::Uuid::now_v7());
        let file_id = uuid::Uuid::now_v7();
        let request_id = uuid::Uuid::now_v7();
        let (immutable, _mutable, execution) = test_store_create().await.expect("stores");

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let oversized = ingest(
                stream::iter(vec![
                    Ok(header(request_id, file_id, 4)),
                    Ok(chunk(Bytes::from_static(b"12345"))),
                ]),
                immutable.clone(),
                repository,
                Some(4),
            )
            .await
            .expect_err("actual size over limit");
            assert_eq!(oversized.code(), tonic::Code::ResourceExhausted);

            let repeated_header = ingest(
                stream::iter(vec![
                    Ok(header(request_id, file_id, 1)),
                    Ok(header(request_id, file_id, 1)),
                ]),
                immutable,
                repository,
                None,
            )
            .await
            .expect_err("header may appear only once");
            assert_eq!(repeated_header.code(), tonic::Code::InvalidArgument);
        }))
        .await;
    }
}
