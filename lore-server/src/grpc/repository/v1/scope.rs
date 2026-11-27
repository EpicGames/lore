// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::types::Context;
use lore_proto::lore::model::v1 as model_v1;
use lore_revision::interface::ExecutionContext;
use lore_revision::repository::RepositoryContext;
use lore_revision::scope;
use lore_revision::scope::Refused;
use lore_revision::scope::Scope;
use lore_revision::scope::ScopeError;
use lore_revision::scope::ScopeId;
use lore_revision::state::no_link_traversal;
use lore_telemetry::InstrumentProvider;
use opentelemetry::KeyValue;
use tonic::Status;
use tracing::info;

use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::grpc::RepositoryAdministration;
use crate::grpc::forwarded_requests::CallerContext;
use crate::grpc::repository_administration;
use crate::grpc::run_detached;
use crate::grpc::warn_error_to_status;
use crate::util::setup_execution;

/// The name of the counter of the scope creates, renames, archives and restores that succeed, by
/// `verb`, a repeated one that writes nothing included. A create that restores an archived scope
/// counts as a restore, and a repeat of it, which finds the scope live, as a create.
const LIFECYCLE: &str = "scope.lifecycle";

/// A lifecycle write of a scope, the `verb` attribute of its count.
#[derive(Clone, Copy, Debug)]
pub(super) enum Verb {
    Create,
    Rename,
    Archive,
    Restore,
}

impl Verb {
    fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Rename => "rename",
            Self::Archive => "archive",
            Self::Restore => "restore",
        }
    }

    /// The name of the task that makes the write.
    fn task_name(self) -> &'static str {
        match self {
            Self::Create => "scope create",
            Self::Rename => "scope rename",
            Self::Archive => "scope archive",
            Self::Restore => "scope restore",
        }
    }
}

/// Counts a lifecycle write of `verb` that succeeded.
pub(super) fn count(instrument_provider: &impl InstrumentProvider, verb: Verb) {
    instrument_provider
        .counter(LIFECYCLE)
        .add(1, &[KeyValue::new("verb", verb.as_str())]);
}

/// The server context of the repository `caller` names and the execution context of its call.
pub(super) fn scope_context(
    caller: &CallerContext,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    module: &'static str,
) -> (Arc<RepositoryContext>, Arc<ExecutionContext>) {
    let execution = setup_execution(
        module,
        caller.correlation_id.clone(),
        caller.user_id.clone(),
    );
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        caller.repository_id,
        no_link_traversal(),
    ));
    (repository, execution)
}

/// The scope id `bytes` hold, refusing an id that isn't 16 non-zero bytes.
pub(crate) fn requested_scope_id(bytes: &[u8]) -> Result<ScopeId, Status> {
    let id = Context::from(bytes);
    if bytes.len() != size_of::<Context>() || id.is_zero() {
        return Err(Status::invalid_argument(
            "Scope id must be 16 bytes and not zero",
        ));
    }
    Ok(id)
}

/// The v1 `Scope` record of `scope`.
pub(super) fn build_scope(scope: Scope) -> model_v1::Scope {
    model_v1::Scope {
        id: scope.id.into(),
        name: scope.name,
        creator: scope.creator,
        created: scope.created,
        archived: scope.archived,
    }
}

/// The status a failed scope read or write answers. A store that asks for a slow down answers
/// `RESOURCE_EXHAUSTED`.
pub(super) fn scope_status(err: ScopeError) -> Status {
    if err.is_slow_down() {
        Status::resource_exhausted(err.to_string())
    } else if err.is_not_found() {
        Status::not_found(described(&err))
    } else if err.is_already_exists() {
        Status::already_exists(described(&err))
    } else if err.is_invalid_arguments() {
        Status::invalid_argument(err.to_string())
    } else {
        warn_error_to_status(&err, |err| Status::internal(err.to_string()))
    }
}

/// The message of `err`, followed by the last context its trace holds: the scope, for the errors
/// the scope writes build.
fn described(err: &ScopeError) -> String {
    match err
        .trace()
        .locations()
        .iter()
        .rev()
        .find_map(|location| location.context())
    {
        Some(context) => format!("{err}: {context}"),
        None => err.to_string(),
    }
}

/// The status a lifecycle write that found the scope in a state it doesn't apply to answers.
pub(super) fn refused_status(refused: Refused) -> Status {
    Status::failed_precondition(match refused {
        Refused::Default => "The default scope can't be renamed or archived",
        Refused::Archived => "The scope is archived, create it with its id to restore it first",
    })
}

/// Refuses with `failed_precondition` the default scope, before any authorization, since the
/// refusal holds for every caller and tells none of them anything about stored state. Refuses with
/// `permission_denied` a caller who may not administer the repository and didn't create scope
/// `id`, as [`repository_administration`] answers against the scope's recorded creator. The
/// creator is read only when the decision rests on it, and a scope without metadata is then
/// refused with `not_found`.
pub(super) async fn require_scope_administration(
    authorizer: &Arc<dyn RepositoryAuthorizer>,
    extensions: &tonic::Extensions,
    repository: Arc<RepositoryContext>,
    id: ScopeId,
    user_id: &str,
) -> Result<(), Status> {
    if id == scope::default_scope_id(repository.id) {
        return Err(refused_status(Refused::Default));
    }
    let administration = repository_administration(authorizer, extensions, repository.id).await?;
    let creator = if administration == RepositoryAdministration::CreatorOnly {
        scope::get(repository, id)
            .await
            .map_err(scope_status)?
            .ok_or_else(|| Status::not_found(format!("Scope {id} not found")))?
            .creator
    } else {
        String::new()
    };
    if administration.permits(&creator, user_id) {
        return Ok(());
    }
    info!(user_id, "Scope administration refused");
    Err(Status::permission_denied(
        "Not permitted to change this scope",
    ))
}

/// Makes the lifecycle write `verb` of scope `id`, which the future `write` builds makes, once the
/// caller may administer the scope ([`require_scope_administration`]), and answers the scope it
/// wrote. The write is built and runs to its end in a task of its own ([`run_detached`]), which
/// counts it once it succeeds, so the request's future holds only `write`'s captures while it
/// authorizes the caller.
#[allow(clippy::too_many_arguments)]
pub(super) async fn administer<Written>(
    authorizer: &Arc<dyn RepositoryAuthorizer>,
    extensions: &tonic::Extensions,
    repository: Arc<RepositoryContext>,
    id: ScopeId,
    user_id: &str,
    verb: Verb,
    instrument_provider: impl InstrumentProvider + Send + 'static,
    write: impl FnOnce() -> Written + Send + 'static,
) -> Result<model_v1::Scope, Status>
where
    Written: Future<Output = Result<Result<Scope, Refused>, ScopeError>> + Send + 'static,
{
    require_scope_administration(authorizer, extensions, repository, id, user_id).await?;
    let written = run_detached(verb.task_name(), async move {
        let written = write().await;
        if matches!(written, Ok(Ok(_))) {
            count(&instrument_provider, verb);
        }
        written
    })
    .await?
    .map_err(scope_status)?;
    written.map(build_scope).map_err(refused_status)
}
