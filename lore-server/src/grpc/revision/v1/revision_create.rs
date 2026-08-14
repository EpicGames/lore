// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::BTreeMap;
use std::sync::Arc;

use futures::StreamExt;
use futures::TryStreamExt;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::BranchId;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_proto::lore::revision::v1 as revision_v1;
use lore_proto::lore::revision::v1::revision_create_operation::Op;
use lore_revision::branch;
use lore_revision::commit::CommitError;
use lore_revision::commit::construct_tree_revision;
use lore_revision::interface::LoreNodeType;
use lore_revision::metadata::Metadata;
use lore_revision::node::NodeDelta;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::notification::NotificationSender;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision_tree::RevisionTreeEditError;
use lore_revision::state::State;
use lore_storage::StoreMatch;
use lore_telemetry::InstrumentProvider;
use prost::Message;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use super::branch_push::publish_revision;
use super::idempotency;
use crate::grpc::extract_correlation_id;
use crate::grpc::get_authorization;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::get_write_token;
use crate::grpc::handlers::branch_push::extract_client_ip;
use crate::grpc::server::RevisionListAcceleration;
use crate::hooks::HookDispatcher;
use crate::util::setup_execution;

#[derive(Clone, Copy, Debug)]
pub struct RevisionCreateLimits {
    pub max_request_bytes: usize,
    pub max_operations: usize,
    pub max_metadata_entries: usize,
    pub max_metadata_bytes: usize,
    pub max_path_bytes: usize,
}

impl Default for RevisionCreateLimits {
    fn default() -> Self {
        Self {
            max_request_bytes: 4 * 1024 * 1024,
            max_operations: 10_000,
            max_metadata_entries: 256,
            max_metadata_bytes: 256 * 1024,
            max_path_bytes: 4096,
        }
    }
}

fn resource_limit(name: &str, actual: usize, limit: usize) -> Status {
    Status::resource_exhausted(format!(
        "RevisionCreate {name} limit exceeded: {actual} > {limit}"
    ))
}

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

fn fixed_context(bytes: &[u8], field: &str) -> Result<Context, Status> {
    if bytes.len() != 16 {
        return Err(Status::invalid_argument(format!(
            "{field} must contain exactly 16 bytes"
        )));
    }
    Ok(Context::from(bytes))
}

fn fixed_hash(bytes: &[u8], field: &str) -> Result<Hash, Status> {
    if bytes.len() != 32 {
        return Err(Status::invalid_argument(format!(
            "{field} must contain exactly 32 bytes"
        )));
    }
    Ok(Hash::from(bytes))
}

fn address(value: &lore_proto::lore::model::v1::Address) -> Result<Address, Status> {
    let hash = fixed_hash(value.hash.as_ref(), "address.hash")?;
    let context = fixed_context(value.context.as_ref(), "address.context")?;
    if context.is_zero() {
        return Err(Status::invalid_argument(
            "address.context (file id) must be non-zero",
        ));
    }
    Ok(Address { hash, context })
}

fn path_parts<'a>(path: &'a str, limits: RevisionCreateLimits) -> Result<Vec<&'a str>, Status> {
    if path.is_empty() {
        return Err(Status::invalid_argument("path must not be empty"));
    }
    if path.len() > limits.max_path_bytes {
        return Err(resource_limit(
            "path bytes",
            path.len(),
            limits.max_path_bytes,
        ));
    }
    if path.starts_with('/') || path.ends_with('/') || path.contains('\\') {
        return Err(Status::invalid_argument(
            "path must be normalized, relative, and use '/' separators",
        ));
    }
    let parts = path.split('/').collect::<Vec<_>>();
    if parts
        .iter()
        .any(|part| part.is_empty() || *part == "." || *part == "..")
    {
        return Err(Status::invalid_argument(
            "path contains an empty, '.' or '..' component",
        ));
    }
    Ok(parts)
}

async fn resolve_path(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    parts: &[&str],
) -> Result<NodeID, Status> {
    let mut node_id = ROOT_NODE;
    for part in parts {
        let parent = state
            .node(repository.clone(), node_id)
            .await
            .map_err(|error| Status::internal(format!("failed to read path parent: {error}")))?;
        if parent.is_discarded() || !parent.is_directory() {
            return Err(Status::invalid_argument(format!(
                "path component parent for '{part}' is not a directory"
            )));
        }
        node_id = match state
            .find_subnode(repository.clone(), node_id, lore_storage::hash_string(part))
            .await
        {
            Ok(node_id) => node_id,
            Err(error) if error.is_node_not_found() => {
                return Err(Status::invalid_argument(format!(
                    "path component '{part}' does not exist"
                )));
            }
            Err(error) => {
                return Err(Status::internal(format!(
                    "failed resolving path component '{part}': {error}"
                )));
            }
        };
    }
    Ok(node_id)
}

async fn resolve_parent(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    parts: &[&str],
) -> Result<(NodeID, String), Status> {
    let (name, parents) = parts
        .split_last()
        .ok_or_else(|| Status::invalid_argument("path must not be empty"))?;
    let parent = if parents.is_empty() {
        ROOT_NODE
    } else {
        resolve_path(state, repository, parents).await?
    };
    Ok((parent, (*name).to_string()))
}

fn edit_status(error: RevisionTreeEditError) -> Status {
    if error.is_invalid_arguments() {
        Status::invalid_argument(error.to_string())
    } else {
        Status::internal(error.to_string())
    }
}

fn commit_status(error: CommitError) -> Status {
    if error.is_nothing_staged() || error.is_invalid_arguments() {
        Status::invalid_argument(error.to_string())
    } else if error.is_slow_down() {
        Status::resource_exhausted(error.to_string())
    } else {
        Status::internal(error.to_string())
    }
}

fn metadata(
    entries: &[revision_v1::RevisionCreateMetadataEntry],
    commit_message: &str,
) -> Result<Metadata, Status> {
    let mut metadata = Metadata::default();
    for entry in entries {
        if entry.key.is_empty() {
            return Err(Status::invalid_argument("metadata key must not be empty"));
        }
        let result = match entry.format {
            1 => {
                if entry.value.len() != 48 {
                    return Err(Status::invalid_argument(
                        "address metadata values must contain exactly 48 bytes",
                    ));
                }
                metadata.set_address(
                    &entry.key,
                    Address {
                        hash: Hash::from(&entry.value[..32]),
                        context: Context::from(&entry.value[32..]),
                    },
                )
            }
            2 => {
                if entry.value.len() != 1 || entry.value[0] > 1 {
                    return Err(Status::invalid_argument(
                        "boolean metadata values must be one byte (0 or 1)",
                    ));
                }
                metadata.set_bool(&entry.key, entry.value[0] != 0)
            }
            3 => {
                let value = fixed_context(entry.value.as_ref(), "metadata context value")?;
                metadata.set_context(&entry.key, value)
            }
            4 => {
                let value = fixed_hash(entry.value.as_ref(), "metadata hash value")?;
                metadata.set_hash(&entry.key, value)
            }
            5 => {
                let bytes: [u8; 8] = entry.value.as_ref().try_into().map_err(|_| {
                    Status::invalid_argument("numeric metadata values must contain 8 bytes")
                })?;
                metadata.set_u64(&entry.key, u64::from_le_bytes(bytes))
            }
            6 => {
                let value = std::str::from_utf8(entry.value.as_ref()).map_err(|_| {
                    Status::invalid_argument("string metadata value is not valid UTF-8")
                })?;
                metadata.set_string(&entry.key, value)
            }
            255 => metadata.set_binary(&entry.key, entry.value.as_ref()),
            _ => return Err(Status::invalid_argument("unsupported metadata format")),
        };
        result.map_err(|error| Status::invalid_argument(error.to_string()))?;
    }
    metadata
        .set_string(lore_revision::metadata::MESSAGE, commit_message)
        .map_err(|error| Status::invalid_argument(error.to_string()))?;
    Ok(metadata)
}

async fn address_sizes(
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    repository: Partition,
    operations: &[revision_v1::RevisionCreateOperation],
) -> Result<BTreeMap<Address, u64>, Status> {
    let mut addresses = BTreeMap::new();
    for operation in operations {
        if let Some(Op::PutFile(put)) = &operation.op {
            let value = put
                .address
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("PutFile.address is required"))?;
            addresses.insert(address(value)?, 0);
        }
    }

    let loaded = futures::stream::iter(addresses.into_keys())
        .map(|address| {
            let store = immutable_store.clone();
            async move {
                if address.hash.is_zero() {
                    return Ok((address, 0));
                }
                let query = store
                    .get_metadata(repository, address)
                    .await
                    .map_err(|error| {
                        Status::internal(format!("failed to query PutFile address: {error}"))
                    })?;
                if query.match_made != StoreMatch::MatchFull {
                    return Err(Status::invalid_argument(format!(
                        "PutFile address {address} is not stored in this repository/context"
                    )));
                }
                Ok((address, query.fragment.size_content))
            }
        })
        .buffer_unordered(32)
        .try_collect::<Vec<_>>()
        .await?;
    Ok(loaded.into_iter().collect())
}

async fn apply_operations(
    state: Arc<State>,
    repository: Arc<RepositoryContext>,
    operations: &[revision_v1::RevisionCreateOperation],
    sizes: &BTreeMap<Address, u64>,
    limits: RevisionCreateLimits,
) -> Result<Vec<NodeDelta>, Status> {
    let mut deleted = Vec::new();
    for operation in operations {
        match operation
            .op
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("operation is missing its op"))?
        {
            Op::PutFile(put) => {
                let parts = path_parts(&put.path, limits)?;
                let proto_address = put
                    .address
                    .as_ref()
                    .ok_or_else(|| Status::invalid_argument("PutFile.address is required"))?;
                let address = address(proto_address)?;
                let size = *sizes
                    .get(&address)
                    .ok_or_else(|| Status::internal("PutFile address size was not loaded"))?;
                let mode = u16::try_from(put.mode)
                    .map_err(|_| Status::invalid_argument("PutFile.mode exceeds u16"))?;
                match resolve_path(state.clone(), repository.clone(), &parts).await {
                    Ok(node_id) => {
                        lore_revision::revision_tree::modify_file(
                            state.clone(),
                            repository.clone(),
                            node_id,
                            mode,
                            size,
                            address,
                        )
                        .await
                        .map_err(edit_status)?;
                    }
                    Err(status) if status.code() == tonic::Code::InvalidArgument => {
                        let (parent, name) =
                            resolve_parent(state.clone(), repository.clone(), &parts).await?;
                        lore_revision::revision_tree::add_node(
                            state.clone(),
                            repository.clone(),
                            parent,
                            name.as_bytes(),
                            LoreNodeType::File as u32,
                            mode,
                            size,
                            address,
                        )
                        .await
                        .map_err(edit_status)?;
                    }
                    Err(status) => return Err(status),
                }
            }
            Op::CreateDirectory(create) => {
                let parts = path_parts(&create.path, limits)?;
                match resolve_path(state.clone(), repository.clone(), &parts).await {
                    Ok(_) => {
                        return Err(Status::invalid_argument(format!(
                            "path '{}' already exists",
                            create.path
                        )));
                    }
                    Err(status) if status.code() == tonic::Code::InvalidArgument => {}
                    Err(status) => return Err(status),
                }
                let mode = u16::try_from(create.mode)
                    .map_err(|_| Status::invalid_argument("CreateDirectory.mode exceeds u16"))?;
                let (parent, name) =
                    resolve_parent(state.clone(), repository.clone(), &parts).await?;
                lore_revision::revision_tree::create_directory(
                    state.clone(),
                    repository.clone(),
                    parent,
                    name.as_bytes(),
                    mode,
                )
                .await
                .map_err(edit_status)?;
            }
            Op::DeletePath(delete) => {
                let parts = path_parts(&delete.path, limits)?;
                let node_id = resolve_path(state.clone(), repository.clone(), &parts).await?;
                deleted.extend(
                    lore_revision::revision_tree::delete_node(
                        state.clone(),
                        repository.clone(),
                        node_id,
                    )
                    .await
                    .map_err(edit_status)?,
                );
            }
            Op::MovePath(moved) => {
                let source = path_parts(&moved.source, limits)?;
                let destination = path_parts(&moved.destination, limits)?;
                let node_id = resolve_path(state.clone(), repository.clone(), &source).await?;
                let (parent, name) =
                    resolve_parent(state.clone(), repository.clone(), &destination).await?;
                lore_revision::revision_tree::move_node(
                    state.clone(),
                    repository.clone(),
                    node_id,
                    parent,
                    name.as_bytes(),
                )
                .await
                .map_err(edit_status)?;
            }
        }
    }
    Ok(deleted)
}

#[allow(clippy::too_many_arguments)]
#[tracing::instrument(name = "RevisionCreate::v1::handle", skip_all)]
pub async fn handler(
    request: Request<revision_v1::RevisionCreateRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    notification: Arc<dyn NotificationSender>,
    hook_dispatcher: &HookDispatcher,
    history_step_size: u64,
    acceleration: RevisionListAcceleration,
    limits: RevisionCreateLimits,
    instrument_provider: &impl InstrumentProvider,
) -> Result<Response<revision_v1::RevisionCreateResponse>, Status> {
    let repository_id = get_repository(request.metadata())?;
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let user_id = get_user_id(request.extensions());
    let bypass_protection = get_authorization(request.extensions())
        .ok()
        .and_then(|token| token.is_service_account)
        .unwrap_or_default();
    let client_ip = extract_client_ip(&request).map(|value| value.to_string());
    let req = request.into_inner();

    let encoded_len = req.encoded_len();
    if encoded_len > limits.max_request_bytes {
        return Err(resource_limit(
            "encoded request bytes",
            encoded_len,
            limits.max_request_bytes,
        ));
    }
    if req.operations.is_empty() {
        return Err(Status::invalid_argument(
            "RevisionCreate requires at least one operation",
        ));
    }
    if req.operations.len() > limits.max_operations {
        return Err(resource_limit(
            "operation count",
            req.operations.len(),
            limits.max_operations,
        ));
    }
    if req.metadata.len() > limits.max_metadata_entries {
        return Err(resource_limit(
            "metadata count",
            req.metadata.len(),
            limits.max_metadata_entries,
        ));
    }
    let metadata_bytes = req.metadata.iter().try_fold(0usize, |total, entry| {
        total
            .checked_add(entry.key.len())
            .and_then(|value| value.checked_add(entry.value.len()))
            .ok_or_else(|| Status::resource_exhausted("metadata byte count overflow"))
    })?;
    if metadata_bytes > limits.max_metadata_bytes {
        return Err(resource_limit(
            "metadata bytes",
            metadata_bytes,
            limits.max_metadata_bytes,
        ));
    }

    let request_id = Context::from(uuid_v7(req.request_id.as_ref(), "request_id")?);
    let branch_id = BranchId::from(fixed_context(req.branch_id.as_ref(), "branch_id")?);
    if branch_id.is_zero() {
        return Err(Status::invalid_argument("branch_id must be non-zero"));
    }
    let base_revision = fixed_hash(
        req.revision_signature_base.as_ref(),
        "revision_signature_base",
    )?;
    let request_digest = Hash::hash_buffer(req.encode_to_vec().as_slice());

    let reservation = match idempotency::begin(
        "RevisionCreate",
        immutable_store.clone(),
        mutable_store.clone(),
        repository_id,
        branch_id,
        request_id,
        request_digest,
    )
    .await?
    {
        idempotency::Start::Completed {
            revision,
            revision_number,
        } => {
            return Ok(Response::new(revision_v1::RevisionCreateResponse {
                revision_signature: revision.into(),
                revision_number,
            }));
        }
        idempotency::Start::Reserved(reservation) => reservation,
    };

    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store.clone(),
        mutable_store.clone(),
        repository_id,
    ));
    let current_tip = branch::load_latest(repository.clone(), branch_id)
        .await
        .unwrap_or_default();
    if current_tip != base_revision {
        idempotency::release(mutable_store, repository_id, &reservation).await;
        return Err(Status::failed_precondition(format!(
            "branch advanced; current latest: {current_tip}"
        )));
    }

    let pending_metadata = match metadata(&req.metadata, &req.commit_message) {
        Ok(metadata) => metadata,
        Err(status) => {
            idempotency::release(mutable_store, repository_id, &reservation).await;
            return Err(status);
        }
    };
    let sizes = match address_sizes(immutable_store.clone(), repository_id, &req.operations).await {
        Ok(sizes) => sizes,
        Err(status) => {
            idempotency::release(mutable_store, repository_id, &reservation).await;
            return Err(status);
        }
    };
    let execution = setup_execution(module_path!(), correlation_id.clone(), user_id.clone());
    let outcome = LORE_CONTEXT
        .scope(execution, async move {
            let state = State::deserialize(repository.clone(), base_revision)
                .await
                .map_err(|error| {
                    if error.is_address_not_found()
                        || error.is_payload_not_found()
                        || error.is_not_found()
                    {
                        Status::not_found(format!("base revision was not found: {error}"))
                    } else {
                        Status::internal(format!("failed to deserialize base revision: {error}"))
                    }
                })?;
            let deleted = apply_operations(
                state.clone(),
                repository.clone(),
                &req.operations,
                &sizes,
                limits,
            )
            .await?;
            let token = get_write_token();
            let revision = construct_tree_revision(
                repository.clone(),
                &token,
                state,
                pending_metadata,
                branch_id,
                deleted,
            )
            .await
            .map_err(commit_status)?;

            let published = publish_revision(
                repository,
                branch_id,
                revision,
                bypass_protection,
                false,
                false,
                client_ip,
                correlation_id,
                user_id,
                notification.clone(),
                hook_dispatcher,
                history_step_size,
                acceleration,
                instrument_provider,
            )
            .await?
            .into_inner();

            Ok(Response::new(revision_v1::RevisionCreateResponse {
                revision_signature: published.revision_signature,
                revision_number: published.revision_number,
            }))
        })
        .await;

    match outcome {
        Ok(response) => {
            idempotency::complete(
                immutable_store,
                mutable_store,
                repository_id,
                &reservation,
                fixed_hash(
                    response.get_ref().revision_signature.as_ref(),
                    "published revision",
                )?,
                response.get_ref().revision_number,
            )
            .await?;
            Ok(response)
        }
        Err(status) => {
            idempotency::release(mutable_store, repository_id, &reservation).await;
            Err(status)
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use lore_proto::lore::storage::v1::UploadContentHeader;
    use lore_proto::lore::storage::v1::UploadContentRequest;
    use lore_proto::lore::storage::v1::upload_content_request::Part as UploadPart;
    use lore_proto::lore::thin_client::v1::RevisionTreeRequest;
    use lore_proto::lore::thin_client::v1::revision_tree_request::Query;
    use lore_proto::lore::thin_client::v1::revision_tree_response::Payload;
    use lore_revision::branch::DEFAULT_HISTORY_STEP_SIZE;
    use lore_revision::node::NodeIDExt;
    use lore_storage::options::WriteOptions;
    use lore_telemetry::InstrumentProvider;
    use opentelemetry::KeyValue;
    use tokio_stream::StreamExt;
    use tonic::metadata::BinaryMetadataValue;

    use super::*;
    use crate::hooks::HookDispatcher;
    use crate::notification::testing::MockNotificationSender;
    use crate::store::test_store_create;

    struct TestInstrumentProvider;

    impl InstrumentProvider for TestInstrumentProvider {
        fn namespace(&self) -> &'static str {
            "test"
        }

        fn labels(&self) -> &[KeyValue] {
            &[]
        }
    }

    async fn create_root_branch(repository: &Arc<RepositoryContext>, branch_id: BranchId) {
        let token = get_write_token();
        branch::create(
            repository.clone(),
            &token,
            branch_id,
            "main",
            branch::default_category(),
            "test-creator",
            1,
            vec![],
            false,
            false,
        )
        .await
        .expect("create root branch");
    }

    fn request(
        repository: Partition,
        request_id: uuid::Uuid,
        branch_id: BranchId,
        base: Hash,
        operations: Vec<revision_v1::RevisionCreateOperation>,
    ) -> Request<revision_v1::RevisionCreateRequest> {
        let mut request = Request::new(revision_v1::RevisionCreateRequest {
            request_id: Bytes::copy_from_slice(request_id.as_bytes()),
            branch_id: branch_id.into(),
            revision_signature_base: base.into(),
            commit_message: "browser edit".into(),
            metadata: Vec::new(),
            operations,
        });
        request.metadata_mut().insert_bin(
            lore_transport::grpc::REPOSITORY_ID_KEY,
            BinaryMetadataValue::from_bytes(repository.data()),
        );
        request
    }

    fn service_account_request(
        repository: Partition,
        request_id: uuid::Uuid,
        branch_id: BranchId,
        base: Hash,
        operations: Vec<revision_v1::RevisionCreateOperation>,
    ) -> Request<revision_v1::RevisionCreateRequest> {
        let mut request = request(repository, request_id, branch_id, base, operations);
        request
            .extensions_mut()
            .insert(crate::auth::jwt::AuthorizationToken {
                user_id: "service-bot".into(),
                is_service_account: Some(true),
                ..crate::auth::jwt::AuthorizationToken::default()
            });
        request
    }

    fn create_directory(path: &str) -> revision_v1::RevisionCreateOperation {
        revision_v1::RevisionCreateOperation {
            op: Some(Op::CreateDirectory(revision_v1::RevisionCreateDirectory {
                path: path.into(),
                mode: 0o755,
            })),
        }
    }

    fn put_file(path: &str, address: Address) -> revision_v1::RevisionCreateOperation {
        revision_v1::RevisionCreateOperation {
            op: Some(Op::PutFile(revision_v1::RevisionCreatePutFile {
                path: path.into(),
                mode: 0o644,
                address: Some(address.into()),
            })),
        }
    }

    fn delete_path(path: &str) -> revision_v1::RevisionCreateOperation {
        revision_v1::RevisionCreateOperation {
            op: Some(Op::DeletePath(revision_v1::RevisionCreateDeletePath {
                path: path.into(),
            })),
        }
    }

    fn move_path(source: &str, destination: &str) -> revision_v1::RevisionCreateOperation {
        revision_v1::RevisionCreateOperation {
            op: Some(Op::MovePath(revision_v1::RevisionCreateMovePath {
                source: source.into(),
                destination: destination.into(),
            })),
        }
    }

    #[tokio::test]
    async fn revision_create_publishes_file_and_nested_empty_directory_idempotently() {
        let repository_id = Partition::from(uuid::Uuid::now_v7());
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        let request_id = uuid::Uuid::now_v7();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("create stores");

        let mut notification = MockNotificationSender::new();
        notification
            .expect_branch_pushed()
            .once()
            .return_once(|_, _, _, _, _| ());
        let notification = Arc::new(notification);
        let hooks = HookDispatcher::empty();
        let instruments = TestInstrumentProvider;

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository_id,
            ));
            create_root_branch(&repository, branch_id).await;

            let payload = Bytes::from_static(b"hello from a browser upload");
            let file_id = uuid::Uuid::now_v7();
            let uploaded = crate::grpc::storage::v1::upload_content::ingest(
                futures::stream::iter(vec![
                    Ok(UploadContentRequest {
                        part: Some(UploadPart::Header(UploadContentHeader {
                            file_id: Bytes::copy_from_slice(file_id.as_bytes()),
                            expected_size: Some(payload.len() as u64),
                            request_id: Bytes::copy_from_slice(uuid::Uuid::now_v7().as_bytes()),
                        })),
                    }),
                    Ok(UploadContentRequest {
                        part: Some(UploadPart::Chunk(payload)),
                    }),
                ]),
                immutable_store.clone(),
                repository_id,
                None,
            )
            .await
            .expect("stream file content");
            let file_address = Address::from(uploaded.address.expect("upload address"));
            assert_eq!(uploaded.size, 27);

            let operations = vec![
                create_directory("docs"),
                create_directory("docs/empty"),
                create_directory("root-empty"),
                put_file("docs/readme.txt", file_address),
            ];
            let first = handler(
                request(
                    repository_id,
                    request_id,
                    branch_id,
                    Hash::default(),
                    operations.clone(),
                ),
                immutable_store.clone(),
                mutable_store.clone(),
                notification.clone(),
                &hooks,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                RevisionCreateLimits::default(),
                &instruments,
            )
            .await
            .expect("create revision")
            .into_inner();
            assert!(!first.revision_signature.is_empty());
            assert_eq!(first.revision_number, 1);

            let retry = handler(
                request(
                    repository_id,
                    request_id,
                    branch_id,
                    Hash::default(),
                    operations,
                ),
                immutable_store.clone(),
                mutable_store.clone(),
                notification.clone(),
                &hooks,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                RevisionCreateLimits::default(),
                &instruments,
            )
            .await
            .expect("idempotent retry")
            .into_inner();
            assert_eq!(retry, first);

            let conflict = handler(
                request(
                    repository_id,
                    request_id,
                    branch_id,
                    Hash::default(),
                    vec![create_directory("different")],
                ),
                immutable_store.clone(),
                mutable_store.clone(),
                notification.clone(),
                &hooks,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                RevisionCreateLimits::default(),
                &instruments,
            )
            .await
            .expect_err("request id cannot be reused for different content");
            assert_eq!(conflict.code(), tonic::Code::AlreadyExists);

            let occupied = handler(
                request(
                    repository_id,
                    uuid::Uuid::now_v7(),
                    branch_id,
                    fixed_hash(first.revision_signature.as_ref(), "revision").unwrap(),
                    vec![create_directory("docs")],
                ),
                immutable_store.clone(),
                mutable_store.clone(),
                notification.clone(),
                &hooks,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                RevisionCreateLimits::default(),
                &instruments,
            )
            .await
            .expect_err("occupied directory path");
            assert_eq!(occupied.code(), tonic::Code::InvalidArgument);

            let revision = fixed_hash(first.revision_signature.as_ref(), "revision").unwrap();
            let state = State::deserialize(repository.clone(), revision)
                .await
                .expect("load published revision");
            let docs = state
                .find_subnode(
                    repository.clone(),
                    ROOT_NODE,
                    lore_storage::hash_string("docs"),
                )
                .await
                .expect("docs directory");
            let empty = state
                .find_subnode(repository.clone(), docs, lore_storage::hash_string("empty"))
                .await
                .expect("empty directory");
            let empty = state.node(repository.clone(), empty).await.unwrap();
            assert!(empty.is_directory());
            assert!(!empty.child.is_valid_node_id());
            let file = state
                .find_subnode(
                    repository.clone(),
                    docs,
                    lore_storage::hash_string("readme.txt"),
                )
                .await
                .expect("uploaded file");
            let file = state.node(repository, file).await.unwrap();
            assert_eq!(file.address, file_address);
            assert_eq!(file.size, uploaded.size);

            let mut tree_request = Request::new(RevisionTreeRequest {
                query: Some(Query::Signature(first.revision_signature.clone())),
                path_prefix: None,
                max_depth: None,
            });
            tree_request.metadata_mut().insert_bin(
                lore_transport::grpc::REPOSITORY_ID_KEY,
                BinaryMetadataValue::from_bytes(repository_id.data()),
            );
            let mut tree = crate::grpc::thinclient::v1::revision_tree::handler(
                tree_request,
                immutable_store,
                mutable_store,
            )
            .await
            .expect("open thin-client tree")
            .into_inner();
            let mut paths = Vec::new();
            while let Some(item) = tree.next().await {
                if let Some(Payload::Node(node)) = item.expect("tree item").payload {
                    paths.push(node.path);
                }
            }
            assert!(paths.iter().any(|path| path == "docs/empty"));
            assert!(paths.iter().any(|path| path == "root-empty"));
            assert!(paths.iter().any(|path| path == "docs/readme.txt"));
        }))
        .await;
    }

    #[tokio::test]
    async fn revision_create_applies_modify_move_and_delete_in_order() {
        let repository_id = Partition::from(uuid::Uuid::now_v7());
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("create stores");
        let mut notification = MockNotificationSender::new();
        notification
            .expect_branch_pushed()
            .times(2)
            .returning(|_, _, _, _, _| ());
        let notification = Arc::new(notification);
        let hooks = HookDispatcher::empty();
        let instruments = TestInstrumentProvider;

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository_id,
            ));
            create_root_branch(&repository, branch_id).await;
            let old_address = lore_storage::write_content(
                immutable_store.clone(),
                repository_id,
                Context::from(uuid::Uuid::now_v7()),
                Bytes::from_static(b"old"),
                WriteOptions::default(),
                None,
                None,
                None,
            )
            .await
            .unwrap();
            let new_address = lore_storage::write_content(
                immutable_store.clone(),
                repository_id,
                old_address.context,
                Bytes::from_static(b"new content"),
                WriteOptions::default(),
                None,
                None,
                None,
            )
            .await
            .unwrap();
            let new_fragment = immutable_store
                .clone()
                .get_metadata(repository_id, new_address)
                .await
                .unwrap()
                .fragment;

            let first = handler(
                request(
                    repository_id,
                    uuid::Uuid::now_v7(),
                    branch_id,
                    Hash::default(),
                    vec![
                        create_directory("src"),
                        create_directory("src/empty"),
                        put_file("src/file.txt", old_address),
                        put_file("src/gone.txt", old_address),
                    ],
                ),
                immutable_store.clone(),
                mutable_store.clone(),
                notification.clone(),
                &hooks,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                RevisionCreateLimits::default(),
                &instruments,
            )
            .await
            .expect("initial revision")
            .into_inner();
            let first_hash = fixed_hash(first.revision_signature.as_ref(), "first").unwrap();

            let second = handler(
                request(
                    repository_id,
                    uuid::Uuid::now_v7(),
                    branch_id,
                    first_hash,
                    vec![
                        put_file("src/file.txt", new_address),
                        move_path("src/file.txt", "moved.txt"),
                        delete_path("src/gone.txt"),
                        delete_path("src/empty"),
                    ],
                ),
                immutable_store,
                mutable_store,
                notification,
                &hooks,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                RevisionCreateLimits::default(),
                &instruments,
            )
            .await
            .expect("ordered edit revision")
            .into_inner();
            assert_eq!(second.revision_number, 2);

            let second_hash = fixed_hash(second.revision_signature.as_ref(), "second").unwrap();
            let state = State::deserialize(repository.clone(), second_hash)
                .await
                .unwrap();
            let moved = state
                .find_subnode(
                    repository.clone(),
                    ROOT_NODE,
                    lore_storage::hash_string("moved.txt"),
                )
                .await
                .expect("moved file");
            let moved = state.node(repository.clone(), moved).await.unwrap();
            assert_eq!(moved.address, new_address);
            assert_eq!(moved.size, new_fragment.size_content);
            let src = state
                .find_subnode(
                    repository.clone(),
                    ROOT_NODE,
                    lore_storage::hash_string("src"),
                )
                .await
                .expect("src directory");
            assert!(
                state
                    .find_subnode(
                        repository.clone(),
                        src,
                        lore_storage::hash_string("gone.txt")
                    )
                    .await
                    .is_err()
            );
            assert!(
                state
                    .find_subnode(repository, src, lore_storage::hash_string("empty"))
                    .await
                    .is_err()
            );
        }))
        .await;
    }

    #[tokio::test]
    async fn revision_create_rejects_stale_base_missing_parent_and_operation_limit() {
        let repository_id = Partition::from(uuid::Uuid::now_v7());
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("create stores");
        let notification = Arc::new(MockNotificationSender::new());
        let hooks = HookDispatcher::empty();
        let instruments = TestInstrumentProvider;

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository_id,
            ));
            create_root_branch(&repository, branch_id).await;

            let stale = handler(
                request(
                    repository_id,
                    uuid::Uuid::now_v7(),
                    branch_id,
                    Hash::hash_buffer(b"not the current tip"),
                    vec![create_directory("dir")],
                ),
                immutable_store.clone(),
                mutable_store.clone(),
                notification.clone(),
                &hooks,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                RevisionCreateLimits::default(),
                &instruments,
            )
            .await
            .expect_err("stale base");
            assert_eq!(stale.code(), tonic::Code::FailedPrecondition);
            assert!(stale.message().contains("current latest"));

            let missing_parent = handler(
                request(
                    repository_id,
                    uuid::Uuid::now_v7(),
                    branch_id,
                    Hash::default(),
                    vec![create_directory("missing/child")],
                ),
                immutable_store.clone(),
                mutable_store.clone(),
                notification.clone(),
                &hooks,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                RevisionCreateLimits::default(),
                &instruments,
            )
            .await
            .expect_err("missing parent");
            assert_eq!(missing_parent.code(), tonic::Code::InvalidArgument);

            let limited = handler(
                request(
                    repository_id,
                    uuid::Uuid::now_v7(),
                    branch_id,
                    Hash::default(),
                    vec![create_directory("dir")],
                ),
                immutable_store,
                mutable_store,
                notification,
                &hooks,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                RevisionCreateLimits {
                    max_operations: 0,
                    ..RevisionCreateLimits::default()
                },
                &instruments,
            )
            .await
            .expect_err("operation limit");
            assert_eq!(limited.code(), tonic::Code::ResourceExhausted);
            assert!(limited.message().contains("operation count"));
        }))
        .await;
    }

    #[tokio::test]
    async fn revision_create_uses_branch_protection_and_service_account_bypass() {
        let repository_id = Partition::from(uuid::Uuid::now_v7());
        let branch_id = BranchId::from(uuid::Uuid::now_v7());
        let request_id = uuid::Uuid::now_v7();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("create stores");
        let mut notification = MockNotificationSender::new();
        notification
            .expect_branch_pushed()
            .once()
            .return_once(|_, _, _, _, _| ());
        let notification = Arc::new(notification);
        let hooks = HookDispatcher::empty();
        let instruments = TestInstrumentProvider;

        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let repository = Arc::new(RepositoryContext::new_server_context(
                immutable_store.clone(),
                mutable_store.clone(),
                repository_id,
            ));
            create_root_branch(&repository, branch_id).await;
            branch::protect(repository.clone(), branch_id)
                .await
                .expect("protect branch");
            let operations = vec![create_directory("protected")];

            let denied = handler(
                request(
                    repository_id,
                    request_id,
                    branch_id,
                    Hash::default(),
                    operations.clone(),
                ),
                immutable_store.clone(),
                mutable_store.clone(),
                notification.clone(),
                &hooks,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                RevisionCreateLimits::default(),
                &instruments,
            )
            .await
            .expect_err("protected branch");
            assert_eq!(denied.code(), tonic::Code::PermissionDenied);
            assert!(
                branch::load_latest(repository.clone(), branch_id)
                    .await
                    .unwrap_or_default()
                    .is_zero()
            );

            let published = handler(
                service_account_request(
                    repository_id,
                    request_id,
                    branch_id,
                    Hash::default(),
                    operations,
                ),
                immutable_store,
                mutable_store,
                notification,
                &hooks,
                DEFAULT_HISTORY_STEP_SIZE,
                RevisionListAcceleration::default(),
                RevisionCreateLimits::default(),
                &instruments,
            )
            .await
            .expect("service account bypass")
            .into_inner();
            assert_eq!(published.revision_number, 1);
        }))
        .await;
    }

    #[test]
    fn revision_create_default_limits_are_changeset_not_file_limits() {
        let limits = RevisionCreateLimits::default();
        assert_eq!(limits.max_request_bytes, 4 * 1024 * 1024);
        assert_eq!(limits.max_operations, 10_000);
    }
}
