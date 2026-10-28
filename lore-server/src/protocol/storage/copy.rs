// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_revision::lore::RepositoryId;
use lore_storage::ImmutableStore;
use lore_storage::immutable_store::CopyBehavior;
use tracing::warn;

use crate::auth::jwt::AuthorizationToken;
use crate::authnz::repository_authorizer::Action;
use crate::authnz::repository_authorizer::RawToken;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::authnz::repository_authorizer::VerifiedToken;
use crate::correlation::CorrelationId;
use crate::protocol::attribute_map::AttributeMap;
use crate::protocol::attribute_map::get_user_id_from_context;
use crate::protocol::storage::messages::LoreResponse;
use crate::protocol::storage::messages::Message;
use crate::protocol::storage::messages::MessageHandleError;
use crate::protocol::storage::messages::MessageParseError;
use crate::protocol::storage::messages::Response;
use crate::util::setup_execution;

#[derive(Clone, Debug, PartialEq)]
pub struct Copy {
    pub source_repository: RepositoryId,
    pub source_address: Address,
    /// Destination context. The destination address is `(target_partition,
    /// source_address.hash, target_context)` — same hash, possibly different context. Allows
    /// in-partition payload duplication when only the dedup tag changes.
    pub target_context: Context,
}

impl Copy {
    /// Legacy urc/0.2 wire — 64 bytes, no `target_context` on the wire. The destination's context
    /// is implicitly the source's, preserving the behavior the protocol shipped with.
    pub fn parse(bytes: Bytes) -> Result<Self, MessageParseError> {
        if bytes.len() != 64 {
            return Err(MessageParseError::InvalidFieldLength);
        }
        let mut bytes = bytes;
        let source_repository = RepositoryId::from(&bytes.split_to(size_of::<RepositoryId>())[..]);
        let hash = Hash::from(bytes.split_to(size_of::<Hash>()));
        let context = Context::from(bytes.split_to(size_of::<Context>()));
        let source_address = Address { hash, context };
        Ok(Self {
            source_repository,
            source_address,
            target_context: context,
        })
    }

    /// lore-storage/0.4 wire — 80 bytes, with `target_context` on the tail. Lets the destination
    /// take a different context from the source, including the same-partition different-context
    /// case used for in-partition payload deduplication.
    pub fn parse_v4(bytes: Bytes) -> Result<Self, MessageParseError> {
        if bytes.len() != 80 {
            return Err(MessageParseError::InvalidFieldLength);
        }
        let mut bytes = bytes;
        let source_repository = RepositoryId::from(&bytes.split_to(size_of::<RepositoryId>())[..]);
        let hash = Hash::from(bytes.split_to(size_of::<Hash>()));
        let context = Context::from(bytes.split_to(size_of::<Context>()));
        let target_context = Context::from(bytes.split_to(size_of::<Context>()));
        let source_address = Address { hash, context };
        Ok(Self {
            source_repository,
            source_address,
            target_context,
        })
    }
}

/// Performs the copy itself; the caller has already authorized the source
/// partition (each transport's dispatch asks the shared authorizer with its
/// own caller's token).
///
/// `destination_context` selects the destination tuple's dedup tag — destination address is
/// `(destination_repository, source_address.hash, destination_context)`. Legacy urc/0.2 callers
/// pass the source's context so behavior is unchanged; lore-storage/0.4 callers can pass a
/// different context to perform in-partition or cross-partition duplication without payload
/// transfer.
pub async fn handle_copy(
    source_repository: RepositoryId,
    source_address: Address,
    destination_repository: RepositoryId,
    destination_context: Context,
    correlation_id: String,
    user_id: String,
    immutable_store: Arc<dyn ImmutableStore>,
) -> Result<LoreResponse, MessageHandleError> {
    let execution = setup_execution(module_path!(), correlation_id, user_id);

    LORE_CONTEXT
        .scope(execution, async move {
            match immutable_store
                .copy(
                    source_repository,
                    source_address,
                    destination_repository,
                    destination_context,
                    CopyBehavior {
                        durable: true,
                        do_not_replicate: false,
                    },
                )
                .await
            {
                Ok(()) => Ok(LoreResponse::Copy(CopyResponse::default())),
                Err(err) if err.is_address_not_found() => Err(MessageHandleError::FragmentNotFound),
                Err(err) => {
                    warn!(error = ?err, "Failed to copy fragment");
                    Err(MessageHandleError::StoreFailure)
                }
            }
        })
        .await
}

#[async_trait]
impl Message for Copy {
    #[tracing::instrument(name = "Copy::handle", skip_all)]
    async fn handle(
        &self,
        context: Arc<AttributeMap>,
        immutable_store: Arc<dyn ImmutableStore>,
        repository_authorizer: Arc<dyn RepositoryAuthorizer>,
    ) -> Result<LoreResponse, MessageHandleError> {
        let destination_repository = *context
            .get_or::<RepositoryId, MessageHandleError>(MessageHandleError::NotConnected)?;

        // Cross-partition copy needs to verify also the source partition authorization.
        // In-partition copies are authorized by the connection.
        if self.source_repository != destination_repository {
            let raw = context.get::<RawToken>();
            let claims = context.get::<AuthorizationToken>();
            let token = match (raw.as_deref(), claims.as_deref()) {
                (Some(raw), Some(claims)) => Some(VerifiedToken {
                    raw: &raw.0,
                    claims,
                }),
                _ => None,
            };
            repository_authorizer
                .check_repository_access(token.as_ref(), self.source_repository, Action::Read)
                .await
                .map_err(|status| {
                    MessageHandleError::AuthorizationFailure(status.message().to_string())
                })?;
        }

        let user_id = get_user_id_from_context(&context);
        let correlation_id = context.get::<CorrelationId>().unwrap_or_default();
        handle_copy(
            self.source_repository,
            self.source_address,
            destination_repository,
            // urc/0.2 has no target_context on the wire; `Copy::parse` filled this with the
            // source's context so the destination tuple is exactly the source tuple under the
            // destination repository — matches the protocol's shipped behavior.
            self.target_context,
            correlation_id.to_string(),
            user_id,
            immutable_store,
        )
        .await
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct CopyResponse {}

impl Response for CopyResponse {
    fn data(&self) -> Vec<Bytes> {
        vec![]
    }
}
