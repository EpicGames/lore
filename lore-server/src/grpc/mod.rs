// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use futures::FutureExt;
pub mod admin_service;
pub mod environment;
pub mod environment_service;
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

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;

mod grpc_internal_server;
mod replication_service;
pub mod tower;

pub use admin_service::LoreAdminService;
pub use grpc_internal_server::GrpcInternalServerBuilder;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_revision::branch::BranchError;
use lore_revision::diff::DiffError;
use lore_revision::find::FindError;
use lore_revision::immutable::ImmutableError;
use lore_revision::link::LinkError;
use lore_revision::lore::RepositoryId;
use lore_revision::metadata::MetadataError;
use lore_revision::metadata::branch::BranchMetadataError;
use lore_revision::metadata::repository::RepositoryMetadataError;
use lore_revision::repository::RepositoryError;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::repository::ServerContext;
use lore_revision::state::StateError;
use lore_storage::StoreError;
use lore_transport::grpc::CORRELATION_ID_HEADER;
use lore_transport::grpc::PARTITION_ID_KEY;
use lore_transport::grpc::REPOSITORY_ID_KEY;
pub use repository::LoreRepositoryV1Service;
pub use revision::LoreRevisionV1Service;
pub use revision_service::LoreRevisionService;
pub use server::GrpcServerBuilder;
pub use server::GrpcServiceSettings;
pub use server::GrpcTimeouts;
pub use storage_service::LoreStorageService;
pub use thinclient::LoreThinClientV1Service;
use tokio::sync::mpsc::Sender;
use tokio::time::timeout;
use tonic::Code;
use tonic::Extensions;
use tonic::Status;
use tonic::metadata::MetadataMap;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::auth::jwt::AuthorizationToken;
use crate::auth::jwt::ResourceMatcher;
use crate::authnz::repository_authorizer::RawToken;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::authnz::repository_authorizer::VerifiedToken;
use crate::authnz::repository_authorizer::VerifiedTokenOwned;
use crate::hooks::traits::HookError;
use crate::hooks::traits::StatusCode;
use crate::protocol::attribute_map::AttributeMap;
use crate::protocol::storage::messages::MessageHandleError;
use crate::util::get_user_id_from_token_ref;
use crate::util::resources_from_token;

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
            .check_repository_access_sync(token.as_ref(), repository_id, None)
            .is_some_and(|verdict| verdict.is_ok())
    })
}

pub fn get_user_id(extensions: &Extensions) -> String {
    let auth = extensions.get::<AuthorizationToken>();
    get_user_id_from_token_ref(auth)
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

pub fn is_owner_or_admin(extensions: &Extensions, repository: RepositoryId) -> bool {
    let user_permissions = user_permissions(extensions, repository);
    user_permissions.contains(&"owner".to_string())
        || user_permissions.contains(&"admin".to_string())
}

pub fn can_obliterate(extensions: &Extensions, repository: RepositoryId) -> bool {
    user_permissions(extensions, repository).contains(&"obliterate".to_string())
}

pub fn can_admin_lock(extensions: &Extensions, repository: RepositoryId) -> bool {
    user_permissions(extensions, repository).contains(&"migrate".to_string())
}

pub fn user_permissions(extensions: &Extensions, repository: RepositoryId) -> Vec<String> {
    let user_resources = resources_from_token(get_authorization(extensions).ok());
    ResourceMatcher::default().merged_permissions(&user_resources, repository)
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
/// Maps [`StatusCode`] variants to their corresponding gRPC status codes.
/// Non-rejection errors (timeout, panic, execution failure) map to `INTERNAL`.
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
        },
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

pub fn timeout_grpc<T>(
    duration: Duration,
    fut: impl Future<Output = Result<T, Status>>,
) -> impl Future<Output = Result<T, Status>> {
    timeout(duration, fut).map(|result| {
        result.unwrap_or_else(|_| Err(Status::cancelled("Request handler timeout exceeded")))
    })
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
