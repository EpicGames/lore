// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::any::Any;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::HealResult;
use lore_revision::lore::RepositoryId;
use lore_storage::ImmutableStore;
use lore_storage::LocalImmutableStore;
use lore_storage::StoreError;
use lore_storage::StoreMatch;
use tracing::debug;
use tracing::info;
use tracing::warn;
use zerocopy::FromBytes;

use crate::authnz::repository_authorizer::RepositoryAuthorizer;
use crate::correlation::CorrelationId;
use crate::protocol::attribute_map::AttributeMap;
use crate::protocol::attribute_map::get_user_id_from_context;
use crate::protocol::storage::messages::LoreResponse;
use crate::protocol::storage::messages::Message;
use crate::protocol::storage::messages::MessageHandleError;
use crate::protocol::storage::messages::MessageParseError;
use crate::protocol::storage::messages::Response;
use crate::util::setup_execution;

#[derive(Debug, PartialEq, FromBytes)]
#[repr(C)]
pub struct Verify {
    pub address: Address,
    pub heal: u8,
}

impl Display for Verify {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{:#?}", self.address)
    }
}

impl Verify {
    pub fn parse(bytes: Bytes) -> Result<Self, MessageParseError>
    where
        Self: Sized,
    {
        Verify::read_from_bytes(bytes.as_ref()).map_err(|_e| MessageParseError::InvalidFieldLength)
    }
}

/// Checks `local_store`'s copy of `address` and, where `heal_flag` asks, drops a corrupt one. A
/// heal of a copy that is corrupt or missing also asks `store` to heal its own, so a payload the
/// client refetches is sound. A write of the address to `local_store` that a read through `store`
/// started ends first, so the check sees the copy it leaves.
pub async fn handle_verify(
    address: Address,
    heal_flag: u8,
    repository: RepositoryId,
    correlation_id: String,
    user_id: String,
    local_store: Arc<dyn ImmutableStore>,
    store: Arc<dyn ImmutableStore>,
) -> Result<LoreResponse, MessageHandleError> {
    let execution = setup_execution(module_path!(), correlation_id, user_id);
    let heal = heal_flag != 0;

    debug!(%address, "Handling verify request for address");

    let match_requested = if address.context.is_zero() {
        StoreMatch::MatchPartition
    } else {
        StoreMatch::MatchFull
    };

    LORE_CONTEXT
        .scope(execution, async move {
            store.clone().settle(repository, address).await;
            let concrete_local_store: Arc<LocalImmutableStore> =
                {
                    let any_store: Arc<dyn Any + Send + Sync> = local_store;
                    any_store
                        .downcast::<LocalImmutableStore>()
                        .map_err(|_err| MessageHandleError::StoreFailure)?
                };

            match concrete_local_store
                .verify_fragment(address, repository, match_requested, heal)
                .await
            {
                Ok(result) => {
                    info!(%address, "Verify result: {result:?}");

                    match result.verification_result {
                        Ok(()) =>
                            {
                                Ok(LoreResponse::Verify(VerifyResponse {
                                    corrupted: 0,
                                    healed: HealResult::NotAttempted,
                                }))
                            }
                        Err(err) => {
                            let healed = if result.healed {
                                HealResult::Healed
                            } else if heal {
                                warn!(%address, error = %err, "Attempted to heal while verifying fragment, but result indicated we did not heal?");
                                HealResult::Failed
                            } else {
                                HealResult::NotAttempted
                            };
                            if heal {
                                heal_beyond_local(store, repository, address).await;
                            }
                            Ok(LoreResponse::Verify(VerifyResponse { corrupted: 1, healed }))
                        }
                    }
                }
                Err(StoreError::AddressNotFound(_)) => {
                    if heal
                        && heal_beyond_local(store, repository, address).await
                            == HealResult::Healed
                    {
                        return Ok(LoreResponse::Verify(VerifyResponse {
                            corrupted: 1,
                            healed: HealResult::Healed,
                        }));
                    }
                    info!(%address, "Fragment verification failed, fragment not found");
                    Err(MessageHandleError::FragmentNotFound)
                }
                Err(StoreError::SlowDown(_)) => Err(MessageHandleError::SlowDown),
                Err(e) => {
                    warn!(%address, error = ?e, "Fragment verification failed");
                    Err(MessageHandleError::StoreFailure)
                }
            }
        })
        .await
}

/// Asks `store` to heal its copy of `address` beyond the local one, and answers what it did. A
/// failure is logged and answered as [`HealResult::Failed`].
async fn heal_beyond_local(
    store: Arc<dyn ImmutableStore>,
    repository: RepositoryId,
    address: Address,
) -> HealResult {
    match store.heal(repository, address).await {
        Ok(healed) => {
            debug!(%address, ?healed, "Asked the store beyond the local one to heal");
            healed
        }
        Err(err) => {
            warn!(%address, error = %err, "Healing beyond the local store failed");
            HealResult::Failed
        }
    }
}

impl Verify {
    /// Handles the request on a connection whose session `context` holds, checking
    /// `local_store`'s copy and healing beyond it in `store`, as [`handle_verify`] does.
    #[tracing::instrument(name = "Verify::handle", skip_all)]
    pub async fn handle_with(
        &self,
        context: Arc<AttributeMap>,
        local_store: Arc<dyn ImmutableStore>,
        store: Arc<dyn ImmutableStore>,
    ) -> Result<LoreResponse, MessageHandleError> {
        let repository = *context
            .get_or::<RepositoryId, MessageHandleError>(MessageHandleError::NotConnected)?;
        let user_id = get_user_id_from_context(&context);
        let correlation_id = context.get::<CorrelationId>().unwrap_or_default();
        handle_verify(
            self.address,
            self.heal,
            repository,
            correlation_id.to_string(),
            user_id,
            local_store,
            store,
        )
        .await
    }
}

#[async_trait]
impl Message for Verify {
    /// With the local store alone known, nothing beyond it is healed; the services call
    /// [`Verify::handle_with`] with the store beyond it.
    async fn handle(
        &self,
        context: Arc<AttributeMap>,
        local_immutable_store: Arc<dyn ImmutableStore>,
        _repository_authorizer: Arc<dyn RepositoryAuthorizer>,
    ) -> Result<LoreResponse, MessageHandleError> {
        self.handle_with(
            context,
            local_immutable_store.clone(),
            local_immutable_store,
        )
        .await
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct VerifyResponse {
    pub corrupted: u8,
    pub healed: HealResult,
}

impl Response for VerifyResponse {
    fn data(&self) -> Vec<Bytes> {
        vec![Bytes::from(vec![self.corrupted, self.healed as u8])]
    }
}
