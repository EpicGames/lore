// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use crate::authnz::repository_authorizer::Action;
pub mod admin_service;
pub mod environment;
pub mod environment_service;
pub mod forwarded_lock_service;
pub mod forwarded_repository;
pub mod forwarded_requests;
pub mod forwarded_revision;
pub mod handlers;
pub mod lock_service;
pub mod notification_service;
pub mod repository;
pub mod repository_service;
pub mod revision;
pub mod revision_service;
pub mod server;
pub mod storage;
pub mod storage_service;
pub mod thinclient;

use std::pin::Pin;
use std::sync::Arc;
use std::task::Context as TaskContext;
use std::task::Poll;
use std::time::Duration;

use bytes::Bytes;

mod grpc_internal_server;
mod replication_service;
pub mod tower;

pub use admin_service::LoreAdminService;
use futures::Stream;
pub use grpc_internal_server::GrpcInternalServerBuilder;
use lore_base::error::Cancelled;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_revision::branch::BranchError;
use lore_revision::commit::CommitError;
use lore_revision::diff::DiffError;
use lore_revision::find::FindError;
use lore_revision::immutable::ImmutableError;
use lore_revision::link::LinkError;
use lore_revision::lore::RepositoryId;
use lore_revision::metadata::MetadataError;
use lore_revision::metadata::branch::BranchMetadataError;
use lore_revision::metadata::repository::RepositoryMetadataError;
use lore_revision::notification::NotificationSender;
use lore_revision::repository::RepositoryError;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::repository::ServerContext;
use lore_revision::runtime::Cancellation;
use lore_revision::state::StateError;
use lore_storage::StoreError;
use lore_telemetry::InstrumentProvider;
use lore_transport::grpc::CORRELATION_ID_HEADER;
use lore_transport::grpc::PARTITION_ID_KEY;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use pin_project::pin_project;
pub use repository::LoreRepositoryV1Service;
pub use revision::LoreRevisionV1Service;
pub use revision_service::LoreRevisionService;
pub use server::GrpcServerBuilder;
pub use server::GrpcServiceSettings;
pub use server::GrpcTimeouts;
pub use storage_service::LoreStorageService;
pub use thinclient::LoreThinClientV1Service;
use tokio::sync::mpsc::Sender;
use tokio::time::Sleep;
use tokio::time::sleep;
use tokio_util::sync::WaitForCancellationFutureOwned;
use tonic::Code;
use tonic::Extensions;
use tonic::Status;
use tonic::metadata::MetadataMap;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::auth::jwt::AuthorizationToken;
use crate::authnz::repository_authorizer::PartitionGrants;
use crate::authnz::repository_authorizer::RawToken;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::authnz::repository_authorizer::VerifiedToken;
use crate::authnz::repository_authorizer::VerifiedTokenOwned;
use crate::authnz::repository_authorizer::is_denial;
use crate::authnz::repository_authorizer::permits_with;
use crate::hooks::HookContext;
use crate::hooks::HookDispatcher;
use crate::hooks::HookPoint;
use crate::hooks::traits::HookError;
use crate::hooks::traits::StatusCode;
use crate::protocol::attribute_map::AttributeMap;
use crate::protocol::storage::messages::MessageHandleError;
use crate::util::core_hop::poll_with_request_cancellation;
use crate::util::core_hop::request_cancellation;
use crate::util::core_hop::request_is_cancelled;
use crate::util::get_user_id_from_token_ref;

/// Matches the Infrastructure alerting rule regex
/// for what counts as an internal status code error
pub fn is_code_considered_server_error(code: &Code) -> bool {
    matches!(code, Code::Internal | Code::Unavailable | Code::Cancelled)
}

pub(crate) fn simple_map_message_handle_error(error: MessageHandleError) -> Status {
    map_message_handle_error_to_status(&error, None, None)
}

pub fn map_message_handle_error_to_status(
    error: &MessageHandleError,
    message: Option<String>,
    details: Option<Bytes>,
) -> Status {
    let (code, message) = match error {
        MessageHandleError::AuthorizationFailure(err) => (
            Code::PermissionDenied,
            message.unwrap_or_else(|| format!("Authorization failed {err}")),
        ),
        MessageHandleError::MissingToken => (
            Code::Unauthenticated,
            message.unwrap_or_else(|| "Missing auth token".into()),
        ),
        MessageHandleError::AlreadyConnected => (
            Code::FailedPrecondition,
            message.unwrap_or_else(|| "Already connected".into()),
        ),
        MessageHandleError::BranchExists => (
            Code::AlreadyExists,
            message.unwrap_or_else(|| "Branch already exists".into()),
        ),
        MessageHandleError::BranchMismatch => (
            Code::InvalidArgument,
            message.unwrap_or_else(|| "Branch mismatch".into()),
        ),
        MessageHandleError::BranchProtected => (
            Code::PermissionDenied,
            message.unwrap_or_else(|| "Branch protected".into()),
        ),
        MessageHandleError::FragmentNotFound => (
            Code::NotFound,
            message.unwrap_or_else(|| "Fragment not found".into()),
        ),
        MessageHandleError::HashMismatch => (
            Code::InvalidArgument,
            message.unwrap_or_else(|| "Hash mismatch".into()),
        ),
        MessageHandleError::InvalidParentBranch => (
            Code::InvalidArgument,
            message.unwrap_or_else(|| "Invalid parent branch".into()),
        ),
        MessageHandleError::InternalError => (
            Code::Internal,
            message.unwrap_or_else(|| "Internal error".into()),
        ),
        MessageHandleError::MutableDataNotFound(hash) => (
            Code::NotFound,
            message.unwrap_or_else(|| format!("No data found for hash: {hash}")),
        ),
        MessageHandleError::NoSuchBranch => (
            Code::NotFound,
            message.unwrap_or_else(|| "No such branch".into()),
        ),
        MessageHandleError::NotConnected => (
            Code::FailedPrecondition,
            message.unwrap_or_else(|| "Not connected".into()),
        ),
        MessageHandleError::NotImplemented => (
            Code::Internal,
            message.unwrap_or_else(|| "Operation not implemented".into()),
        ),
        MessageHandleError::QueryResultSizeMismatch => (
            Code::Internal,
            message.unwrap_or_else(|| "Query result size mismatch".into()),
        ),
        MessageHandleError::StoreFailure => (
            Code::Internal,
            message.unwrap_or_else(|| "Store failure".into()),
        ),
        MessageHandleError::SlowDown => (
            Code::ResourceExhausted,
            message.unwrap_or_else(|| "slowdown".into()),
        ),
        MessageHandleError::Oversized => (
            Code::OutOfRange,
            message.unwrap_or_else(|| "Oversized fragment or blob".into()),
        ),
        MessageHandleError::Metadata => (
            Code::Internal,
            message.unwrap_or_else(|| "Metadata failure".into()),
        ),
        MessageHandleError::HashFailed => (
            Code::InvalidArgument,
            message.unwrap_or_else(|| "Hash failed".into()),
        ),
        MessageHandleError::InvalidFragment => (
            Code::InvalidArgument,
            message.unwrap_or_else(|| "Invalid fragment".into()),
        ),
        MessageHandleError::HandlerTimeout => (
            Code::Cancelled,
            message.unwrap_or_else(|| "Request Handler Timeout".into()),
        ),
        MessageHandleError::SessionLimitReached => (
            Code::Unavailable,
            message.unwrap_or_else(|| "Session limit reached".into()),
        ),
        MessageHandleError::InvalidArgument(err) => (
            Code::InvalidArgument,
            message.unwrap_or_else(|| format!("Invalid argument: {err}")),
        ),
    };

    Status::with_details(code, message, details.unwrap_or_default())
}

pub fn get_repository(metadata: &MetadataMap) -> Result<RepositoryId, Status> {
    let repo_id = metadata
        .get_bin(PARTITION_ID_KEY)
        .or_else(|| metadata.get_bin(REPOSITORY_ID_KEY))
        .ok_or_else(|| Status::invalid_argument("Missing repository ID"))?;

    let context: Context = repo_id
        .to_bytes()
        .map_err(|e| Status::invalid_argument(format!("Error converting repo ID: {e}")))?
        .into();

    Ok(context.into())
}

/// Decodes a repository id carried in a request body.
///
/// The all-zero id is refused because it identifies the namespace-root
/// partition, which holds the deployment's repository name table rather than
/// a repository.
pub fn requested_repository_id(id: &[u8]) -> Result<RepositoryId, Status> {
    let id = Context::from(id);
    if id.is_zero() {
        return Err(Status::invalid_argument("Repository id must not be zero"));
    }
    Ok(id.into())
}

pub fn get_authorization(extensions: &Extensions) -> Result<AuthorizationToken, Status> {
    match extensions.get::<AuthorizationToken>() {
        Some(auth) => Ok(auth.clone()),
        None => Err(Status::unauthenticated("Missing authorization")),
    }
}

/// Rebuild the interceptor-verified token from request extensions. `None`
/// when no interceptor ran (no verifier configured) or when either half is
/// missing.
pub fn get_verified_token(extensions: &Extensions) -> Option<VerifiedToken<'_>> {
    let claims = extensions.get::<AuthorizationToken>()?;
    let raw = extensions.get::<RawToken>()?;
    Some(VerifiedToken {
        raw: &raw.0,
        claims,
    })
}

/// The cross-partition link-read check for a revision-graph traversal: may
/// the caller behind `extensions` reach a partition a link points into?
///
/// Asked synchronously, potentially many times per request, so it consults
/// [`RepositoryAuthorizer::check_repository_access_sync`] with the verified
/// token and denies when the authorizer cannot answer without I/O. The
/// partition-access layer's `PartitionGrants` extension does not apply: it
/// answers for the request's own partition, and a link points elsewhere.
pub fn link_read_authorizer(
    authorizer: &Arc<dyn RepositoryAuthorizer>,
    extensions: &Extensions,
) -> lore_revision::state::CanReadRepository {
    let authorizer = authorizer.clone();
    let token = get_verified_token(extensions).map(|token| token.owned());
    Arc::new(move |repository_id| {
        let token = token.as_ref().map(VerifiedTokenOwned::as_token);
        authorizer
            .check_repository_access_sync(token.as_ref(), repository_id, Action::Read)
            .is_some_and(|verdict| verdict.is_ok())
    })
}

pub fn get_user_id(extensions: &Extensions) -> String {
    let auth = extensions.get::<AuthorizationToken>();
    get_user_id_from_token_ref(auth)
}

/// Whether the caller's verified token marks it as a service account. False
/// without a token.
pub fn get_is_service_account(extensions: &Extensions) -> bool {
    extensions
        .get::<AuthorizationToken>()
        .and_then(|token| token.is_service_account)
        .unwrap_or(false)
}

/// Marker that opts the gRPC server crate into [`RepositoryWriteToken::server`].
///
/// Defined here (private) so the only path to mint a server token in this
/// crate is via [`get_write_token`] below.
struct LoreServer;
impl ServerContext for LoreServer {}
const LORE_SERVER: LoreServer = LoreServer;

/// Mint a fresh server-side write token. Server contexts are always writable
/// (the storage layer's per-bucket `RwLock`s are the actual concurrency
/// boundary), so handlers can call this directly at the start of a handler
/// without consulting the `RepositoryContext`.
pub fn get_write_token() -> RepositoryWriteToken {
    RepositoryWriteToken::server(&LORE_SERVER)
}

/// The notification sender, the post hooks' dispatcher and context, and the instrument provider a
/// branch create or delete uses once its write succeeds.
pub(crate) struct BranchWriteFollowUp<P> {
    pub(crate) notification_sender: Arc<dyn NotificationSender>,
    pub(crate) hook_dispatcher: Arc<HookDispatcher>,
    pub(crate) hook_context: HookContext,
    pub(crate) instrument_provider: P,
}

/// Creates `branch` in `repository`, as `lore_revision::branch::create` does without a dry run or
/// linked repositories, in a task that runs to its end ([`run_detached`]). Once the create
/// succeeds, the task sends its notification, spawns its post hooks and counts it, with what
/// `follow_up` holds. A request dropped part way stops neither the create between its writes nor
/// what follows it.
///
/// Not an `async fn`, which would hold its arguments until its first poll, and spawn only then.
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_branch_detached(
    repository: Arc<lore_revision::repository::RepositoryContext>,
    branch: lore_revision::lore::BranchId,
    name: String,
    category: String,
    creator: String,
    created: u64,
    stack: Vec<lore_base::types::BranchPoint>,
    follow_up: BranchWriteFollowUp<impl InstrumentProvider + Send + 'static>,
) -> impl Future<Output = Result<Result<Context, BranchError>, Status>> {
    run_detached("branch create", async move {
        let repository_id = repository.id;
        let branch = lore_revision::branch::create(
            repository,
            &get_write_token(),
            branch,
            name.as_str(),
            category.as_str(),
            creator.as_str(),
            created,
            stack,
            false,
            false,
        )
        .await?;
        follow_up
            .notification_sender
            .branch_created(repository_id, branch)
            .await;
        follow_up
            .hook_dispatcher
            .spawn_post(HookPoint::BranchCreate, follow_up.hook_context);
        follow_up
            .instrument_provider
            .counter("num_branches_created")
            .add(1, &[]);
        Ok::<_, BranchError>(branch)
    })
}

/// Archives `branch` in `repository`, as `lore_revision::branch::delete` does, in a task that runs
/// to its end ([`run_detached`]). Once the archive succeeds, the task sends its notification,
/// spawns its post hooks and counts it, with what `follow_up` holds. A request dropped part way
/// stops neither a settle nor an archive between its writes, nor what follows them.
///
/// Not an `async fn`, which would hold its arguments until its first poll, and spawn only then.
pub(crate) fn delete_branch_detached(
    repository: Arc<lore_revision::repository::RepositoryContext>,
    branch: lore_revision::lore::BranchId,
    follow_up: BranchWriteFollowUp<impl InstrumentProvider + Send + 'static>,
) -> impl Future<Output = Result<Result<(), BranchError>, Status>> {
    run_detached("branch delete", async move {
        let repository_id = repository.id;
        lore_revision::branch::delete(repository, branch).await?;
        follow_up
            .notification_sender
            .branch_deleted(repository_id, branch)
            .await;
        follow_up
            .hook_dispatcher
            .spawn_post(HookPoint::BranchDelete, follow_up.hook_context);
        follow_up
            .instrument_provider
            .counter("num_branches_deleted")
            .add(1, &[]);
        Ok::<_, BranchError>(())
    })
}

/// Spawns `task`, named `what` in errors, in the current span, and answers its output. A request
/// dropped part way, at its deadline or by its client, drops only the task's `JoinHandle`, which
/// detaches the task, so the task isn't stopped between its writes. A task that panics fails as
/// internal, with a warning.
///
/// Not an `async fn`, which would hold `task` in its future as well as spawn it: the task starts
/// at the call, and the answer holds only its handle.
#[lore_macro::test_pub]
fn run_detached<T: Send + 'static>(
    what: &'static str,
    task: impl Future<Output = T> + Send + 'static,
) -> impl Future<Output = Result<T, Status>> {
    let handle = lore_base::lore_spawn!(tracing::Instrument::in_current_span(
        crate::util::core_hop::outliving_request(task)
    ));
    async move {
        handle.await.map_err(|err| {
            warn_error_to_status(&err, |err| {
                Status::internal(format!("The {what}'s task failed: {err}"))
            })
        })
    }
}

pub(crate) fn metadata_to_attribute(
    metadata: &MetadataMap,
    extensions: &Extensions,
) -> Result<AttributeMap, Status> {
    let repository = get_repository(metadata)?;
    let attr_map = AttributeMap::default();
    attr_map.insert(repository);

    // Both halves of the verified token, so a handler working from the
    // attribute map (copy's source check) can rebuild a `VerifiedToken`.
    if let Ok(token) = get_authorization(extensions) {
        attr_map.insert(token);
    }
    if let Some(raw) = extensions.get::<RawToken>() {
        attr_map.insert(raw.clone());
    }

    Ok(attr_map)
}

pub fn interpret_streaming_error(err: Status) -> Status {
    // Surfaced from tonic crate src/codec/decode.rs
    // An abrupt client error has occurred where they were streaming data then suddenly
    // they have dropped.
    if err.code() == Code::Internal && err.message() == "Unexpected EOF decoding stream." {
        return Status::invalid_argument(format!("Probable client disconnect: {}", err.message()));
    }

    err
}

pub(crate) async fn send_err<T>(status: Status, tx: Sender<Result<T, Status>>) {
    let rpc_status_code = rpc_code_to_str(&status.code());

    if is_code_considered_server_error(&status.code()) {
        warn!(response = ?status, rpc_status_code, "GRPC service send_err - server error");
    } else {
        info!(response = ?status, rpc_status_code, "GRPC service send_err - user error");
    }
    if let Err(e) = tx.send(Err(status)).await {
        debug!(send_error = ?e, "GRPC service error performing send_err");
    }
}

/// Warns if the status code is a server error — call at unified response points so internal failures are observable even when the error path didn't go through `warn_error_to_status`.
pub(crate) fn log_server_error(status: &Status) {
    if is_code_considered_server_error(&status.code()) {
        warn!(
            response = ?status,
            rpc_status_code = rpc_code_to_str(&status.code()),
            "GRPC handler server error response",
        );
    }
}

/// What the caller's grants allow for administering a repository, before
/// the recorded creator is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepositoryAdministration {
    /// `owner` or `admin`; also `write` without a verified token, where there
    /// is no identity to compare with the creator: every caller is
    /// `<unknown>` while the recorded creator is whatever identity the client
    /// sent at create.
    Permitted,
    /// `write`: permitted only to the recorded creator.
    CreatorOnly,
    Denied,
    /// The caller's enumerated grants do not reach the repository at all. A
    /// handler whose refusal would tell an existing repository from a missing
    /// one answers this as the missing one.
    Unreachable,
}

impl RepositoryAdministration {
    pub fn permits(self, creator: &str, user_id: &str) -> bool {
        match self {
            Self::Permitted => true,
            Self::CreatorOnly => creator == user_id,
            Self::Denied | Self::Unreachable => false,
        }
    }
}

/// Answered from the grants the partition-access layer enumerated for
/// `repository` when the request carries them, else from one enumeration,
/// else per action. `Err` when a check failed rather than denied.
pub async fn repository_administration(
    authorizer: &Arc<dyn RepositoryAuthorizer>,
    extensions: &Extensions,
    repository: RepositoryId,
) -> Result<RepositoryAdministration, Status> {
    let token = get_verified_token(extensions);
    let enumerated;
    let grants = if let Some(grants) = extensions
        .get::<PartitionGrants>()
        .filter(|grants| grants.repository_id == repository)
    {
        Some(&grants.grants)
    } else {
        match authorizer.granted_actions(token.as_ref(), repository).await {
            Ok(grants) => enumerated = grants,
            Err(status) if is_denial(&status) => return Ok(RepositoryAdministration::Denied),
            Err(status) => return Err(status),
        }
        enumerated.as_ref()
    };
    if grants.is_some_and(|grants| !grants.reachable()) {
        return Ok(RepositoryAdministration::Unreachable);
    }
    let permits = async |action| {
        permits_with(&**authorizer, grants, token.as_ref(), repository, action).await
    };
    Ok(
        if permits(Action::Owner).await? || permits(Action::Admin).await? {
            RepositoryAdministration::Permitted
        } else if !permits(Action::Write).await? {
            RepositoryAdministration::Denied
        } else if token.is_none() {
            RepositoryAdministration::Permitted
        } else {
            RepositoryAdministration::CreatorOnly
        },
    )
}

/// Whether `user_id` may administer `repository`, recorded as created by
/// `creator`, per [`RepositoryAdministration`]. `Err` when a check failed
/// rather than denied.
pub async fn may_administer_repository(
    authorizer: &Arc<dyn RepositoryAuthorizer>,
    extensions: &Extensions,
    repository: RepositoryId,
    creator: &str,
    user_id: &str,
) -> Result<bool, Status> {
    Ok(
        repository_administration(authorizer, extensions, repository)
            .await?
            .permits(creator, user_id),
    )
}

/// The creator recorded in `repository`'s metadata. Reads only, so it does
/// not repair the name mapping and may run before the caller is authorized.
pub async fn repository_creator(
    repository: Arc<lore_revision::repository::RepositoryContext>,
) -> Result<String, Status> {
    let to_status = |err: &lore_revision::repository::RepositoryError| {
        if err.is_not_found() || err.is_repository_not_found() || err.is_address_not_found() {
            Status::not_found("Repository does not exist")
        } else {
            Status::internal(format!("Failed to read repository metadata: {err}"))
        }
    };
    let metadata_hash = lore_revision::repository::metadata_hash(repository.clone())
        .await
        .filter_slow_down()?
        .warn_map_err(to_status)?;
    let metadata = lore_revision::repository::metadata(repository, metadata_hash)
        .await
        .filter_slow_down()?
        .warn_map_err(to_status)?;
    Ok(metadata.creator)
}

/// Refuses with `permission_denied` unless the caller may administer
/// `repository` or holds [`Action::SetProtected`]. The recorded creator is
/// read only when the decision rests on it.
pub async fn require_branch_protection_administration(
    authorizer: &Arc<dyn RepositoryAuthorizer>,
    extensions: &Extensions,
    repository: Arc<lore_revision::repository::RepositoryContext>,
    user_id: &str,
) -> Result<(), Status> {
    if authorizer
        .permits(extensions, repository.id, Action::SetProtected)
        .await?
    {
        return Ok(());
    }
    let permitted = match repository_administration(authorizer, extensions, repository.id).await? {
        RepositoryAdministration::Permitted => true,
        RepositoryAdministration::CreatorOnly => repository_creator(repository).await? == user_id,
        RepositoryAdministration::Denied | RepositoryAdministration::Unreachable => false,
    };
    if permitted {
        Ok(())
    } else {
        info!(user_id, "Repository administration refused");
        Err(Status::permission_denied(
            "Not permitted to change branch protection",
        ))
    }
}

/// Whether the caller may perform `action` on the partition the request's
/// metadata names, answered from the partition-access layer's grants when
/// present. Denial is the uniform [`no_repository_access_status`].
pub async fn require_action<T>(
    authorizer: &Arc<dyn RepositoryAuthorizer>,
    request: &tonic::Request<T>,
    action: Action,
) -> Result<(), Status> {
    let repository = get_repository(request.metadata())?;
    if authorizer
        .permits(request.extensions(), repository, action)
        .await?
    {
        Ok(())
    } else {
        Err(no_repository_access_status())
    }
}

pub fn extract_correlation_id<B>(request: &tonic::Request<B>) -> Option<String> {
    match request.metadata().get(CORRELATION_ID_HEADER) {
        Some(val) => val.to_str().map(|s| s.to_string()).ok(),
        None => None,
    }
}

pub fn extract_authorization_header<B>(request: &tonic::Request<B>) -> Option<String> {
    request
        .metadata()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(|s| s.to_string())
}

pub fn rpc_code_to_str(code: &Code) -> &'static str {
    match code {
        Code::Ok => "Ok",
        Code::Cancelled => "Cancelled",
        Code::Unknown => "Unknown",
        Code::InvalidArgument => "InvalidArgument",
        Code::DeadlineExceeded => "DeadlineExceeded",
        Code::NotFound => "NotFound",
        Code::AlreadyExists => "AlreadyExists",
        Code::PermissionDenied => "PermissionDenied",
        Code::ResourceExhausted => "ResourceExhausted",
        Code::FailedPrecondition => "FailedPrecondition",
        Code::Aborted => "Aborted",
        Code::OutOfRange => "OutOfRange",
        Code::Unimplemented => "Unimplemented",
        Code::Internal => "Internal",
        Code::Unavailable => "Unavailable",
        Code::DataLoss => "DataLoss",
        Code::Unauthenticated => "Unauthenticated",
    }
}

pub trait ServerResultExt<T, E> {
    fn warn_map_err<Callback>(self, map: Callback) -> Result<T, Status>
    where
        Callback: FnOnce(&E) -> Status;
}

impl<T, E> ServerResultExt<T, E> for Result<T, E>
where
    E: std::error::Error,
{
    fn warn_map_err<Callback>(self, map: Callback) -> Result<T, Status>
    where
        Callback: FnOnce(&E) -> Status,
    {
        match self {
            Ok(t) => Ok(t),
            Err(error) => {
                let response = warn_error_to_status(&error, map);
                Err(response)
            }
        }
    }
}

pub fn warn_error_to_status<E, Callback>(error: &E, map: Callback) -> Status
where
    E: std::error::Error,
    Callback: FnOnce(&E) -> Status,
{
    let response = map(error);
    warn_mapped_error_status(error, &response);
    response
}

pub fn warn_mapped_error_status<E>(error: &E, response: &Status)
where
    E: std::error::Error,
{
    if !is_code_considered_server_error(&response.code()) {
        return;
    }
    let rpc_status_code = rpc_code_to_str(&response.code());
    warn!(?error, ?response, rpc_status_code, "error status");
}

/// Converts a [`HookError`] into a [`tonic::Status`].
///
/// Maps [`StatusCode`] variants to their corresponding gRPC status codes. A
/// timeout maps to `UNAVAILABLE`, so the client may retry. Other non-rejection
/// errors (panic, execution failure) map to `INTERNAL`.
pub fn hook_error_to_status(error: HookError) -> Status {
    match &error {
        HookError::Rejected {
            message, status, ..
        } => match status {
            StatusCode::PermissionDenied => Status::permission_denied(message),
            StatusCode::FailedPrecondition => Status::failed_precondition(message),
            StatusCode::ResourceExhausted => Status::resource_exhausted(message),
            StatusCode::InvalidArgument => Status::invalid_argument(message),
            StatusCode::Aborted => Status::aborted(message),
            StatusCode::Internal => Status::internal(message),
            StatusCode::Unavailable => Status::unavailable(message),
        },
        HookError::Timeout { .. } => Status::unavailable(error.to_string()),
        _ => Status::internal(error.to_string()),
    }
}

pub fn no_repository_access_status() -> Status {
    Status::permission_denied("Unauthorized")
}

/// What an authorization check answers with when its own bound elapses,
/// wherever that check is made. A server condition rather than a denial, and
/// worded apart from a handler timeout so that which of the two elapsed stays
/// legible in logs and metrics.
pub fn authorization_timeout_status() -> Status {
    Status::cancelled("Authorization timeout exceeded")
}

/// Runs a request handler with a deadline.
///
/// At the deadline the handler is cancelled, not dropped: it runs on to its next cancellation
/// check, so a write it has started is never cut off part way. A handler that still fails as
/// cancelled reports the timeout, and one that completes regardless keeps its result.
pub fn timeout_grpc<T, F>(duration: Duration, fut: F) -> TimeoutGrpc<F>
where
    F: Future<Output = Result<T, Status>>,
{
    TimeoutGrpc {
        handler: fut,
        sleep: sleep(duration),
        duration,
        deadline: None,
        elapsed: false,
    }
}

/// The future [`timeout_grpc`] returns. It holds the handler future once, in place.
#[pin_project]
pub struct TimeoutGrpc<F> {
    #[pin]
    handler: F,
    #[pin]
    sleep: Sleep,
    duration: Duration,
    /// The handler's cancellation: a child of the request's, made on first poll.
    deadline: Option<Cancellation>,
    elapsed: bool,
}

impl<T, F> Future for TimeoutGrpc<F>
where
    F: Future<Output = Result<T, Status>>,
{
    type Output = Result<T, Status>;

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let deadline = this.deadline.get_or_insert_with(|| {
            request_cancellation().map_or_else(Cancellation::new, |request| request.child())
        });
        let handler = this.handler;
        if let Poll::Ready(result) =
            poll_with_request_cancellation(deadline.clone(), || handler.poll(cx))
        {
            return Poll::Ready(match result {
                // A client that left after the deadline keeps its own status. Any other
                // CANCELLED returned after the deadline is reported as the timeout.
                Err(status)
                    if *this.elapsed
                        && status.code() == Code::Cancelled
                        && !deadline.is_parent_cancelled() =>
                {
                    Err(Status::cancelled("Request handler timeout exceeded"))
                }
                result => result,
            });
        }
        if !*this.elapsed && this.sleep.poll(cx).is_ready() {
            warn!(
                timeout_ms = this.duration.as_millis(),
                "Request handler timeout exceeded, cancelling"
            );
            *this.elapsed = true;
            // Wakes the handler if it waits for cancellation, so it is polled again to see it.
            deadline.cancel();
        }
        Poll::Pending
    }
}

pub trait FilterSlowDownExt<T, E> {
    fn filter_slow_down(self) -> Result<Result<T, E>, Status>;
}

/// Implements [`FilterSlowDownExt`] for error sets that declare a `SlowDown`
/// variant: the backpressure signal becomes `RESOURCE_EXHAUSTED`, and every
/// other outcome passes through for the caller to match on.
macro_rules! impl_filter_slow_down {
    ($($error:ty),+ $(,)?) => {
        $(
            impl<T> FilterSlowDownExt<T, $error> for Result<T, $error> {
                fn filter_slow_down(self) -> Result<Result<T, $error>, Status> {
                    if let Err(err) = &self
                        && err.is_slow_down()
                    {
                        return Err(Status::resource_exhausted(err.to_string()));
                    }
                    Ok(self)
                }
            }
        )+
    };
}

impl_filter_slow_down!(
    BranchError,
    BranchMetadataError,
    CommitError,
    DiffError,
    FindError,
    ImmutableError,
    LinkError,
    MetadataError,
    RepositoryError,
    RepositoryMetadataError,
    StateError,
    StoreError,
);

/// Fails with `CANCELLED` once the request being served has been cancelled, whether or not the
/// caller has entered the request's execution context yet.
///
/// Call it only where returning leaves no partial write behind, as with
/// [`lore_revision::runtime::check_cancelled`].
pub fn check_request_cancelled() -> Result<(), Status> {
    if request_is_cancelled() {
        return Err(Status::cancelled(Cancelled.to_string()));
    }
    Ok(())
}

/// `stream`, ending once the request being served is cancelled rather than at its next message.
pub fn until_request_cancelled<S: Stream>(stream: S) -> UntilCancelled<S> {
    UntilCancelled {
        stream,
        cancellation: request_cancellation(),
        waiting: None,
        ended: false,
    }
}

/// The stream [`until_request_cancelled`] returns. Each poll reads the cancellation's flags
/// before polling the inner stream, and only an inner stream that is waiting also waits on the
/// cancellation, so a message costs no lock.
#[pin_project]
pub struct UntilCancelled<S> {
    #[pin]
    stream: S,
    cancellation: Option<Cancellation>,
    #[pin]
    waiting: Option<WaitForCancellationFutureOwned>,
    ended: bool,
}

impl<S: Stream> Stream for UntilCancelled<S> {
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<S::Item>> {
        let mut this = self.project();
        if *this.ended {
            return Poll::Ready(None);
        }
        if this
            .cancellation
            .as_ref()
            .is_some_and(Cancellation::is_cancelled)
        {
            *this.ended = true;
            return Poll::Ready(None);
        }
        if let Poll::Ready(item) = this.stream.poll_next(cx) {
            *this.ended = item.is_none();
            return Poll::Ready(item);
        }
        let Some(cancellation) = this.cancellation.as_ref() else {
            return Poll::Pending;
        };
        if this.waiting.is_none() {
            this.waiting.set(Some(cancellation.cancelled()));
        }
        if let Some(waiting) = this.waiting.as_pin_mut()
            && waiting.poll(cx).is_ready()
        {
            *this.ended = true;
            return Poll::Ready(None);
        }
        Poll::Pending
    }
}

pub trait FilterCancelledExt<T, E> {
    fn filter_cancelled(self) -> Result<Result<T, E>, Status>;
}

/// Implements [`FilterCancelledExt`] for error sets that declare a `Cancelled` variant: an
/// operation that stopped because nobody waits for it any more becomes `CANCELLED`, and every
/// other outcome passes through for the caller to match on.
macro_rules! impl_filter_cancelled {
    ($($error:ty),+ $(,)?) => {
        $(
            impl<T> FilterCancelledExt<T, $error> for Result<T, $error> {
                fn filter_cancelled(self) -> Result<Result<T, $error>, Status> {
                    if let Err(err) = &self
                        && err.is_cancelled()
                    {
                        return Err(Status::cancelled(err.to_string()));
                    }
                    Ok(self)
                }
            }
        )+
    };
}

impl_filter_cancelled!(
    BranchError,
    DiffError,
    LinkError,
    RepositoryError,
    StateError
);

/// Converts a failed result into `Ok(None)` when `discard` accepts the error,
/// and into a [`Status`] otherwise.
///
/// The failing path routes through [`FilterSlowDownExt::filter_slow_down`]
/// first, so an error that carries its own status keeps it and only the
/// remainder is reported as internal. A caller that can act on `None` keeps
/// that path without also discarding the errors it cannot act on.
pub fn none_or_status<T, E>(
    result: Result<T, E>,
    discard: impl FnOnce(&E) -> bool,
) -> Result<Option<T>, Status>
where
    Result<T, E>: FilterSlowDownExt<T, E>,
    E: std::error::Error,
{
    match result.filter_slow_down()? {
        Ok(value) => Ok(Some(value)),
        Err(err) if discard(&err) => Ok(None),
        Err(err) => Err(warn_error_to_status(&err, |err| {
            Status::internal(err.to_string())
        })),
    }
}

/// Reads a revision hash out of a request's `signature` field, refusing a
/// signature that is not a whole one.
///
/// A partial hash signature is a request the server cannot act on rather than a
/// revision it looked for and did not find, so it is refused as
/// `FAILED_PRECONDITION`: retrying it unchanged fails the same way.
///
/// An empty field is not a partial signature but an unset one, and is left to
/// the zero hash its callers already handle.
pub fn revision_signature(signature: Bytes) -> Result<Hash, Status> {
    if !signature.is_empty() && signature.len() != size_of::<Hash>() {
        return Err(Status::failed_precondition(format!(
            "partial revision hash signature of {} byte(s) - give the whole {} byte signature, or a branch and revision number",
            signature.len(),
            size_of::<Hash>(),
        )));
    }

    Ok(Hash::from(signature))
}
