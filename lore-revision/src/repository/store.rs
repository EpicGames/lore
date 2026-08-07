// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use lore_transport::quic::storage_service::QueryStatus;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinSet;

use super::RepositoryContext;
use super::RepositoryError;
use crate::event;
use crate::fragment::FragmentFlags;
use crate::lore::Address;
use crate::lore::Fragment;
use crate::lore::FragmentReference;
use crate::lore::TypedBytes;
use crate::lore_debug;
use crate::lore_error;
use crate::store::StoreMatch;
use crate::util::serde::u8_as_bool;

/// Result of a query against the immutable store for a single fragment.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryStoreImmutableQueryEventData {
    /// Address of fragment
    pub address: Address,
    /// Remote flag, true if results are from remote store, false if local store
    #[serde(with = "u8_as_bool")]
    pub remote: u8,
    /// Status, where
    /// 0 = exact address exist
    /// 1 = hash exist in repository
    /// 2 = hash exist in other repository
    /// 3 = hash does not exist
    pub status: u32,
    /// Payload flag, true if payload data is present in the store, false if not
    #[serde(with = "u8_as_bool")]
    pub payload: u8,
    /// Subfragment flag, true if this fragment was a subfragment of the original query, false if not
    #[serde(with = "u8_as_bool")]
    pub subfragment: u8,
    /// Internal flags
    pub flags: u32,
    /// Payload size
    pub payload_size: u32,
    /// Content size
    pub content_size: u64,
}

pub async fn immutable_query(
    repository: Arc<RepositoryContext>,
    address: String,
    local: bool,
    recurse: bool,
) -> Result<(), RepositoryError> {
    let address = Address::from_str(address.as_str()).internal("Invalid address")?;

    immutable_query_address(repository, address, local, recurse, false).await
}

async fn immutable_query_address(
    repository: Arc<RepositoryContext>,
    address: Address,
    local: bool,
    recurse: bool,
    subfragment: bool,
) -> Result<(), RepositoryError> {
    let mut maybe_fragment = None;
    let mut maybe_payload = None;

    let result = repository
        .immutable_store()
        .get_metadata(repository.id, address)
        .await
        .forward::<RepositoryError>("Unable to query immutable store")?;
    let has_local_payload = result.fragment.flags & FragmentFlags::PayloadStoredLocal != 0;

    event::LoreEvent::RepositoryStoreImmutableQuery(LoreRepositoryStoreImmutableQueryEventData {
        address,
        remote: 0,
        status: match result.match_made {
            StoreMatch::MatchFull => 0,
            StoreMatch::MatchPartition => 1,
            StoreMatch::MatchHash => 2,
            StoreMatch::MatchNone => 3,
        },
        payload: has_local_payload as u8,
        subfragment: subfragment as u8,
        flags: result.fragment.flags,
        payload_size: result.fragment.size_payload,
        content_size: result.fragment.size_content,
    })
    .send();

    // Verify the local payload
    if has_local_payload
        && let Ok((fragment, payload)) = repository
            .immutable_store()
            .get(repository.id, address)
            .await
            .and_then(lore_storage::StoreGetData::into_payload)
            .forward::<RepositoryError>("Failed to validate payload in local store")
    {
        lore_debug!(
            "Loaded fragment: {fragment:?} - payload {} bytes",
            payload.len()
        );
        maybe_fragment.replace(fragment);
        maybe_payload.replace(payload);
    }

    if !local
        && let Ok(remote) = repository.remote().await
        && let Ok(remote_storage) = {
            let correlation_id = crate::lore::execution_context()
                .globals()
                .correlation_id
                .to_string();
            remote.session(repository.id, &correlation_id).await
        }
        && let Ok(result) = remote_storage
            .query(&[address])
            .await
            .forward::<RepositoryError>("Unable to query remote store")
    {
        if let Some(raw_status) = result.first() {
            let status = QueryStatus::from(*raw_status);

            let fragment = if status == QueryStatus::ExistFullMatch
                && let Ok((fragment, payload)) = remote_storage.get(&address).await
            {
                maybe_fragment.replace(fragment);
                maybe_payload.replace(payload);
                fragment
            } else {
                Fragment::default()
            };

            event::LoreEvent::RepositoryStoreImmutableQuery(
                LoreRepositoryStoreImmutableQueryEventData {
                    address,
                    remote: 1,
                    status: match status {
                        QueryStatus::ExistFullMatch => 0,
                        QueryStatus::ExistPartitionMatch => 1,
                        QueryStatus::NotFound => 3,
                    },
                    payload: (fragment.size_payload != 0) as u8,
                    subfragment: subfragment as u8,
                    flags: fragment.flags,
                    payload_size: fragment.size_payload,
                    content_size: fragment.size_content,
                },
            )
            .send();
        } else {
            lore_error!("Server failed to respond with a valid result to fragment query");
        }
    }

    let mut tasks = JoinSet::new();

    if recurse
        && let Some(fragment) = maybe_fragment
        && fragment.flags & FragmentFlags::PayloadFragmented.as_u32() != 0
        && let Some(payload) = maybe_payload
    {
        // Query the subfragments
        let payload = payload.to_aligned::<FragmentReference>();
        for reference in payload.as_type_slice::<FragmentReference>() {
            let repository = repository.clone();
            let reference_address = Address {
                hash: reference.hash,
                context: address.context,
            };
            lore_spawn!(tasks, async move {
                immutable_query_subfragment(repository, reference_address, local).await
            });
        }
    }

    let mut failure = None;
    while let Some(result) = tasks.join_next().await {
        failure = failure.or(result
            .map_err(|e| RepositoryError::internal_with_context(e, "Failed to complete a task"))
            .and_then(|inner| inner)
            .err());
    }

    if let Some(err) = failure {
        return Err(err);
    }

    Ok(())
}

fn immutable_query_subfragment(
    repository: Arc<RepositoryContext>,
    address: Address,
    local: bool,
) -> Pin<Box<dyn Future<Output = Result<(), RepositoryError>> + Send>> {
    Box::pin(immutable_query_address(
        repository, address, local, true, true,
    ))
}
