// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use lore_revision::lore::RepositoryId;
use lore_telemetry::tracing::fields::USER_ID;
use tracing::debug;
use tracing::warn;

use crate::auth::jwt::AuthorizationToken;
use crate::auth::jwt::JwtVerifier;
use crate::authnz::repository_authorizer::Action;
use crate::authnz::repository_authorizer::PartitionGrants;
use crate::authnz::repository_authorizer::RawToken;
use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::authnz::repository_authorizer::VerifiedToken;
use crate::correlation::CorrelationId;
use crate::protocol::attribute_map::AttributeMap;
use crate::protocol::storage::messages::LoreResponse;
use crate::protocol::storage::messages::Message;
use crate::protocol::storage::messages::MessageHandleError;
use crate::protocol::storage::messages::MessageParseError;
use crate::protocol::storage::messages::Response;
use crate::util::get_user_id_from_token;

#[derive(Clone, Debug, PartialEq)]
pub struct Connect {
    pub repository: RepositoryId,
    pub auth_token: Option<String>,
}

impl Connect {
    pub fn parse(bytes: Bytes) -> Result<Self, MessageParseError>
    where
        Self: Sized,
    {
        if bytes.len() < size_of::<RepositoryId>() {
            return Err(MessageParseError::InvalidFieldLength);
        }

        let mut bytes = bytes;
        let context = bytes.split_to(size_of::<RepositoryId>()).into();

        let auth_token: Option<String> = if !bytes.is_empty() {
            String::from_utf8(bytes.to_vec()).ok()
        } else {
            None
        };

        Ok(Self {
            repository: context,
            auth_token,
        })
    }
}

#[async_trait]
impl Message for Connect {
    #[tracing::instrument(name = "Connect::handle_auth", skip_all)]
    async fn handle_auth(
        &self,
        context: Arc<AttributeMap>,
        jwt_verifier: Arc<Option<JwtVerifier>>,
        repository_authorizer: Arc<dyn RepositoryAuthorizer>,
    ) -> Result<LoreResponse, MessageHandleError> {
        // Make sure a correlation ID exists
        if context.get::<CorrelationId>().is_none() {
            warn!("Connection is missing correlation ID");
            let correlation_id = CorrelationId::default();

            if let Some(span) = context.get::<tracing::Span>() {
                span.record("correlation_id", correlation_id.to_string());
            }

            context.insert(correlation_id);
        }

        if let Some(span) = context.get::<tracing::Span>() {
            span.record("repository_id", self.repository.to_string());
        }

        debug!("Handling connect request");

        // Before any verification or context mutation: a rejected Connect must
        // not touch a connection already bound to another repository, or it
        // leaves that connection holding this request's token and grants.
        if let Some(id) = context.get::<RepositoryId>()
            && *id != self.repository
        {
            warn!("Attempted to set repository id for connection, but it was already set!");
            return Err(MessageHandleError::AlreadyConnected);
        }

        if let Some(jwt_verifier) = jwt_verifier.as_ref() {
            match self.auth_token.as_ref() {
                Some(auth_token) => {
                    let authorization = jwt_verifier
                        .verify_token(auth_token)
                        .await
                        .map_err(|err| MessageHandleError::AuthorizationFailure(err.to_string()))?;
                    let token = VerifiedToken {
                        raw: auth_token,
                        claims: &authorization,
                    };
                    let grants = repository_authorizer
                        .granted_access(Some(&token), self.repository)
                        .await
                        .map_err(|status| {
                            MessageHandleError::AuthorizationFailure(status.message().to_string())
                        })?;
                    if let Some(grants) = grants {
                        context.insert(PartitionGrants {
                            repository_id: self.repository,
                            grants,
                        });
                    }
                    // Both halves of the verified token, so a later command's
                    // check (copy's source) can rebuild a `VerifiedToken`.
                    context.insert(RawToken(auth_token.clone()));
                    context.insert(authorization.clone());
                    if let Some(span) = context.get::<tracing::Span>() {
                        span.record(USER_ID, get_user_id_from_token(Some(authorization)));
                    }
                }
                None => {
                    return Err(MessageHandleError::MissingToken);
                }
            }
        } else if let Some(grants) = repository_authorizer
            .granted_access(None, self.repository)
            .await
            .map_err(|status| {
                MessageHandleError::AuthorizationFailure(status.message().to_string())
            })?
        {
            // Enumerated without a token too, so each command is a set lookup
            // rather than an authorizer call.
            context.insert(PartitionGrants {
                repository_id: self.repository,
                grants,
            });
        }

        // A first connect, or a reconnect to the same repository; the
        // mismatched case returned above.
        context.insert(self.repository);
        Ok(LoreResponse::Connect(ConnectResponse::default()))
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct ConnectResponse {}

impl Response for ConnectResponse {
    fn data(&self) -> Vec<Bytes> {
        vec![]
    }
}

/// [`RepositoryAuthorizer::permits`] for a urc/0.2 connection: answered from
/// the grants connect stored, asked of the authorizer with the connection's
/// token otherwise. `None` before connect, when no partition is named.
pub async fn connection_permits(
    context: &AttributeMap,
    authorizer: &dyn RepositoryAuthorizer,
    action: Action,
) -> Option<bool> {
    let repository = *context.get::<RepositoryId>()?;
    if let Some(grants) = context
        .get::<PartitionGrants>()
        .filter(|grants| grants.repository_id == repository)
    {
        return Some(grants.grants.permits(action));
    }
    let raw = context.get::<RawToken>();
    let claims = context.get::<AuthorizationToken>();
    let token = match (raw.as_deref(), claims.as_deref()) {
        (Some(raw), Some(claims)) => Some(VerifiedToken {
            raw: &raw.0,
            claims,
        }),
        _ => None,
    };
    Some(
        authorizer
            .check_repository_access(token.as_ref(), repository, action)
            .await
            .is_ok(),
    )
}
