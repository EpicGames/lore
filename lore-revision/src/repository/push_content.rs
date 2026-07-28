// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `repository push-content` — push locally-stored, not-yet-durable content to
//! the remote store.
//!
//! A commit whose remote upload fails (e.g. a timeout) keeps the payload in
//! the local store with `PayloadStoredDurable` clear. Other clients then fail
//! to sync the published revision with `Address not found`. This command
//! re-uploads such payloads from the machine that still holds them:
//!
//! - `scan == true`: enumerate every non-durable entry for this repository's
//!   partition in the local store and upload each one.
//! - otherwise: upload the explicitly given addresses (`hash` or
//!   `hash-context` strings, matching the format printed in errors).
//!
//! Each item emits a [`LoreStorageUploadItemCompleteEventData`] event; the
//! call fails if any item fails.

use std::str::FromStr;
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use lore_storage::concurrency::acquire_fragment_memory_permit;
use lore_storage::options::ReadOptions;
use lore_storage::read::load_fragment;
use lore_storage::store_types::StoreMatch;
use lore_storage::write::store_fragment;
use lore_transport::StorageSession;
use tokio::task::JoinSet;

use super::RepositoryContext;
use super::RepositoryError;
use crate::event;
use crate::fragment::FragmentFlags;
use crate::interface::LoreString;
use crate::lore::Address;
use crate::lore::Context;
use crate::lore::Hash;
use crate::lore::Partition;
use crate::lore::execution_context;
use crate::lore_info;
use crate::store::event::LoreStorageUploadItemCompleteEventData;

/// Arguments for `push_content` from the library layer.
pub struct PushContentArgs {
    /// Explicit addresses to push, as `hash` or `hash-context` strings.
    pub addresses: Vec<LoreString>,
    /// Scan the local store for all non-durable entries instead.
    pub scan: bool,
}

pub async fn push_content(
    repository: Arc<RepositoryContext>,
    args: PushContentArgs,
) -> Result<(), RepositoryError> {
    let items: Vec<(Partition, Address)> = if args.scan {
        scan_non_durable(&repository).await?
    } else {
        let mut items = Vec::with_capacity(args.addresses.len());
        for address in &args.addresses {
            items.push((repository.id, parse_address(address.as_str())?));
        }
        items
    };

    if items.is_empty() {
        lore_info!("No non-durable content found; nothing to push");
        return Ok(());
    }
    lore_info!("Pushing {} content fragment(s) to remote", items.len());

    let remote = repository
        .remote()
        .await
        .forward::<RepositoryError>("Push content failed: no remote connection")?;
    let correlation_id = execution_context().globals().correlation_id.to_string();
    let session = remote
        .session(repository.id, &correlation_id)
        .await
        .forward::<RepositoryError>("Push content failed: failed to connect")?;

    let mut tasks: JoinSet<Result<(), RepositoryError>> = JoinSet::new();
    for (index, (partition, address)) in items.into_iter().enumerate() {
        let repository = repository.clone();
        let session = session.clone();
        lore_spawn!(tasks, async move {
            push_item(repository, index as u64, partition, address, session).await
        });
    }

    let mut result: Result<(), RepositoryError> = Ok(());
    while let Some(task_result) = tasks.join_next().await {
        let item_result = task_result
            .unwrap_or_else(|err| Err(RepositoryError::internal(format!("Task failure: {err}"))));
        if result.is_ok() && item_result.is_err() {
            result = item_result;
        }
    }
    result
}

/// Enumerate every non-durable entry for this repository's partition in the
/// local immutable store.
async fn scan_non_durable(
    repository: &Arc<RepositoryContext>,
) -> Result<Vec<(Partition, Address)>, RepositoryError> {
    let store = repository.immutable_store();
    if !store.is_local() {
        return Err(RepositoryError::internal(
            "Push content failed: scan requires a local store",
        ));
    }
    // Same downcast as `verify_fragment_local`: the local store is the
    // concrete `LocalImmutableStore` behind the trait object.
    let concrete_store: Arc<crate::store::immutable::ImmutableStore> = (store
        as Arc<dyn std::any::Any + Send + Sync>)
        .downcast::<crate::store::immutable::ImmutableStore>()
        .map_err(|_not_local| {
            RepositoryError::internal("Push content failed: store is not a local ImmutableStore")
        })?;
    Ok(concrete_store
        .non_durable_entries(Some(repository.id))
        .await)
}

/// Parse a `hash` or `hash-context` address string (the format printed in
/// `Address not found` errors).
fn parse_address(text: &str) -> Result<Address, RepositoryError> {
    let (hash_text, context_text) = match text.split_once('-') {
        Some((hash, context)) => (hash, Some(context)),
        None => (text, None),
    };
    let hash = Hash::from_str(hash_text).internal("Invalid address")?;
    let context = match context_text {
        Some(context) => Context::from_str(context).internal("Invalid address")?,
        None => Context::default(),
    };
    Ok(Address { hash, context })
}

async fn push_item(
    repository: Arc<RepositoryContext>,
    id: u64,
    partition: Partition,
    address: Address,
    session: Arc<StorageSession>,
) -> Result<(), RepositoryError> {
    let store = repository.immutable_store();

    // Skip entries that are already durable (e.g. healed since the scan).
    let query = store
        .clone()
        .query(partition, address, StoreMatch::MatchFull)
        .await;
    if let Ok(qr) = &query
        && qr.match_made == StoreMatch::MatchFull
        && qr.fragment.flags & FragmentFlags::PayloadStoredDurable != 0
    {
        emit_complete(id, address, 1, event::LoreErrorCode::None);
        return Ok(());
    }

    // `no_remote()` is load-bearing: we must not pull from a third party to
    // satisfy a missing-local-payload push.
    let load = load_fragment(
        store.clone(),
        partition,
        address,
        ReadOptions::default().no_remote(),
        None,
    )
    .await;
    let (fragment, payload) = match load {
        Ok(pair) => pair,
        Err(err) => {
            emit_complete(id, address, 0, event::LoreErrorCode::AddressNotFound);
            return Err(RepositoryError::internal(format!(
                "Push content failed: No local payload for {address}: {err}"
            )));
        }
    };

    let permit = acquire_fragment_memory_permit(payload.len()).await;
    match store_fragment(
        store,
        partition,
        address,
        fragment,
        payload,
        true,
        Some(session),
        None,
        permit,
    )
    .await
    {
        Ok(_) => {
            emit_complete(id, address, 0, event::LoreErrorCode::None);
            Ok(())
        }
        Err(err) => {
            emit_complete(id, address, 0, event::LoreErrorCode::Internal);
            Err(RepositoryError::internal(format!(
                "Push content failed: Failed to upload {address}: {err}"
            )))
        }
    }
}

fn emit_complete(id: u64, address: Address, already_durable: u8, error_code: event::LoreErrorCode) {
    event::LoreEvent::StorageUploadItemComplete(LoreStorageUploadItemCompleteEventData {
        id,
        address,
        already_durable,
        error_code,
    })
    .send();
}
